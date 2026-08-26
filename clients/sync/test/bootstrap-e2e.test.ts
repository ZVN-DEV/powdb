import { strict as assert } from "node:assert";
import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { createRequire } from "node:module";
import * as fs from "node:fs";
import * as fsp from "node:fs/promises";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { Client, type QueryResult } from "../../ts/src/index.js";
import {
  PowDBSyncReplica,
  SUPPORTED_CATALOG_VERSION,
  type SyncIdentity,
} from "../src/index.js";

type EmbeddedDatabase = {
  queryReadonly(query: string): QueryResult;
  applyRetainedUnits(request: unknown): { throughLsn: bigint; unitsApplied: number };
};

type IdentitySnapshot = {
  database_id: string;
  primary_generation: number;
};

const require = createRequire(import.meta.url);
const here = path.dirname(fileURLToPath(import.meta.url));
const syncRoot = path.resolve(here, "..");
const repoRoot = path.resolve(syncRoot, "..", "..");
const HOST = "127.0.0.1";
const PASSWORD = "sync-bootstrap-e2e-secret";
const REPLICA_ID = "js-bootstrap-e2e";
const WAL_FORMAT_VERSION = 1;
// A replica states the newest catalog format it can read, not the format the
// primary happens to be on: the primary accepts any ceiling at or above its
// active format, so this also covers a primary that has activated v7.
const CATALOG_VERSION = SUPPORTED_CATALOG_VERSION;
const SEGMENT_FORMAT_VERSION = 1;
const EMBEDDED_PACKAGE = "@zvndev/powdb-embedded";
const REPO_LOCAL_EMBEDDED_ENTRY = "../../../bindings/node/index.js";

let passed = 0;
let failed = 0;
const failures: string[] = [];

function isMissingEmbeddedPackage(err: unknown): boolean {
  return (
    err instanceof Error &&
    "code" in err &&
    err.code === "MODULE_NOT_FOUND" &&
    err.message.includes(`Cannot find module '${EMBEDDED_PACKAGE}'`)
  );
}

function resolveOptionalEmbeddedPackage(): string | null {
  try {
    return require.resolve(EMBEDDED_PACKAGE);
  } catch (err) {
    if (isMissingEmbeddedPackage(err)) return null;
    throw err;
  }
}

function loadEmbeddedDatabase(): { open(dir: string): EmbeddedDatabase } {
  const override = process.env.POWDB_SYNC_NATIVE_EMBEDDED_ENTRY;
  if (override !== undefined && override.length > 0) {
    return (require(override) as { Database: { open(dir: string): EmbeddedDatabase } }).Database;
  }
  const packageEntry = resolveOptionalEmbeddedPackage();
  if (packageEntry !== null) {
    return (require(packageEntry) as { Database: { open(dir: string): EmbeddedDatabase } })
      .Database;
  }
  return (require(REPO_LOCAL_EMBEDDED_ENTRY) as {
    Database: { open(dir: string): EmbeddedDatabase };
  }).Database;
}

const Database = loadEmbeddedDatabase();

async function test(name: string, fn: () => Promise<void> | void) {
  try {
    await fn();
    passed++;
    console.log(`  ✓ ${name}`);
  } catch (err) {
    failed++;
    const msg = err instanceof Error ? err.stack ?? err.message : String(err);
    failures.push(`${name}: ${msg}`);
    console.log(`  ✗ ${name}`);
    console.log(`    ${msg}`);
  }
}

function bin(name: "powdb-cli" | "powdb-server"): { cmd: string; prefix: string[] } {
  const override =
    name === "powdb-cli" ? process.env.POWDB_CLI_BIN : process.env.POWDB_SERVER_BIN;
  if (override !== undefined && override.trim().length > 0) {
    return { cmd: override, prefix: [] };
  }
  return {
    cmd: "cargo",
    prefix: ["run", "--release", "-p", name, "--"],
  };
}

function runCli(args: string[]): void {
  const { cmd, prefix } = bin("powdb-cli");
  const result = spawnSync(cmd, [...prefix, ...args], {
    cwd: repoRoot,
    encoding: "utf8",
  });
  if (result.status !== 0) {
    throw new Error(
      `powdb-cli ${args.join(" ")} failed (${result.status}):\n${result.stderr}\n${result.stdout}`,
    );
  }
}

async function getFreePort(): Promise<number> {
  return await new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, HOST, () => {
      const addr = server.address();
      if (!addr || typeof addr === "string") {
        server.close();
        reject(new Error("failed to allocate TCP port"));
        return;
      }
      const port = addr.port;
      server.close(() => resolve(port));
    });
  });
}

function canConnect(port: number): Promise<boolean> {
  return new Promise((resolve) => {
    const socket = new net.Socket();
    let done = false;
    const finish = (ok: boolean) => {
      if (done) return;
      done = true;
      socket.destroy();
      resolve(ok);
    };
    socket.setTimeout(250);
    socket.once("connect", () => finish(true));
    socket.once("timeout", () => finish(false));
    socket.once("error", () => finish(false));
    socket.connect(port, HOST);
  });
}

