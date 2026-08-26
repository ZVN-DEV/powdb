export type QueryResult =
  | { kind: "rows"; columns: string[]; rows: string[][] }
  | { kind: "scalar"; value: string }
  | { kind: "ok"; affected: bigint }
  | { kind: "message"; message: string };

export type QueryParam = string | number | bigint | boolean | null;
export type SyncU64 = bigint | number;
export type SyncRepairAction = "none" | "pull" | "awaitArchive" | "rebootstrap";

/**
 * The maximum catalog format version this client can read. This is the
 * `catalogVersion` a replica should state in its identity: the primary accepts
 * any replica whose maximum is at least the database's active catalog format,
 * and rejects an older replica. Tracks the engine's `CATALOG_VERSION`
 * (crates/storage/src/catalog/mod.rs); `test/sync.test.ts` fails when the two
 * disagree.
 */
export const SUPPORTED_CATALOG_VERSION = 7;

/**
 * Throw when a server-reported catalog format is newer than this client can
 * read. Accepts `serverCatalogVersion <= clientMax`; rejects a newer server,
 * which requires upgrading the client.
 */
export function assertServerCatalogVersionSupported(
  serverCatalogVersion: number,
  clientMax: number = SUPPORTED_CATALOG_VERSION,
): void {
  if (!Number.isInteger(serverCatalogVersion) || serverCatalogVersion < 1) {
    throw new Error(`invalid server catalog version ${serverCatalogVersion}`);
  }
  if (serverCatalogVersion > clientMax) {
    throw new Error(
      `server catalog format v${serverCatalogVersion} is newer than this client supports (max v${clientMax}); upgrade the client`,
    );
  }
}

export interface SyncStatus {
  replicaId: string;
  active: boolean;
  lastAppliedLsn: bigint | null;
  remoteLsn: bigint;
  servableLsn: bigint | null;
  unarchivedLsn: bigint | null;
  lagLsn: bigint | null;
  lagBytes: bigint | null;
  lagMs: bigint | null;
  stale: boolean;
  repairAction: SyncRepairAction;
  lastSyncError: string | null;
}

export interface RetainedUnit {
  txId: bigint;
  recordType: number;
  lsn: bigint;
  data: Uint8Array;
}

export interface SyncIdentity {
  databaseId: string | Uint8Array;
  primaryGeneration: SyncU64;
  walFormatVersion: number;
  catalogVersion: number;
  segmentFormatVersion: number;
}

export interface SyncPullRequest extends SyncIdentity {
  replicaId: string;
  sinceLsn: SyncU64;
  maxUnits: number;
  maxBytes: SyncU64;
}

export interface SyncPullResult {
  status: SyncStatus;
  units: RetainedUnit[];
  hasMore: boolean;
}

export interface SyncAckRequest {
  replicaId: string;
  appliedLsn: SyncU64;
  remoteLsn: SyncU64;
}

export interface SyncAckResult {
  previousAppliedLsn: bigint;
  appliedLsn: bigint;
  remoteLsn: bigint;
  advanced: boolean;
  status: SyncStatus;
}

export interface RemoteSyncClient {
  query(
    query: string,
    paramsOrOpts?: QueryParam[] | { signal?: AbortSignal },
    maybeOpts?: { signal?: AbortSignal },
  ): Promise<QueryResult>;
  syncStatus(replicaId: string, opts?: { signal?: AbortSignal }): Promise<SyncStatus>;
  syncPull(request: SyncPullRequest, opts?: { signal?: AbortSignal }): Promise<SyncPullResult>;
  syncAck(request: SyncAckRequest, opts?: { signal?: AbortSignal }): Promise<SyncAckResult>;
}

export interface LocalApplyRequest extends SyncIdentity {
  replicaId: string;
  sinceLsn: bigint;
  units: RetainedUnit[];
}

export interface LocalApplyResult {
  throughLsn?: SyncU64;
  unitsApplied?: number;
}

export interface LocalReplica {
  queryReadonly(query: string, opts?: { signal?: AbortSignal }): QueryResult | Promise<QueryResult>;
  applyRetainedUnits(request: LocalApplyRequest): LocalApplyResult | Promise<LocalApplyResult>;
}

