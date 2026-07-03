/**
 * Live embedded-sync wire test against a real powdb-server.
 *
 * This covers the JavaScript client helpers over the real Rust server instead
 * of the protocol mock in protocol.test.ts. The server is restarted after the
 * post-bootstrap write so Engine::Drop performs the sync-aware checkpoint that
 * archives retained WAL units before the pull.
 */

import * as fsp from "node:fs/promises";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { spawn, type ChildProcess } from "node:child_process";
import { fileURLToPath } from "node:url";
import { strict as assert } from "node:assert";
import { Client, type QueryResult } from "../src/index.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const clientRoot = path.resolve(here, "..");
const repoRoot = path.resolve(clientRoot, "..", "..");
const HOST = "127.0.0.1";
const PASSWORD = "sync-live-secret";
const REPLICA_ID = "replica-js-live";
const DATABASE_ID = Buffer.from("ts-sync-live-001", "utf8");
const DATABASE_ID_HEX = DATABASE_ID.toString("hex");
const PRIMARY_GENERATION = 1n;
const WAL_FORMAT_VERSION = 1;
const CATALOG_VERSION = 5;
const SEGMENT_FORMAT_VERSION = 1;

assert.equal(DATABASE_ID.length, 16);

let passed = 0;
let failed = 0;
const failures: string[] = [];

function bin(name: string): { cmd: string; prefix: string[] } {
  const override = process.env.POWDB_SERVER_BIN;
  if (override !== undefined && override.trim().length > 0) {
    return { cmd: override, prefix: [] };
  }
  return {
    cmd: "cargo",
    prefix: ["run", "--release", "-p", name, "--"],
  };
}

async function test(name: string, fn: () => Promise<void>) {
  try {
    await fn();
    passed++;
    console.log(`  ✓ ${name}`);
  } catch (err) {
    failed++;
    const message = err instanceof Error ? err.message : String(err);
    failures.push(`${name}: ${message}`);
    console.log(`  ✗ ${name}`);
    console.log(`    ${message}`);
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

function assertMessage(result: QueryResult): void {
  assert.equal(result.kind, "message", `expected message, got ${result.kind}`);
}

function assertOk(result: QueryResult, affected: number): void {
  assert.equal(result.kind, "ok", `expected ok, got ${result.kind}`);
  if (result.kind === "ok") {
    assert.equal(Number(result.affected), affected);
  }
}

function lsnToSafeNumber(lsn: bigint): number {
  assert.ok(lsn <= BigInt(Number.MAX_SAFE_INTEGER), `test LSN too large: ${lsn}`);
  return Number(lsn);
}

async function writeSyncMetadata(dataDir: string, appliedLsn: bigint): Promise<void> {
  const stateDir = path.join(dataDir, ".powdb-sync");
  await fsp.mkdir(stateDir, { recursive: true });
  const now = Math.floor(Date.now() / 1000);
  await fsp.writeFile(
    path.join(stateDir, "identity.json"),
    JSON.stringify(
      {
        format_version: 1,
        database_id: DATABASE_ID_HEX,
        primary_generation: lsnToSafeNumber(PRIMARY_GENERATION),
        created_unix_secs: now,
      },
      null,
      2,
    ),
  );
  await fsp.writeFile(
    path.join(stateDir, "replica-cursors.json"),
    JSON.stringify(
      {
        format_version: 1,
        cursors: [
          {
            replica_id: REPLICA_ID,
            applied_lsn: lsnToSafeNumber(appliedLsn),
            updated_unix_secs: now,
            active: true,
          },
        ],
      },
      null,
      2,
    ),
  );
}

async function connect(port: number): Promise<Client> {
  return await Client.connect({
    host: HOST,
    port,
    password: PASSWORD,
    connectTimeoutMs: 10_000,
  });
}

async function main() {
  console.log("\nLive embedded sync client/server");
  const dataDir = await fsp.mkdtemp(path.join(os.tmpdir(), "powdb-ts-sync-live-"));
  const port = await getFreePort();
  let server: ChildProcess | undefined;

  try {
    await test("status/pull/ack round-trip against real powdb-server", async () => {
      server = await startServer(port, dataDir);
      let client = await connect(port);

      assertMessage(
        await client.query("type LiveSync { required id: int, synced: bool }"),
      );
      await writeSyncMetadata(dataDir, 0n);

      const schemaStatus = await client.syncStatus(REPLICA_ID);
      assert.ok(schemaStatus.remoteLsn > 0n, "schema DDL should advance remote LSN");
      const baselineLsn = schemaStatus.remoteLsn;
      await writeSyncMetadata(dataDir, baselineLsn);

      const baselineStatus = await client.syncStatus(REPLICA_ID);
      assert.equal(baselineStatus.lastAppliedLsn, baselineLsn);
      assert.equal(baselineStatus.repairAction, "none");

      assertOk(
        await client.query("insert LiveSync { id := 1, synced := true }"),
        1,
      );
      const beforeArchive = await client.syncStatus(REPLICA_ID);
      assert.ok(beforeArchive.remoteLsn > baselineLsn, "insert should advance LSN");
      assert.equal(beforeArchive.lastAppliedLsn, baselineLsn);
      assert.equal(beforeArchive.repairAction, "awaitArchive");
      const remoteAfterWrite = beforeArchive.remoteLsn;

      await client.close();
      await stopServer(server);
      server = undefined;

      server = await startServer(port, dataDir);
      client = await connect(port);
      const ready = await client.syncStatus(REPLICA_ID);
      assert.equal(ready.lastAppliedLsn, baselineLsn);
      assert.equal(ready.remoteLsn, remoteAfterWrite);
      assert.equal(ready.repairAction, "pull");
      assert.ok(
        ready.servableLsn !== null && ready.servableLsn >= remoteAfterWrite,
        "restart checkpoint should archive the post-bootstrap write",
      );

      const pull = await client.syncPull({
        replicaId: REPLICA_ID,
        sinceLsn: baselineLsn,
        maxUnits: 128,
        maxBytes: 1024n * 1024n,
        databaseId: DATABASE_ID_HEX,
        primaryGeneration: PRIMARY_GENERATION,
        walFormatVersion: WAL_FORMAT_VERSION,
        catalogVersion: CATALOG_VERSION,
        segmentFormatVersion: SEGMENT_FORMAT_VERSION,
      });
      assert.equal(pull.status.repairAction, "pull");
      assert.ok(pull.units.length > 0, "pull should return retained units");
      assert.equal(pull.units[pull.units.length - 1]?.lsn, remoteAfterWrite);
      assert.equal(pull.hasMore, false);

      const ack = await client.syncAck({
        replicaId: REPLICA_ID,
        appliedLsn: remoteAfterWrite,
        remoteLsn: remoteAfterWrite,
      });
      assert.equal(ack.previousAppliedLsn, baselineLsn);
      assert.equal(ack.appliedLsn, remoteAfterWrite);
      assert.equal(ack.advanced, true);
      assert.equal(ack.status.repairAction, "none");

      await client.close();
    });
  } finally {
    if (server) await stopServer(server);
    await fsp.rm(dataDir, { recursive: true, force: true });
  }

  console.log(`\n${passed} passed, ${failed} failed`);
  if (failed > 0) {
    for (const failure of failures) console.log(`  - ${failure}`);
    process.exitCode = 1;
  }
}

main().catch((err) => {
  console.error(err);
  process.exitCode = 1;
});