async function startServer(port: number, dataDir: string): Promise<ChildProcess> {
  const { cmd, prefix } = bin("powdb-server");
  const child = spawn(
    cmd,
    [...prefix, "--bind", HOST, "--port", String(port), "--data-dir", dataDir],
    {
      cwd: repoRoot,
      env: { ...process.env, POWDB_PASSWORD: PASSWORD },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  const log: string[] = [];
  const capture = (chunk: Buffer) => {
    for (const line of chunk.toString("utf8").split(/\r?\n/)) {
      if (line.trim().length === 0) continue;
      log.push(line);
      if (log.length > 80) log.shift();
    }
  };
  child.stdout?.on("data", capture);
  child.stderr?.on("data", capture);

  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      throw new Error(
        `powdb-server exited early with code ${child.exitCode}\n${log.join("\n")}`,
      );
    }
    if (await canConnect(port)) return child;
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
  child.kill("SIGKILL");
  throw new Error(`timed out waiting for powdb-server on ${HOST}:${port}\n${log.join("\n")}`);
}

async function stopServer(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null) return;
  child.kill("SIGINT");
  await new Promise<void>((resolve) => {
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      resolve();
    }, 10_000);
    child.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
  });
}

async function connect(port: number): Promise<Client> {
  return await Client.connect({
    host: HOST,
    port,
    password: PASSWORD,
    connectTimeoutMs: 10_000,
  });
}

function freshRoot(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "powdb-sync-bootstrap-e2e-"));
}

function readIdentity(dataDir: string): SyncIdentity {
  const raw = fs.readFileSync(path.join(dataDir, ".powdb-sync", "identity.json"), "utf8");
  const snapshot = JSON.parse(raw) as IdentitySnapshot;
  return {
    databaseId: snapshot.database_id,
    primaryGeneration: BigInt(snapshot.primary_generation),
    walFormatVersion: WAL_FORMAT_VERSION,
    catalogVersion: CATALOG_VERSION,
    segmentFormatVersion: SEGMENT_FORMAT_VERSION,
  };
}

function assertRows(
  result: QueryResult,
  expectedColumns: string[],
  expectedRows: string[][],
): void {
  assert.equal(result.kind, "rows", `expected rows, got ${result.kind}`);
  if (result.kind !== "rows") return;
  assert.deepEqual(result.columns, expectedColumns);
  assert.deepEqual(result.rows, expectedRows);
}

async function main() {
  console.log("\n@zvndev/powdb-sync — backup bootstrap/native/server e2e");

  await test("backup bootstrap replica catches up through native retained apply", async () => {
    const root = freshRoot();
    const primaryDir = path.join(root, "primary");
    const backupDir = path.join(root, "backup");
    const replicaDir = path.join(root, "replica");
    const port = await getFreePort();
    let server: ChildProcess | undefined;
    let remote: Client | undefined;

    try {
      runCli(["--data-dir", primaryDir, "-c", "type LiveSync { required id: int, synced: bool }"]);
      runCli(["--data-dir", primaryDir, "-c", "insert LiveSync { id := 1, synced := false }"]);
      runCli(["--data-dir", primaryDir, "sync-enable"]);
      runCli(["--data-dir", primaryDir, "backup", backupDir]);

      server = await startServer(port, primaryDir);
      remote = await connect(port);
      const write = await remote.query("insert LiveSync { id := 2, synced := true }");
      assert.equal(write.kind, "ok");
      await remote.close();
      remote = undefined;
      await stopServer(server);
      server = undefined;

      runCli(["--data-dir", primaryDir, "sync-bootstrap", backupDir, replicaDir, REPLICA_ID]);

      const local = Database.open(replicaDir);
      assertRows(
        local.queryReadonly("LiveSync order .id { .id, .synced }"),
        ["id", "synced"],
        [["1", "false"]],
      );

      server = await startServer(port, primaryDir);
      remote = await connect(port);
      const replica = new PowDBSyncReplica({
        replicaId: REPLICA_ID,
        identity: readIdentity(replicaDir),
        local: {
          queryReadonly: (query) => local.queryReadonly(query),
          applyRetainedUnits: (request) => local.applyRetainedUnits(request),
        },
        remote,
        maxPullUnits: 128,
        maxPullBytes: 1024n * 1024n,
      });

      const before = await replica.status();
      assert.equal(before.repairAction, "pull");
      assert.equal(before.stale, true);

      const synced = await replica.syncNow();
      assert.equal(synced.stale, false);
      assert.equal(synced.repairAction, "none");
      assert.ok(synced.units > 0, "sync should apply retained units from the primary");

      assertRows(
        await replica.queryReadonly("LiveSync order .id { .id, .synced }"),
        ["id", "synced"],
        [
          ["1", "false"],
          ["2", "true"],
        ],
      );
    } finally {
      if (remote !== undefined) await remote.close().catch(() => {});
      if (server !== undefined) await stopServer(server);
      await fsp.rm(root, { recursive: true, force: true });
    }
  });
}

await main();

if (failed > 0) {
  console.error(`\n${failed} failed, ${passed} passed`);
  for (const failure of failures) console.error(`- ${failure}`);
  process.exit(1);
}

console.log(`\n${passed} passed`);