export type PowDBSyncErrorCode =
  | "ddl_not_supported"
  | "remote_unavailable"
  | "remote_write_failed"
  | "commit_outcome_unknown"
  | "rebootstrap_required"
  | "protocol_error"
  | "apply_failed"
  | "ack_failed";

export interface PowDBSyncErrorOptions {
  cause?: unknown;
  localApplied?: boolean;
  appliedLsn?: bigint;
  remoteLsn?: bigint;
}

export class PowDBSyncError extends Error {
  readonly code: PowDBSyncErrorCode;
  readonly localApplied: boolean;
  readonly appliedLsn?: bigint;
  readonly remoteLsn?: bigint;

  constructor(message: string, code: PowDBSyncErrorCode, options: PowDBSyncErrorOptions = {}) {
    super(message, { cause: options.cause });
    this.name = "PowDBSyncError";
    this.code = code;
    this.localApplied = options.localApplied ?? false;
    this.appliedLsn = options.appliedLsn;
    this.remoteLsn = options.remoteLsn;
    Object.setPrototypeOf(this, PowDBSyncError.prototype);
  }
}

export interface PowDBSyncReplicaOptions {
  replicaId: string;
  identity: SyncIdentity;
  local: LocalReplica;
  remote: RemoteSyncClient;
  maxPullUnits?: number;
  maxPullBytes?: SyncU64;
  maxPullRounds?: number;
}

export interface SyncNowOptions {
  signal?: AbortSignal;
  maxPullRounds?: number;
  once?: boolean;
}

export interface SyncNowResult {
  status: SyncStatus;
  pulls: number;
  units: number;
  appliedLsn: bigint | null;
  stale: boolean;
  repairAction: SyncRepairAction;
  exhausted: boolean;
}

export interface BackgroundSyncOptions {
  intervalMs: number;
  immediate?: boolean;
  stopOnError?: boolean;
  signal?: AbortSignal;
  maxPullRounds?: number;
  onResult?: (result: SyncNowResult) => void;
  onError?: (error: PowDBSyncError) => void;
}

export interface BackgroundSyncHandle {
  stop(): void;
  isStopped(): boolean;
  isRunning(): boolean;
}

export interface WriteOptions {
  signal?: AbortSignal;
  sync?: "immediate" | "defer";
}

export type LocalWriteVisibility =
  | "visible"
  | "not_guaranteed"
  | "applied_but_unacked";

export interface WriteResult {
  result: QueryResult;
  /**
   * True only when a local read is guaranteed to include the committed remote
   * write. When false, inspect `localVisibility` and sync LSNs before deciding
   * whether to read locally or re-sync.
   */
  localVisible: boolean;
  localVisibility: LocalWriteVisibility;
  sync?: SyncNowResult;
  status?: SyncStatus;
  syncError?: PowDBSyncError;
  syncAppliedLsn?: bigint;
  syncRemoteLsn?: bigint;
}

const DEFAULT_MAX_PULL_UNITS = 512;
const DEFAULT_MAX_PULL_BYTES = 4 * 1024 * 1024;
const DEFAULT_MAX_PULL_ROUNDS = 32;
const MAX_U64 = 0xffff_ffff_ffff_ffffn;
const DDL_KEYWORDS = new Set(["alter", "create", "drop", "materialize", "type"]);

export class PowDBSyncReplica {
  private readonly replicaId: string;
  private readonly identity: NormalizedIdentity;
  private readonly local: LocalReplica;
  private readonly remote: RemoteSyncClient;
  private readonly maxPullUnits: number;
  private readonly maxPullBytes: bigint;
  private readonly maxPullRounds: number;

