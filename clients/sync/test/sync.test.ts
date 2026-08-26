import { strict as assert } from "node:assert";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import {
  PowDBSyncError,
  PowDBSyncReplica,
  SUPPORTED_CATALOG_VERSION,
  assertServerCatalogVersionSupported,
  type LocalApplyRequest,
  type LocalApplyResult,
  type LocalReplica,
  type QueryParam,
  type QueryResult,
  type RemoteSyncClient,
  type RetainedUnit,
  type SyncAckRequest,
  type SyncAckResult,
  type SyncIdentity,
  type SyncNowResult,
  type SyncPullRequest,
  type SyncPullResult,
  type SyncStatus,
} from "../src/index.js";

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

async function expectSyncError(
  fn: () => Promise<unknown>,
  code: PowDBSyncError["code"],
): Promise<PowDBSyncError> {
  try {
    await fn();
  } catch (err) {
    assert.ok(err instanceof PowDBSyncError, "expected PowDBSyncError");
    assert.equal(err.code, code);
    return err;
  }
  assert.fail(`expected PowDBSyncError with code ${code}`);
}

function timeoutAfter(ms: number, label: string): Promise<never> {
  return new Promise((_, reject) => {
    setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms);
  });
}

const identity: SyncIdentity = {
  databaseId: "00112233445566778899aabbccddeeff",
  primaryGeneration: 1n,
  walFormatVersion: 1,
  catalogVersion: 1,
  segmentFormatVersion: 1,
};

