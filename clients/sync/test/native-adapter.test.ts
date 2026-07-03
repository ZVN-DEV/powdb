import { strict as assert } from "node:assert";
import { createRequire } from "node:module";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  PowDBSyncReplica,
  type QueryParam,
  type QueryResult,
  type RemoteSyncClient,
  type SyncAckRequest,
  type SyncAckResult,
  type SyncIdentity,
  type SyncPullRequest,
  type SyncPullResult,
  type SyncStatus,
} from "../src/index.js";

type EmbeddedDatabase = {
  queryReadonly(query: string): QueryResult;
  applyRetainedUnits(request: unknown): { throughLsn: bigint; unitsApplied: number };
};

const require = createRequire(import.meta.url);
const EMBEDDED_PACKAGE = "@zvndev/powdb-embedded";
const REPO_LOCAL_EMBEDDED_ENTRY = "../../../bindings/node/index.js";

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
    if (isMissingEmbeddedPackage(err)) {
      return null;
    }
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

let passed = 0;
let failed = 0;
const failures: string[] = [];

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

function freshDir(): string {
  return mkdtempSync(join(tmpdir(), "powdb-sync-native-test-"));
}

function bytesFromHex(hex: string): Uint8Array {
  const pairs = hex.match(/../g);
  assert.ok(pairs, "expected even-length hex");
  return Uint8Array.from(pairs.map((byte) => Number.parseInt(byte, 16)));
}

function seedApplyBoundary(dir: string, databaseIdHex: string, generation = 1, lsn = 0): void {
  const syncDir = join(dir, ".powdb-sync");
  mkdirSync(syncDir, { recursive: true });
  writeFileSync(
    join(syncDir, "identity.json"),
    JSON.stringify({
      format_version: 1,
      database_id: databaseIdHex,
      primary_generation: generation,
      created_unix_secs: 1,
    }),
  );
  writeFileSync(
    join(syncDir, "apply-state.json"),
    JSON.stringify({
      format_version: 1,
      database_id: Array.from(bytesFromHex(databaseIdHex)),
      primary_generation: generation,
      wal_format_version: 1,
      catalog_version: 5,
      from_lsn: lsn,
      through_lsn: lsn,
      applied_lsn: lsn,
      status: "complete",
      started_unix_secs: 1,
      updated_unix_secs: 1,
    }),
  );
}

function readApplyState(dir: string): Record<string, unknown> {
  const raw = readFileSync(join(dir, ".powdb-sync", "apply-state.json"), "utf8");
  return JSON.parse(raw) as Record<string, unknown>;
}

function syncStatus(overrides: Partial<SyncStatus> = {}): SyncStatus {
  return {
    replicaId: "native-replica",
    active: true,
    lastAppliedLsn: 0n,
    remoteLsn: 0n,
    servableLsn: 0n,
    unarchivedLsn: 0n,
    lagLsn: 0n,
    lagBytes: 0n,
    lagMs: 0n,
    stale: false,
    repairAction: "none",
    lastSyncError: null,
    ...overrides,
  };
}

class ScriptedRemote implements RemoteSyncClient {
  readonly log: string[] = [];
  readonly queries: string[] = [];
  readonly queryParams: QueryParam[][] = [];
  readonly pullRequests: SyncPullRequest[] = [];
  readonly ackRequests: SyncAckRequest[] = [];

  constructor(private readonly identity: SyncIdentity) {}

  async query(
    query: string,
    paramsOrOpts?: QueryParam[] | { signal?: AbortSignal },
  ): Promise<QueryResult> {
    this.log.push("query");
    this.queries.push(query);
    if (Array.isArray(paramsOrOpts)) {
      this.queryParams.push(paramsOrOpts);
    }
    return { kind: "ok", affected: 1n };
  }

  async syncStatus(replicaId: string): Promise<SyncStatus> {
    this.log.push("status");
    assert.equal(replicaId, "native-replica");
    return syncStatus({
      lastAppliedLsn: 0n,
      remoteLsn: 1n,
      servableLsn: 1n,
      stale: true,
      repairAction: "pull",
    });
  }

  async syncPull(request: SyncPullRequest): Promise<SyncPullResult> {
    this.log.push("pull");
    this.pullRequests.push(request);
    assert.equal(request.replicaId, "native-replica");
    assert.equal(request.sinceLsn, 0n);
    assert.deepEqual(
      Array.from(request.databaseId as Uint8Array),
      Array.from(this.identity.databaseId as Uint8Array),
    );
    return {
      status: syncStatus({
        lastAppliedLsn: 0n,
        remoteLsn: 1n,
        servableLsn: 1n,
        stale: true,
        repairAction: "pull",
      }),
      units: [
        {
          txId: 0n,
          recordType: 4,
          lsn: 1n,
          data: new Uint8Array(),
        },
      ],
      hasMore: false,
    };
  }

  async syncAck(request: SyncAckRequest): Promise<SyncAckResult> {
    this.log.push("ack");
    this.ackRequests.push(request);
    assert.equal(request.replicaId, "native-replica");
    assert.equal(request.appliedLsn, 1n);
    assert.equal(request.remoteLsn, 1n);
    return {
      previousAppliedLsn: 0n,
      appliedLsn: 1n,
      remoteLsn: 1n,
      advanced: true,
      status: syncStatus({
        lastAppliedLsn: 1n,
        remoteLsn: 1n,
        servableLsn: 1n,
        stale: false,
        repairAction: "none",
      }),
    };
  }
}

async function main() {
  console.log("\n@zvndev/powdb-sync — native embedded adapter integration");

  await test("write forwards remotely, applies locally through native addon, then acks", async () => {
    const dir = freshDir();
    try {
      const databaseIdHex = "101112131415161718191a1b1c1d1e1f";
      seedApplyBoundary(dir, databaseIdHex);
      const localDb = Database.open(dir);
      const identity: SyncIdentity = {
        databaseId: bytesFromHex(databaseIdHex),
        primaryGeneration: 1n,
        walFormatVersion: 1,
        catalogVersion: 5,
        segmentFormatVersion: 1,
      };
      const remote = new ScriptedRemote(identity);
      const replica = new PowDBSyncReplica({
        replicaId: "native-replica",
        identity,
        local: {
          queryReadonly: (query) => localDb.queryReadonly(query),
          applyRetainedUnits: (request) => localDb.applyRetainedUnits(request),
        },
        remote,
        maxPullUnits: 4,
        maxPullBytes: 1024n,
      });

      const result = await replica.write("User filter .id = $1 update { synced := true }", [1n]);

      assert.deepEqual(remote.log, ["query", "status", "pull", "ack"]);
      assert.deepEqual(remote.queries, ["User filter .id = $1 update { synced := true }"]);
      assert.deepEqual(remote.queryParams, [[1n]]);
      assert.equal(remote.pullRequests.length, 1);
      assert.equal(remote.ackRequests.length, 1);
      assert.equal(result.result.kind, "ok");
      assert.equal(result.localVisible, true);
      assert.equal(result.localVisibility, "visible");
      assert.equal(result.sync?.appliedLsn, 1n);
      assert.equal(result.sync?.units, 1);

      const state = readApplyState(dir);
      assert.equal(state.status, "complete");
      assert.equal(state.from_lsn, 0);
      assert.equal(state.through_lsn, 1);
      assert.equal(state.applied_lsn, 1);
    } finally {
      rmSync(dir, { recursive: true, force: true });
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