  constructor(options: PowDBSyncReplicaOptions) {
    this.replicaId = validateReplicaId(options.replicaId);
    this.identity = normalizeIdentity(options.identity);
    this.local = validateLocalReplica(options.local);
    this.remote = validateRemoteSyncClient(options.remote);
    this.maxPullUnits = validatePositiveInteger(
      options.maxPullUnits ?? DEFAULT_MAX_PULL_UNITS,
      "maxPullUnits",
    );
    this.maxPullBytes = toU64(options.maxPullBytes ?? DEFAULT_MAX_PULL_BYTES, "maxPullBytes");
    if (this.maxPullBytes === 0n) {
      throw new PowDBSyncError("maxPullBytes must be greater than zero", "protocol_error");
    }
    this.maxPullRounds = validatePositiveInteger(
      options.maxPullRounds ?? DEFAULT_MAX_PULL_ROUNDS,
      "maxPullRounds",
    );
  }

  async queryReadonly(query: string, opts?: { signal?: AbortSignal }): Promise<QueryResult> {
    return this.local.queryReadonly(query, opts);
  }

  async status(opts?: { signal?: AbortSignal }): Promise<SyncStatus> {
    return this.remote.syncStatus(this.replicaId, opts);
  }

  startBackgroundSync(options: BackgroundSyncOptions): BackgroundSyncHandle {
    const intervalMs = validatePositiveInteger(options.intervalMs, "intervalMs");
    let stopped = false;
    let running = false;
    let timer: ReturnType<typeof setInterval> | undefined;

    const stop = () => {
      if (stopped) return;
      stopped = true;
      if (timer !== undefined) {
        clearInterval(timer);
        timer = undefined;
      }
      options.signal?.removeEventListener("abort", stop);
    };

    const tick = async () => {
      if (stopped || running) return;
      running = true;
      try {
        const result = await this.syncNow({
          signal: options.signal,
          maxPullRounds: options.maxPullRounds,
        });
        if (!stopped) {
          options.onResult?.(result);
        }
      } catch (err) {
        if (!stopped) {
          const syncError = toSyncError(err, "remote_unavailable");
          try {
            options.onError?.(syncError);
          } catch {
            // Background callbacks must not turn the scheduler into an
            // unhandled rejection source.
          }
          if (options.stopOnError) {
            stop();
          }
        }
      } finally {
        running = false;
      }
    };

    const handle: BackgroundSyncHandle = {
      stop,
      isStopped: () => stopped,
      isRunning: () => running,
    };

    if (options.signal?.aborted) {
      stop();
      return handle;
    }
    options.signal?.addEventListener("abort", stop, { once: true });

    timer = setInterval(() => {
      void tick();
    }, intervalMs);
    if (typeof timer.unref === "function") timer.unref();

    if (options.immediate ?? true) {
      void tick();
    }

    return handle;
  }