function syncStatus(overrides: Partial<SyncStatus> = {}): SyncStatus {
  return {
    replicaId: "replica-a",
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

function retainedUnit(lsn: bigint): RetainedUnit {
  return {
    txId: 0n,
    recordType: 1,
    lsn,
    data: new Uint8Array([Number(lsn & 0xffn)]),
  };
}

class MockRemote implements RemoteSyncClient {
  queryCalls: Array<{
    query: string;
    paramsOrOpts?: QueryParam[] | { signal?: AbortSignal };
    maybeOpts?: { signal?: AbortSignal };
  }> = [];
  statusCalls: string[] = [];
  pullRequests: SyncPullRequest[] = [];
  ackRequests: SyncAckRequest[] = [];
  statuses: SyncStatus[] = [];
  pulls: SyncPullResult[] = [];
  acks: SyncAckResult[] = [];
  queryResult: QueryResult = { kind: "ok", affected: 1n };
  queryError: unknown = null;
  log: string[];

  constructor(log: string[] = []) {
    this.log = log;
  }

  async query(
    query: string,
    paramsOrOpts?: QueryParam[] | { signal?: AbortSignal },
    maybeOpts?: { signal?: AbortSignal },
  ): Promise<QueryResult> {
    this.log.push("query");
    this.queryCalls.push({ query, paramsOrOpts, maybeOpts });
    if (this.queryError !== null) throw this.queryError;
    return this.queryResult;
  }

  async syncStatus(replicaId: string): Promise<SyncStatus> {
    this.log.push("status");
    this.statusCalls.push(replicaId);
    const status = this.statuses.shift();
    assert.ok(status, "unexpected syncStatus call");
    return status;
  }

  async syncPull(request: SyncPullRequest): Promise<SyncPullResult> {
    this.log.push("pull");
    this.pullRequests.push(request);
    const pull = this.pulls.shift();
    assert.ok(pull, "unexpected syncPull call");
    return pull;
  }

  async syncAck(request: SyncAckRequest): Promise<SyncAckResult> {
    this.log.push("ack");
    this.ackRequests.push(request);
    const ack = this.acks.shift();
    assert.ok(ack, "unexpected syncAck call");
    return ack;
  }
}

class MockLocal implements LocalReplica {
  readonlyQueries: string[] = [];
  applyRequests: LocalApplyRequest[] = [];
  readonlyResult: QueryResult = {
    kind: "rows",
    columns: ["id"],
    rows: [["1"]],
  };
  applyResult: LocalApplyResult = {};
  applyError: unknown = null;
  log: string[];

  constructor(log: string[] = []) {
    this.log = log;
  }

  queryReadonly(query: string): QueryResult {
    this.log.push("readonly");
    this.readonlyQueries.push(query);
    return this.readonlyResult;
  }

  async applyRetainedUnits(request: LocalApplyRequest): Promise<LocalApplyResult> {
    this.log.push("apply");
    this.applyRequests.push(request);
    if (this.applyError !== null) throw this.applyError;
    return this.applyResult;
  }
}

function replica(remote: RemoteSyncClient, local: LocalReplica): PowDBSyncReplica {
  return new PowDBSyncReplica({
    replicaId: "replica-a",
    identity,
    local,
    remote,
    maxPullUnits: 2,
    maxPullBytes: 1024n,
  });
}

async function main() {
  console.log("\n@zvndev/powdb-sync — embedded replica control loop");

  await test("SUPPORTED_CATALOG_VERSION matches the engine's CATALOG_VERSION", () => {
    // Drift gate: nothing else ties this package's exported ceiling to the
    // engine's `pub const CATALOG_VERSION`, and it sat at 6 for eight
    // releases after the engine moved to 7 (entity links, 0.19.0).
    const path = fileURLToPath(
      new URL("../../../crates/storage/src/catalog/mod.rs", import.meta.url),
    );
    const text = readFileSync(path, "utf8");
    const matches = [...text.matchAll(/^pub const CATALOG_VERSION: u16 = (\d+);$/gm)];
    assert.equal(
      matches.length,
      1,
      `expected exactly one \`pub const CATALOG_VERSION: u16 = N;\` in ${path}, found ${matches.length}`,
    );
    const engineCatalogVersion = Number(matches[0]![1]);
    assert.equal(
      SUPPORTED_CATALOG_VERSION,
      engineCatalogVersion,
      `SUPPORTED_CATALOG_VERSION (${SUPPORTED_CATALOG_VERSION}) has fallen behind the engine's CATALOG_VERSION (${engineCatalogVersion})`,
    );
  });

  await test("assertServerCatalogVersionSupported accepts a v7 primary and rejects v8", () => {
    // Behavior behind the gate: a primary whose database activated the
    // entity-links format (v7) is readable; the next format is not.
    assertServerCatalogVersionSupported(7);
    assertServerCatalogVersionSupported(6);
    assertServerCatalogVersionSupported(SUPPORTED_CATALOG_VERSION);
    assert.throws(
      () => assertServerCatalogVersionSupported(8),
      new Error(
        "server catalog format v8 is newer than this client supports (max v7); upgrade the client",
      ),
    );
    // An explicit client max is honored, and a nonsense version is refused.
    assertServerCatalogVersionSupported(5, 5);
    assert.throws(() => assertServerCatalogVersionSupported(6, 5), /upgrade the client/);
    assert.throws(() => assertServerCatalogVersionSupported(0), /invalid server catalog version/);
  });

  await test("queryReadonly delegates to the local embedded replica", async () => {
    const log: string[] = [];
    const remote = new MockRemote(log);
    const local = new MockLocal(log);
    const db = replica(remote, local);

    const result = await db.queryReadonly("User { .id }");

    assert.deepEqual(result, local.readonlyResult);
    assert.deepEqual(local.readonlyQueries, ["User { .id }"]);
    assert.deepEqual(log, ["readonly"]);
  });

  await test("constructor rejects adapters missing required sync capabilities", () => {
    assert.throws(
      () =>
        new PowDBSyncReplica({
          replicaId: "replica-a",
          identity,
          local: new MockLocal(),
          remote: {
            query: async () => ({ kind: "ok", affected: 0n }),
            syncStatus: async () => syncStatus(),
            syncPull: async () => ({
              status: syncStatus(),
              units: [],
              hasMore: false,
            }),
          } as unknown as RemoteSyncClient,
        }),
      (err) => err instanceof PowDBSyncError && err.code === "protocol_error",
    );
    assert.throws(
      () =>
        new PowDBSyncReplica({
          replicaId: "replica-a",
          identity,
          local: { queryReadonly: () => ({ kind: "ok", affected: 0n }) } as unknown as LocalReplica,
          remote: new MockRemote(),
        }),
      (err) => err instanceof PowDBSyncError && err.code === "protocol_error",
    );
  });

  await test("constructor snapshots byte-form database identity", async () => {
    const original = new Uint8Array([
      0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
      0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
    ]);
    const remote = new MockRemote();
    const local = new MockLocal();
    const db = new PowDBSyncReplica({
      replicaId: "replica-a",
      identity: { ...identity, databaseId: original },
      local,
      remote,
      maxPullUnits: 2,
      maxPullBytes: 1024n,
    });
    original[0] = 0xff;
    remote.statuses.push(
      syncStatus({
        lastAppliedLsn: 10n,
        remoteLsn: 11n,
        servableLsn: 11n,
        stale: true,
        repairAction: "pull",
      }),
    );
    remote.pulls.push({
      status: syncStatus({
        lastAppliedLsn: 10n,
        remoteLsn: 11n,
        servableLsn: 11n,
        stale: true,
        repairAction: "pull",
      }),
      units: [retainedUnit(11n)],
      hasMore: false,
    });
    remote.acks.push({
      previousAppliedLsn: 10n,
      appliedLsn: 11n,
      remoteLsn: 11n,
      advanced: true,
      status: syncStatus({
        lastAppliedLsn: 11n,
        remoteLsn: 11n,
        servableLsn: 11n,
        stale: false,
        repairAction: "none",
      }),
    });

    await db.syncNow();

    assert.ok(remote.pullRequests[0]?.databaseId instanceof Uint8Array);
    assert.equal((remote.pullRequests[0]?.databaseId as Uint8Array)[0], 0x00);
    assert.ok(local.applyRequests[0]?.databaseId instanceof Uint8Array);
    assert.equal((local.applyRequests[0]?.databaseId as Uint8Array)[0], 0x00);
  });

  await test("syncNow pulls, applies, then acks a contiguous retained chunk", async () => {
    const log: string[] = [];
    const remote = new MockRemote(log);
    const local = new MockLocal(log);
    const db = replica(remote, local);
    remote.statuses.push(
      syncStatus({
        lastAppliedLsn: 10n,
        remoteLsn: 12n,
        servableLsn: 12n,
        lagLsn: 2n,
        stale: true,
        repairAction: "pull",
      }),
    );
    remote.pulls.push({
      status: syncStatus({
        lastAppliedLsn: 10n,
        remoteLsn: 12n,
        servableLsn: 12n,
        stale: true,
        repairAction: "pull",
      }),
      units: [retainedUnit(11n), retainedUnit(12n)],
      hasMore: false,
    });
    remote.acks.push({
      previousAppliedLsn: 10n,
      appliedLsn: 12n,
      remoteLsn: 12n,
      advanced: true,
      status: syncStatus({
        lastAppliedLsn: 12n,
        remoteLsn: 12n,
        servableLsn: 12n,
        stale: false,
        repairAction: "none",
      }),
    });

    const result = await db.syncNow();

    assert.deepEqual(log, ["status", "pull", "apply", "ack"]);
    assert.equal(result.pulls, 1);
    assert.equal(result.units, 2);
    assert.equal(result.appliedLsn, 12n);
    assert.equal(result.stale, false);
    assert.equal(remote.pullRequests[0]?.sinceLsn, 10n);
    assert.equal(remote.pullRequests[0]?.maxUnits, 2);
    assert.equal(remote.pullRequests[0]?.maxBytes, 1024n);
    assert.equal(remote.pullRequests[0]?.databaseId, identity.databaseId);
    assert.equal(local.applyRequests[0]?.sinceLsn, 10n);
    assert.equal(local.applyRequests[0]?.units.length, 2);
    assert.equal(remote.ackRequests[0]?.appliedLsn, 12n);
    assert.equal(remote.ackRequests[0]?.remoteLsn, 12n);
  });

  await test("syncNow accepts ack status when primary advances after pull", async () => {
    const remote = new MockRemote();
    const local = new MockLocal();
    const db = replica(remote, local);
    remote.statuses.push(
      syncStatus({
        lastAppliedLsn: 10n,
        remoteLsn: 12n,
        servableLsn: 12n,
        stale: true,
        repairAction: "pull",
      }),
    );
    remote.pulls.push({
      status: syncStatus({
        lastAppliedLsn: 10n,
        remoteLsn: 12n,
        servableLsn: 12n,
        stale: true,
        repairAction: "pull",
      }),
      units: [retainedUnit(11n), retainedUnit(12n)],
      hasMore: false,
    });
    remote.acks.push({
      previousAppliedLsn: 10n,
      appliedLsn: 12n,
      remoteLsn: 13n,
      advanced: true,
      status: syncStatus({
        lastAppliedLsn: 12n,
        remoteLsn: 13n,
        servableLsn: 13n,
        lagLsn: 1n,
        stale: true,
        repairAction: "pull",
      }),
    });

    const result = await db.syncNow();

    assert.equal(result.appliedLsn, 12n);
    assert.equal(result.status.remoteLsn, 13n);
    assert.equal(result.stale, true);
    assert.equal(result.repairAction, "pull");
    assert.equal(remote.ackRequests[0]?.remoteLsn, 12n);
  });

  await test("syncNow rejects mismatched ack results after local apply", async () => {
    const remote = new MockRemote();
    const local = new MockLocal();
    const db = replica(remote, local);
    remote.statuses.push(
      syncStatus({
        lastAppliedLsn: 10n,
        remoteLsn: 12n,
        servableLsn: 12n,
        stale: true,
        repairAction: "pull",
      }),
    );
    remote.pulls.push({
      status: syncStatus({
        lastAppliedLsn: 10n,
        remoteLsn: 12n,
        servableLsn: 12n,
        stale: true,
        repairAction: "pull",
      }),
      units: [retainedUnit(11n), retainedUnit(12n)],
      hasMore: false,
    });
    remote.acks.push({
      previousAppliedLsn: 10n,
      appliedLsn: 11n,
      remoteLsn: 12n,
      advanced: true,
      status: syncStatus({
        lastAppliedLsn: 11n,
        remoteLsn: 12n,
        stale: true,
        repairAction: "pull",
      }),
    });

    const err = await expectSyncError(() => db.syncNow(), "protocol_error");
    assert.equal(err.localApplied, true);
    assert.equal(err.appliedLsn, 12n);
    assert.equal(err.remoteLsn, 12n);
    assert.equal(local.applyRequests.length, 1);
  });

  await test("syncNow rejects ack results behind the requested remote LSN", async () => {
    const remote = new MockRemote();
    const local = new MockLocal();
    const db = replica(remote, local);
    remote.statuses.push(
      syncStatus({
        lastAppliedLsn: 10n,
        remoteLsn: 12n,
        servableLsn: 12n,
        stale: true,
        repairAction: "pull",
      }),
    );
    remote.pulls.push({
      status: syncStatus({
        lastAppliedLsn: 10n,
        remoteLsn: 12n,
        servableLsn: 12n,
        stale: true,
        repairAction: "pull",
      }),
      units: [retainedUnit(11n), retainedUnit(12n)],
      hasMore: false,
    });
    remote.acks.push({
      previousAppliedLsn: 10n,
      appliedLsn: 12n,
      remoteLsn: 11n,
      advanced: true,
      status: syncStatus({
        lastAppliedLsn: 12n,
        remoteLsn: 11n,
        stale: false,
        repairAction: "none",
      }),
    });

    const err = await expectSyncError(() => db.syncNow(), "protocol_error");
    assert.match(err.message, /behind requested remote LSN/);
    assert.equal(err.localApplied, true);
    assert.equal(err.appliedLsn, 12n);
    assert.equal(err.remoteLsn, 12n);
  });

  await test("syncNow rejects non-contiguous pulls before local apply or ack", async () => {
    const remote = new MockRemote();
    const local = new MockLocal();
    const db = replica(remote, local);
    remote.statuses.push(
      syncStatus({
        lastAppliedLsn: 10n,
        remoteLsn: 12n,
        servableLsn: 12n,
        stale: true,
        repairAction: "pull",
      }),
    );
    remote.pulls.push({
      status: syncStatus({
        lastAppliedLsn: 10n,
        remoteLsn: 12n,
        servableLsn: 12n,
        stale: true,
        repairAction: "pull",
      }),
      units: [retainedUnit(12n)],
      hasMore: false,
    });

    await expectSyncError(() => db.syncNow(), "protocol_error");
    assert.equal(local.applyRequests.length, 0);
    assert.equal(remote.ackRequests.length, 0);
  });

  await test("syncNow returns stale when primary history is not archived yet", async () => {
    const remote = new MockRemote();
    const local = new MockLocal();
    const db = replica(remote, local);
    remote.statuses.push(
      syncStatus({
        lastAppliedLsn: 10n,
        remoteLsn: 12n,
        servableLsn: 10n,
        unarchivedLsn: 2n,
        stale: true,
        repairAction: "awaitArchive",
      }),
    );

    const result = await db.syncNow();

    assert.equal(result.pulls, 0);
    assert.equal(result.units, 0);
    assert.equal(result.stale, true);
    assert.equal(result.repairAction, "awaitArchive");
    assert.equal(remote.pullRequests.length, 0);
    assert.equal(local.applyRequests.length, 0);
  });

  await test("startBackgroundSync runs syncNow immediately and stops cleanly", async () => {
    const log: string[] = [];
    const remote = new MockRemote(log);
    const local = new MockLocal(log);
    const db = replica(remote, local);
    remote.statuses.push(syncStatus());

    const seen: SyncNowResult[] = [];
    let handle: ReturnType<PowDBSyncReplica["startBackgroundSync"]> | undefined;
    const finished = new Promise<void>((resolve, reject) => {
      handle = db.startBackgroundSync({
        intervalMs: 1000,
        onResult: (result) => {
          seen.push(result);
          handle?.stop();
          resolve();
        },
        onError: reject,
      });
    });

    await Promise.race([finished, timeoutAfter(1000, "background sync immediate result")]);
    assert.equal(handle?.isStopped(), true);
    assert.equal(handle?.isRunning(), false);
    assert.equal(seen.length, 1);
    assert.deepEqual(log, ["status"]);
  });

  await test("startBackgroundSync schedules repeated non-overlapping syncNow calls", async () => {
    const log: string[] = [];
    const remote = new MockRemote(log);
    const local = new MockLocal(log);
    const db = replica(remote, local);
    remote.statuses.push(syncStatus(), syncStatus());

    let results = 0;
    let handle: ReturnType<PowDBSyncReplica["startBackgroundSync"]> | undefined;
    const finished = new Promise<void>((resolve, reject) => {
      handle = db.startBackgroundSync({
        intervalMs: 10,
        immediate: false,
        onResult: () => {
          results++;
          if (results === 2) {
            handle?.stop();
            resolve();
          }
        },
        onError: reject,
      });
    });

    await Promise.race([finished, timeoutAfter(1000, "background sync interval results")]);
    assert.equal(handle?.isStopped(), true);
    assert.equal(results, 2);
    assert.deepEqual(log, ["status", "status"]);
  });

  await test("startBackgroundSync can stop after a background sync error", async () => {
    const remote = new MockRemote();
    const local = new MockLocal();
    const db = replica(remote, local);

    const errors: PowDBSyncError[] = [];
    let handle: ReturnType<PowDBSyncReplica["startBackgroundSync"]> | undefined;
    const finished = new Promise<void>((resolve) => {
      handle = db.startBackgroundSync({
        intervalMs: 10,
        stopOnError: true,
        onError: (err) => {
          errors.push(err);
          resolve();
        },
      });
    });

    await Promise.race([finished, timeoutAfter(1000, "background sync error")]);
    await new Promise((resolve) => setTimeout(resolve, 0));
    assert.equal(handle?.isStopped(), true);
    assert.equal(errors.length, 1);
    assert.equal(errors[0]?.code, "remote_unavailable");
  });

  await test("syncNow requires rebootstrap for inactive or rebootstrap status", async () => {
    const remote = new MockRemote();
    const local = new MockLocal();
    const db = replica(remote, local);
    remote.statuses.push(
      syncStatus({
        active: false,
        lastAppliedLsn: 10n,
        remoteLsn: 12n,
        stale: true,
        repairAction: "rebootstrap",
        lastSyncError: "retained history has been pruned",
      }),
    );

    const err = await expectSyncError(() => db.syncNow(), "rebootstrap_required");
    assert.match(err.message, /pruned/);
    assert.equal(remote.pullRequests.length, 0);
    assert.equal(local.applyRequests.length, 0);
  });

  await test("write rejects DDL before remote execution", async () => {
    const remote = new MockRemote();
    const local = new MockLocal();
    const db = replica(remote, local);

    await expectSyncError(
      () => db.write("-- migrate\n type User { id: int }"),
      "ddl_not_supported",
    );
    assert.equal(remote.queryCalls.length, 0);
  });

  await test("write maps primary connection failures to remote_unavailable", async () => {
    const remote = new MockRemote();
    const local = new MockLocal();
    const db = replica(remote, local);
    remote.queryError = Object.assign(new Error("connection refused"), {
      code: "connect_failed",
    });

    await expectSyncError(
      () => db.write("insert User { id := 1 }"),
      "remote_unavailable",
    );
    assert.equal(remote.statusCalls.length, 0);
  });

  await test("write reports unknown commit outcome for ambiguous failures", async () => {
    const remote = new MockRemote();
    const local = new MockLocal();
    const db = replica(remote, local);
    remote.queryError = new Error("socket closed after request write");

    const err = await expectSyncError(
      () => db.write("insert User { id := 1 }"),
      "commit_outcome_unknown",
    );
    assert.match(err.message, /do not retry blindly/);
  });

  await test("write with deferred sync does not read local status", async () => {
    const remote = new MockRemote();
    const local = new MockLocal();
    const db = replica(remote, local);

    const result = await db.write("insert User { id := 1 }", { sync: "defer" });

    assert.deepEqual(result, {
      result: { kind: "ok", affected: 1n },
      localVisible: false,
      localVisibility: "not_guaranteed",
    });
    assert.equal(remote.queryCalls.length, 1);
    assert.equal(remote.statusCalls.length, 0);
  });

  await test("write reports applied-but-unacked visibility after ack failure", async () => {
    const remote = new MockRemote();
    const local = new MockLocal();
    const db = replica(remote, local);
    remote.statuses.push(
      syncStatus({
        lastAppliedLsn: 10n,
        remoteLsn: 12n,
        servableLsn: 12n,
        stale: true,
        repairAction: "pull",
      }),
      syncStatus({
        lastAppliedLsn: 10n,
        remoteLsn: 12n,
        servableLsn: 12n,
        stale: true,
        repairAction: "pull",
      }),
    );
    remote.pulls.push({
      status: syncStatus({
        lastAppliedLsn: 10n,
        remoteLsn: 12n,
        servableLsn: 12n,
        stale: true,
        repairAction: "pull",
      }),
      units: [retainedUnit(11n), retainedUnit(12n)],
      hasMore: false,
    });
    remote.syncAck = async (request: SyncAckRequest): Promise<SyncAckResult> => {
      remote.ackRequests.push(request);
      throw new Error("ack socket closed");
    };

    const result = await db.write("insert User { id := 1 }");

    assert.equal(result.result.kind, "ok");
    assert.equal(result.localVisible, true);
    assert.equal(result.localVisibility, "applied_but_unacked");
    assert.equal(result.syncAppliedLsn, 12n);
    assert.equal(result.syncRemoteLsn, 12n);
    assert.equal(result.syncError?.code, "ack_failed");
    assert.equal(local.applyRequests.length, 1);
    assert.equal(remote.ackRequests.length, 1);
  });

  await test("write returns remote success with syncError when local catch-up fails", async () => {
    const remote = new MockRemote();
    const local = new MockLocal();
    const db = replica(remote, local);
    remote.statuses.push(
      syncStatus({
        active: false,
        lastAppliedLsn: 10n,
        remoteLsn: 12n,
        stale: true,
        repairAction: "rebootstrap",
        lastSyncError: "replica cursor is inactive",
      }),
      syncStatus({
        active: false,
        lastAppliedLsn: 10n,
        remoteLsn: 12n,
        stale: true,
        repairAction: "rebootstrap",
        lastSyncError: "replica cursor is inactive",
      }),
    );

    const result = await db.write("insert User { id := 1 }");

    assert.equal(result.result.kind, "ok");
    assert.equal(result.localVisible, false);
    assert.equal(result.localVisibility, "not_guaranteed");
    assert.equal(result.status?.repairAction, "rebootstrap");
    assert.equal(result.syncError?.code, "rebootstrap_required");
    assert.equal(remote.queryCalls.length, 1);
    assert.equal(remote.statusCalls.length, 2);
  });
}

await main();

if (failed > 0) {
  console.error(`\n${failed} failed, ${passed} passed`);
  for (const failure of failures) console.error(`- ${failure}`);
  process.exit(1);
}

console.log(`\n${passed} passed`);