  async syncNow(options: SyncNowOptions = {}): Promise<SyncNowResult> {
    const maxRounds = validatePositiveInteger(
      options.maxPullRounds ?? this.maxPullRounds,
      "maxPullRounds",
    );
    let status = await this.remote.syncStatus(this.replicaId, { signal: options.signal });
    let pulls = 0;
    let units = 0;
    let appliedLsn: bigint | null = status.lastAppliedLsn;

    while (true) {
      this.throwIfUnusableStatus(status);
      if (!status.stale || status.repairAction === "none") {
        return syncSummary(status, pulls, units, appliedLsn, false);
      }
      if (status.repairAction === "awaitArchive") {
        return syncSummary(status, pulls, units, appliedLsn, false);
      }
      if (status.repairAction !== "pull") {
        throw new PowDBSyncError(
          `unsupported sync repair action: ${status.repairAction}`,
          "protocol_error",
        );
      }
      if (status.lastAppliedLsn === null) {
        throw new PowDBSyncError(
          "replica has no primary cursor; rebootstrap required",
          "rebootstrap_required",
        );
      }
      if (pulls >= maxRounds) {
        return syncSummary(status, pulls, units, appliedLsn, true);
      }

      const sinceLsn = status.lastAppliedLsn;
      const pull = await this.remote.syncPull(
        {
          replicaId: this.replicaId,
          sinceLsn,
          maxUnits: this.maxPullUnits,
          maxBytes: this.maxPullBytes,
          ...this.identity,
        },
        { signal: options.signal },
      );
      status = pull.status;
      this.throwIfUnusableStatus(status);

      if (pull.units.length === 0) {
        return syncSummary(status, pulls, units, appliedLsn, false);
      }

      const throughLsn = validatePulledChunk(sinceLsn, pull.units);
      const apply = await this.local.applyRetainedUnits({
        replicaId: this.replicaId,
        sinceLsn,
        units: pull.units,
        ...this.identity,
      });
      const applied =
        apply.throughLsn === undefined
          ? throughLsn
          : toU64(apply.throughLsn, "throughLsn");
      if (applied !== throughLsn) {
        throw new PowDBSyncError(
          `local apply reported LSN ${applied} for chunk through ${throughLsn}`,
          "apply_failed",
        );
      }
      const locallyApplied = {
        localApplied: true,
        appliedLsn: applied,
        remoteLsn: pull.status.remoteLsn,
      };

      let ack: SyncAckResult;
      try {
        ack = await this.remote.syncAck(
          {
            replicaId: this.replicaId,
            appliedLsn: applied,
            remoteLsn: pull.status.remoteLsn,
          },
          { signal: options.signal },
        );
      } catch (err) {
        throw new PowDBSyncError(errorMessage(err, "sync ack failed after local apply"), "ack_failed", {
          ...locallyApplied,
          cause: err,
        });
      }
      validateAckResult(ack, applied, pull.status.remoteLsn, locallyApplied);
      status = ack.status;
      this.throwIfUnusableStatus(status, locallyApplied);
      appliedLsn = applied;
      pulls++;
      units += pull.units.length;

      if (options.once || !pull.hasMore) {
        return syncSummary(status, pulls, units, appliedLsn, false);
      }
    }
  }

  async write(
    query: string,
    paramsOrOpts?: QueryParam[] | WriteOptions,
    maybeOpts?: WriteOptions,
  ): Promise<WriteResult> {
    if (isDdl(query)) {
      throw new PowDBSyncError(
        "V1 embedded sync rejects DDL writes; rebootstrap or upgrade with schema propagation",
        "ddl_not_supported",
      );
    }
    const hasParams = Array.isArray(paramsOrOpts);
    const params = hasParams ? paramsOrOpts : undefined;
    const opts = hasParams ? maybeOpts : (paramsOrOpts as WriteOptions | undefined);

    let result: QueryResult;
    try {
      result =
        params === undefined
          ? await this.remote.query(query, { signal: opts?.signal })
          : await this.remote.query(query, params, { signal: opts?.signal });
    } catch (err) {
      throw classifyWriteError(err);
    }

    if (opts?.sync === "defer") {
      return { result, localVisible: false, localVisibility: "not_guaranteed" };
    }

    try {
      const sync = await this.syncNow({ signal: opts?.signal });
      return {
        result,
        sync,
        status: sync.status,
        localVisible: !sync.stale,
        localVisibility: sync.stale ? "not_guaranteed" : "visible",
      };
    } catch (err) {
      const syncError = toSyncError(err, "apply_failed");
      let status: SyncStatus | undefined;
      try {
        status = await this.status({ signal: opts?.signal });
      } catch {
        status = undefined;
      }
      return {
        result,
        status,
        syncError,
        ...writeVisibilityFromSyncError(syncError),
      };
    }
  }

  private throwIfUnusableStatus(
    status: SyncStatus,
    context: PowDBSyncErrorOptions = {},
  ): void {
    if (!status.active || status.repairAction === "rebootstrap") {
      throw new PowDBSyncError(
        status.lastSyncError ?? "replica must be rebootstrapped before sync can continue",
        "rebootstrap_required",
        context,
      );
    }
  }
}

type NormalizedIdentity = {
  databaseId: string | Uint8Array;
  primaryGeneration: bigint;
  walFormatVersion: number;
  catalogVersion: number;
  segmentFormatVersion: number;
};

function normalizeIdentity(identity: SyncIdentity): NormalizedIdentity {
  return {
    databaseId: normalizeDatabaseId(identity.databaseId),
    primaryGeneration: toU64(identity.primaryGeneration, "primaryGeneration"),
    walFormatVersion: toU16(identity.walFormatVersion, "walFormatVersion"),
    catalogVersion: toU16(identity.catalogVersion, "catalogVersion"),
    segmentFormatVersion: toU16(identity.segmentFormatVersion, "segmentFormatVersion"),
  };
}

function normalizeDatabaseId(databaseId: string | Uint8Array): string | Uint8Array {
  if (typeof databaseId === "string") {
    if (!/^[0-9a-fA-F]{32}$/.test(databaseId)) {
      throw new PowDBSyncError(
        "databaseId string must be exactly 32 hex characters",
        "protocol_error",
      );
    }
    return databaseId.toLowerCase();
  }
  if (databaseId.byteLength !== 16) {
    throw new PowDBSyncError(
      `databaseId must be exactly 16 bytes, got ${databaseId.byteLength}`,
      "protocol_error",
    );
  }
  return new Uint8Array(databaseId);
}

function validateReplicaId(replicaId: string): string {
  if (!/^[A-Za-z0-9._:-]{1,128}$/.test(replicaId)) {
    throw new PowDBSyncError(
      "replicaId must be 1-128 characters of letters, numbers, '.', '_', ':', or '-'",
      "protocol_error",
    );
  }
  return replicaId;
}

function validateRemoteSyncClient(remote: RemoteSyncClient): RemoteSyncClient {
  const candidate = remote as Partial<Record<keyof RemoteSyncClient, unknown>> | null;
  if (
    candidate == null ||
    typeof candidate.query !== "function" ||
    typeof candidate.syncStatus !== "function" ||
    typeof candidate.syncPull !== "function" ||
    typeof candidate.syncAck !== "function"
  ) {
    throw new PowDBSyncError(
      "remote must implement query, syncStatus, syncPull, and syncAck",
      "protocol_error",
    );
  }
  return remote;
}

function validateLocalReplica(local: LocalReplica): LocalReplica {
  const candidate = local as Partial<Record<keyof LocalReplica, unknown>> | null;
  if (
    candidate == null ||
    typeof candidate.queryReadonly !== "function" ||
    typeof candidate.applyRetainedUnits !== "function"
  ) {
    throw new PowDBSyncError(
      "local must implement queryReadonly and applyRetainedUnits",
      "protocol_error",
    );
  }
  return local;
}

function validatePositiveInteger(value: number, label: string): number {
  if (!Number.isInteger(value) || value < 1) {
    throw new PowDBSyncError(`${label} must be a positive integer`, "protocol_error");
  }
  return value;
}

function toU16(value: number, label: string): number {
  if (!Number.isInteger(value) || value < 0 || value > 0xffff) {
    throw new PowDBSyncError(`${label} must fit in u16`, "protocol_error");
  }
  return value;
}

function toU64(value: SyncU64, label: string): bigint {
  if (typeof value === "bigint") {
    if (value < 0n || value > MAX_U64) {
      throw new PowDBSyncError(`${label} must fit in u64`, "protocol_error");
    }
    return value;
  }
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new PowDBSyncError(
      `${label} must be a safe non-negative integer or bigint`,
      "protocol_error",
    );
  }
  return BigInt(value);
}

function validatePulledChunk(sinceLsn: bigint, units: RetainedUnit[]): bigint {
  let expected = sinceLsn + 1n;
  for (const unit of units) {
    if (unit.lsn !== expected) {
      throw new PowDBSyncError(
        `sync pull returned non-contiguous chunk: expected LSN ${expected}, got ${unit.lsn}`,
        "protocol_error",
      );
    }
    if (!Number.isInteger(unit.recordType) || unit.recordType < 0 || unit.recordType > 0xff) {
      throw new PowDBSyncError(
        `sync pull returned invalid retained-unit record type ${unit.recordType}`,
        "protocol_error",
      );
    }
    expected++;
  }
  return units.length === 0 ? sinceLsn : units[units.length - 1]!.lsn;
}

function validateAckResult(
  ack: SyncAckResult,
  appliedLsn: bigint,
  remoteLsn: bigint,
  context: PowDBSyncErrorOptions,
): void {
  if (ack.appliedLsn !== appliedLsn) {
    throw new PowDBSyncError(
      `sync ack reported applied LSN ${ack.appliedLsn} for local apply through ${appliedLsn}`,
      "protocol_error",
      context,
    );
  }
  if (ack.remoteLsn < remoteLsn) {
    throw new PowDBSyncError(
      `sync ack reported remote LSN ${ack.remoteLsn} behind requested remote LSN ${remoteLsn}`,
      "protocol_error",
      context,
    );
  }
  if (ack.status.remoteLsn < ack.remoteLsn) {
    throw new PowDBSyncError(
      `sync ack status remote LSN ${ack.status.remoteLsn} is behind ack remote LSN ${ack.remoteLsn}`,
      "protocol_error",
      context,
    );
  }
  if (!ack.advanced && ack.previousAppliedLsn < appliedLsn) {
    throw new PowDBSyncError(
      `sync ack did not advance from ${ack.previousAppliedLsn} to ${appliedLsn}`,
      "protocol_error",
      context,
    );
  }
  if (ack.status.lastAppliedLsn === null || ack.status.lastAppliedLsn < appliedLsn) {
    throw new PowDBSyncError(
      "sync ack status did not publish the locally applied LSN",
      "protocol_error",
      context,
    );
  }
}

function syncSummary(
  status: SyncStatus,
  pulls: number,
  units: number,
  appliedLsn: bigint | null,
  exhausted: boolean,
): SyncNowResult {
  return {
    status,
    pulls,
    units,
    appliedLsn,
    stale: status.stale,
    repairAction: status.repairAction,
    exhausted,
  };
}

function writeVisibilityFromSyncError(syncError: PowDBSyncError): Pick<
  WriteResult,
  "localVisible" | "localVisibility" | "syncAppliedLsn" | "syncRemoteLsn"
> {
  if (
    syncError.localApplied &&
    syncError.appliedLsn !== undefined &&
    syncError.remoteLsn !== undefined
  ) {
    return {
      localVisible: syncError.appliedLsn >= syncError.remoteLsn,
      localVisibility: "applied_but_unacked",
      syncAppliedLsn: syncError.appliedLsn,
      syncRemoteLsn: syncError.remoteLsn,
    };
  }
  return {
    localVisible: false,
    localVisibility: "not_guaranteed",
  };
}

function isDdl(query: string): boolean {
  const keyword = firstKeyword(query);
  return keyword !== null && DDL_KEYWORDS.has(keyword);
}

function firstKeyword(query: string): string | null {
  let s = query;
  while (true) {
    s = s.trimStart();
    if (s.startsWith("--")) {
      const next = s.indexOf("\n");
      if (next === -1) return null;
      s = s.slice(next + 1);
      continue;
    }
    if (s.startsWith("/*")) {
      const next = s.indexOf("*/");
      if (next === -1) return null;
      s = s.slice(next + 2);
      continue;
    }
    break;
  }
  return /^[A-Za-z_][A-Za-z0-9_]*/.exec(s)?.[0]?.toLowerCase() ?? null;
}

function classifyWriteError(err: unknown): PowDBSyncError {
  const code = typeof err === "object" && err !== null && "code" in err
    ? String((err as { code?: unknown }).code)
    : "";
  if (code === "connect_failed") {
    return new PowDBSyncError("remote primary is unavailable", "remote_unavailable", {
      cause: err,
    });
  }
  if (code === "query_failed" || code === "auth_failed") {
    return new PowDBSyncError(errorMessage(err, "remote primary rejected write"), "remote_write_failed", {
      cause: err,
    });
  }
  return new PowDBSyncError(
    "remote write outcome is unknown; do not retry blindly",
    "commit_outcome_unknown",
    { cause: err },
  );
}

function toSyncError(err: unknown, fallback: PowDBSyncErrorCode): PowDBSyncError {
  if (err instanceof PowDBSyncError) return err;
  return new PowDBSyncError(errorMessage(err, "sync failed"), fallback, { cause: err });
}

function errorMessage(err: unknown, fallback: string): string {
  return err instanceof Error && err.message.length > 0 ? err.message : fallback;
}
