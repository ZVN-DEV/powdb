/**
 * PowDB TypeScript client.
 *
 * Thin async wrapper around a TCP (or TLS) socket speaking the PowDB wire
 * protocol.
 *
 *     const client = await Client.connect({
 *       host: "127.0.0.1",
 *       port: 5433,
 *       dbName: "default",
 *       password: process.env.POWDB_PASSWORD,
 *     });
 *
 *     const result = await client.query("User filter .age > 27 { .name, .age }");
 *     await client.close();
 */

import * as net from "node:net";
import * as tls from "node:tls";
import { EventEmitter } from "node:events";
import {
  encode,
  tryDecode,
  legacyServerHello,
  MAX_SYNC_PULL_BYTES,
  MAX_SYNC_PULL_UNITS,
  PROTOCOL_VERSION_LEGACY,
  PROTOCOL_VERSION_NEGOTIATED,
  WIRE_FEATURE,
  type ClientHello,
  type Message,
  type ServerHello,
  type NativeJson,
  type SyncRepairAction,
  type WireRetainedUnit,
  type WireParam,
  type WireSyncStatus,
  type WireValue,
} from "./protocol.js";
import {
  errorCodeForWireClass,
  isPowDBError,
  PowDBError,
  PowDBScriptError,
  WIRE_ERROR_CLASS,
} from "./errors.js";
import { splitStatements } from "./script.js";
import {
  coerceRows,
  type TypedRow,
  type TypedSchema,
} from "./typed.js";

/** Client library version. Compared to the server's reported version. */
export const CLIENT_VERSION = "0.25.0";

/**
 * Everything this client states about itself at connect time: the wire
 * protocol range it can speak, the catalog format ceiling it can read, and
 * the named wire features it understands.
 *
 * This is the single source of truth for compatibility. It is sent verbatim
 * in the `Connect` hello block, and the server's answer is checked against it
 * before {@link Client.connect} resolves, so a mismatch is a handshake
 * failure, never a surprise on some later frame.
 *
 * `catalogVersion` v7 is the entity-links format: it appends a relationship
 * link section before the trailing CRC, staircase-defaulted so every older
 * file still loads, and activates lazily on the first `link` declaration. The
 * client treats catalog payloads as opaque bytes and only states this ceiling.
 */
export const CLIENT_CAPABILITIES = {
  minProtocolVersion: PROTOCOL_VERSION_LEGACY,
  maxProtocolVersion: PROTOCOL_VERSION_NEGOTIATED,
  catalogVersion: 7,
  features: [
    WIRE_FEATURE.params,
    WIRE_FEATURE.sql,
    WIRE_FEATURE.nativeTyped,
    WIRE_FEATURE.errorClass,
    WIRE_FEATURE.sync,
    WIRE_FEATURE.entityLinks,
    WIRE_FEATURE.nestedProjection,
  ] as string[],
} as const;

/**
 * The maximum catalog format version this client can read.
 *
 * Derived from {@link CLIENT_CAPABILITIES} rather than declared separately:
 * the handshake and the sync-pull request now state the same number, so there
 * is one place to raise it. State it as the `catalogVersion` in sync pull
 * requests: the server accepts any replica whose maximum is at least its
 * active catalog format and rejects an older replica.
 */
export const SUPPORTED_CATALOG_VERSION: number =
  CLIENT_CAPABILITIES.catalogVersion;

/**
 * Throw when a server-reported catalog format is newer than this client can
 * read. Accepts `serverCatalogVersion <= SUPPORTED_CATALOG_VERSION`; rejects a
 * newer server, which requires upgrading the client. This is the same check
 * the handshake applies to the server's reported catalog version.
 */
export function assertServerCatalogVersionSupported(
  serverCatalogVersion: number,
  clientMax: number = CLIENT_CAPABILITIES.catalogVersion,
): void {
  if (!Number.isInteger(serverCatalogVersion) || serverCatalogVersion < 1) {
    throw new Error(
      `invalid server catalog version ${serverCatalogVersion}`,
    );
  }
  if (serverCatalogVersion > clientMax) {
    throw new Error(
      `server catalog format v${serverCatalogVersion} is newer than this client supports (max v${clientMax}); upgrade the client`,
    );
  }
}

/**
 * The client half of the handshake check: confirm the server just reached can
 * actually serve this client. Returns an explanatory message when it cannot,
 * or `null` when the pairing is fine.
 *
 * A server that stated no catalog version (pre-0.22.0) is not judged on one,
 * and a `clientCatalogVersion` of `0` opts out of the catalog check.
 */
export function serverCapabilityMismatch(
  server: ServerHello,
  minProtocol: number,
  requiredFeatures: readonly string[],
  clientCatalogVersion: number,
): string | null {
  if (server.protocol < minProtocol) {
    return (
      `unsupported wire protocol: server negotiated v${server.protocol}, ` +
      `this client requires at least v${minProtocol}; upgrade the server`
    );
  }
  const missing = requiredFeatures.find((f) => !server.features.includes(f));
  if (missing !== undefined) {
    return `server does not support required wire feature '${missing}'; upgrade the server`;
  }
  if (
    clientCatalogVersion > 0 &&
    server.catalogVersion > 0 &&
    server.catalogVersion > clientCatalogVersion
  ) {
    return (
      `server catalog format v${server.catalogVersion} is newer than this ` +
      `client supports (max v${clientCatalogVersion}); upgrade the client`
    );
  }
  return null;
}

export type QueryResult =
  | { kind: "rows"; columns: string[]; rows: string[][] }
  | { kind: "scalar"; value: string }
  | { kind: "ok"; affected: bigint }
  | { kind: "message"; message: string };

export type { NativeJson } from "./protocol.js";

/** A value returned by the lossless native wire surface. */
export type NativeValue =
  | null
  | number
  | bigint
  | boolean
  | string
  | Uint8Array
  | NativeJson;

/**
 * One row of a native (lossless) result, keyed by column name. The default row
 * type of {@link Client.queryObjects} when no generic is supplied.
 */
export type NativeRow = Record<string, NativeValue>;

export type NativeQueryResult =
  | { kind: "rows"; columns: string[]; rows: NativeValue[][] }
  | { kind: "scalar"; value: NativeValue }
  | { kind: "ok"; affected: bigint }
  | { kind: "message"; message: string };

/**
 * The fully lossless result of {@link Client.queryNativeRaw}: every cell is the
 * raw {@link WireValue} tagged union straight off the wire, with no conversion
 * to {@link NativeValue}. Use this when you need storage-level identity that the
 * convenience conversion erases: the raw PJ1 bytes of a JSON cell (`pj1`), or
 * telling an absent value (`{ type: "empty" }`) apart from the string `"null"`
 * (`{ type: "str", value: "null" }`) or a JSON null (`{ type: "json", value:
 * null }`), all of which {@link queryNative} collapses to `null`.
 */
export type RawNativeQueryResult =
  | { kind: "rows"; columns: string[]; rows: WireValue[][] }
  | { kind: "scalar"; value: WireValue }
  | { kind: "ok"; affected: bigint }
  | { kind: "message"; message: string };

/**
 * A value bound to a positional `$N` placeholder in {@link Client.query}.
 *
 * The server binds these at the token level — a string is substituted as a
 * literal token, never interpolated — so injection-shaped input is inert.
 * Numbers bind as ints when integral and floats otherwise; `bigint` always
 * binds as an int; `null` binds PowQL `null`.
 */
export type QueryParam = string | number | bigint | boolean | null;

/** Options for {@link Client.execScript} / {@link Pool.execScript}. */
export interface ExecScriptOptions {
  /**
   * When `true`, keep dispatching statements after one fails and return a
   * per-statement outcome array instead of throwing. Defaults to `false`
   * (fail-fast: stop dispatching and reject with a {@link PowDBScriptError}
   * carrying the failing statement's index and the results so far).
   */
  continueOnError?: boolean;
  /**
   * When `true`, run the whole script atomically: `execScript` opens a
   * transaction before dispatching and sends `commit` only after EVERY
   * statement's reply has arrived successfully — on any failure it sends
   * `rollback` instead, so no statement's effect survives. This is the only
   * all-or-nothing mode: embedding your own `begin`/`commit` in a pipelined
   * script is NOT safe (a trailing `commit` is already on the wire when an
   * earlier error reply arrives, so partial work would commit), and a
   * transactional script containing its own transaction-control statements
   * is rejected up front. Mutually exclusive with `continueOnError`.
   */
  transactional?: boolean;
  /** Abort the remaining statements (see {@link Client.query}). */
  signal?: AbortSignal;
}

/** Transaction-control statements a `transactional` script may not contain. */
const TX_CONTROL_RE = /^(begin|commit|rollback)\b/i;

/**
 * Leading trivia (whitespace and `#` line comments) that may precede a
 * statement's first real token. Must be stripped before matching
 * {@link TX_CONTROL_RE}: the server's lexer skips comments, so
 * `"# note\ncommit"` executes a commit — the guard has to see it too.
 */
const LEADING_TRIVIA_RE = /^(?:\s+|#[^\n]*)+/;

/**
 * Per-statement outcome from `execScript(script, { continueOnError: true })`.
 * Array order matches statement order in the script.
 */
export type ScriptStatementOutcome =
  | { statement: string; ok: true; result: QueryResult }
  | { statement: string; ok: false; error: Error };

/** Unsigned 64-bit sync protocol value. Numbers must be safe non-negative integers. */
export type SyncU64 = bigint | number;

/**
 * Database identity carried by the sync protocol. Strings are the 32-hex-char
 * form stored in `.powdb-sync/identity.json`; byte arrays must be exactly 16B.
 */
export type SyncDatabaseId = string | Uint8Array;

export interface SyncPullRequest {
  replicaId: string;
  sinceLsn: SyncU64;
  maxUnits: number;
  maxBytes: SyncU64;
  databaseId: SyncDatabaseId;
  primaryGeneration: SyncU64;
  walFormatVersion: number;
  catalogVersion: number;
  segmentFormatVersion: number;
}

export interface SyncAckRequest {
  replicaId: string;
  appliedLsn: SyncU64;
  remoteLsn: SyncU64;
}

export interface SyncPullResult {
  status: WireSyncStatus;
  units: WireRetainedUnit[];
  hasMore: boolean;
}

export interface SyncAckResult {
  previousAppliedLsn: bigint;
  appliedLsn: bigint;
  remoteLsn: bigint;
  advanced: boolean;
  status: WireSyncStatus;
}

function socketChunkToBuffer(chunk: Buffer | string): Buffer {
  return typeof chunk === "string" ? Buffer.from(chunk) : chunk;
}

/** Map a JS {@link QueryParam} to its wire encoding. */
function toWireParam(p: QueryParam): WireParam {
  if (p === null) return { tag: "null" };
  switch (typeof p) {
    case "string":
      return { tag: "str", value: p };
    case "boolean":
      return { tag: "bool", value: p };
    case "bigint":
      return { tag: "int", value: p };
    case "number":
      return Number.isInteger(p)
        ? { tag: "int", value: BigInt(p) }
        : { tag: "float", value: p };
    default:
      throw new PowDBError(
        `unsupported query parameter type: ${typeof p}`,
        "protocol_error",
      );
  }
}

function fromWireValue(value: WireValue): NativeValue {
  switch (value.type) {
    case "empty":
      return null;
    case "int":
      return value.value >= BigInt(Number.MIN_SAFE_INTEGER) &&
        value.value <= BigInt(Number.MAX_SAFE_INTEGER)
        ? Number(value.value)
        : value.value;
    case "float":
    case "bool":
    case "str":
      return value.value;
    case "datetime":
      return value.value;
    case "uuid": {
      const hex = Array.from(value.value, (byte) =>
        byte.toString(16).padStart(2, "0"),
      ).join("");
      return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
    }
    case "bytes":
      return new Uint8Array(value.value);
    case "json":
      return value.value;
  }
}

function nativeQueryResult(reply: Message): NativeQueryResult {
  switch (reply.type) {
    case "ResultRowsNative":
      return {
        kind: "rows",
        columns: reply.columns,
        rows: reply.rows.map((row) => row.map(fromWireValue)),
      };
    case "ResultScalarNative":
      return { kind: "scalar", value: fromWireValue(reply.value) };
    case "ResultOk":
      return { kind: "ok", affected: reply.affected };
    case "ResultMessage":
      return { kind: "message", message: reply.message };
    case "Error":
      throw new PowDBError(
        `query failed: ${reply.message}`,
        errorCodeForWireClass(reply.errorClass),
        { wireErrorClass: reply.errorClass },
      );
    default:
      throw new PowDBError(
        `unexpected reply to native query: ${reply.type}`,
        "protocol_error",
      );
  }
}

/**
 * Turn a rows-shaped native result into object rows keyed by column name.
 * Non-rows results are a caller mistake, not a server error, so they raise the
 * same `query_failed` code `queryTyped` uses.
 */
function nativeRowObjects<Row>(
  result: NativeQueryResult,
  method: string,
): Row[] {
  if (result.kind !== "rows") {
    throw new PowDBError(
      `${method}: expected rows result, got ${result.kind}`,
      "query_failed",
    );
  }
  const { columns, rows } = result;
  return rows.map((values) => {
    const row: NativeRow = {};
    for (let i = 0; i < columns.length; i++) {
      row[columns[i]!] = values[i] ?? null;
    }
    return row as Row;
  });
}

function rawNativeQueryResult(reply: Message): RawNativeQueryResult {
  switch (reply.type) {
    case "ResultRowsNative":
      return { kind: "rows", columns: reply.columns, rows: reply.rows };
    case "ResultScalarNative":
      return { kind: "scalar", value: reply.value };
    case "ResultOk":
      return { kind: "ok", affected: reply.affected };
    case "ResultMessage":
      return { kind: "message", message: reply.message };
    case "Error":
      throw new PowDBError(
        `query failed: ${reply.message}`,
        errorCodeForWireClass(reply.errorClass),
        { wireErrorClass: reply.errorClass },
      );
    default:
      throw new PowDBError(
        `unexpected reply to native query: ${reply.type}`,
        "protocol_error",
      );
  }
}

function toU64(value: SyncU64, label: string): bigint {
  if (typeof value === "bigint") {
    if (value < 0n || value > 0xffff_ffff_ffff_ffffn) {
      throw new PowDBError(`${label} must fit in u64`, "protocol_error");
    }
    return value;
  }
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new PowDBError(
      `${label} must be a safe non-negative integer or bigint`,
      "protocol_error",
    );
  }
  return BigInt(value);
}

function toU16(value: number, label: string): number {
  if (!Number.isInteger(value) || value < 0 || value > 0xffff) {
    throw new PowDBError(`${label} must fit in u16`, "protocol_error");
  }
  return value;
}

function toSyncMaxUnits(value: number): number {
  if (
    !Number.isInteger(value) ||
    value < 1 ||
    value > MAX_SYNC_PULL_UNITS
  ) {
    throw new PowDBError(
      `maxUnits must be between 1 and ${MAX_SYNC_PULL_UNITS}`,
      "protocol_error",
    );
  }
  return value;
}

function toSyncMaxBytes(value: SyncU64): bigint {
  const bytes = toU64(value, "maxBytes");
  if (bytes < 1n || bytes > BigInt(MAX_SYNC_PULL_BYTES)) {
    throw new PowDBError(
      `maxBytes must be between 1 and ${MAX_SYNC_PULL_BYTES}`,
      "protocol_error",
    );
  }
  return bytes;
}

function toDatabaseId(value: SyncDatabaseId): Buffer {
  if (typeof value === "string") {
    if (!/^[0-9a-fA-F]{32}$/.test(value)) {
      throw new PowDBError(
        "databaseId string must be exactly 32 hex characters",
        "protocol_error",
      );
    }
    return Buffer.from(value, "hex");
  }
  const bytes = Buffer.from(value);
  if (bytes.length !== 16) {
    throw new PowDBError(
      `databaseId must be exactly 16 bytes, got ${bytes.length}`,
      "protocol_error",
    );
  }
  return bytes;
}

export interface ClientOptions {
  /** TCP host. Required unless `path` (a Unix domain socket) is given. */
  host?: string;
  /** TCP port. Required unless `path` (a Unix domain socket) is given. */
  port?: number;
  /**
   * Path to a Unix domain socket. When set, the client connects over the
   * socket instead of TCP (same-host, ~2× lower round-trip latency) and
   * `host`/`port`/`tls` are ignored. Requires a server started with
   * `--socket <path>`.
   */
  path?: string;
  dbName?: string;
  password?: string | null;
  /**
   * User name for multi-user authentication. Servers ≥0.4.5 with named users
   * defined require a `(user, password)` pair; role enforcement (readonly vs
   * readwrite) requires server ≥0.4.6. Omit for legacy shared-password or
   * no-auth servers — the Connect frame is then byte-identical to 0.3.x.
   */
  user?: string;
  /** Connection timeout in ms. Defaults to 5000. */
  connectTimeoutMs?: number;
  /**
   * Enable TLS. When `true`, connect over TLS with system defaults
   * (servername is taken from `host`). When an object, passed through to
   * `tls.connect(port, host, options)`. Defaults to plain TCP.
   */
  tls?: boolean | tls.ConnectionOptions;
  /**
   * Pipelined ("eager") connect. When `true`, {@link Client.connect}
   * resolves as soon as the socket is open and the Connect frame has been
   * written — it does NOT wait for the server's ConnectOk. Queries issued
   * immediately are queued and written right behind the Connect frame (the
   * server reads frames sequentially, so this is valid on the wire), saving
   * a full round trip on every fresh connection.
   *
   * If the handshake then fails (bad password, protocol mismatch), every
   * queued query rejects with the handshake error and the client closes.
   * `serverVersion` is `""` until the ConnectOk arrives; await
   * {@link Client.ready} if you need the handshake settled. Defaults to
   * `false` (connect blocks until ConnectOk, exactly as before).
   */
  eager?: boolean;
  /**
   * Refuse to connect unless the negotiated wire protocol is at least this
   * version. Defaults to {@link PROTOCOL_VERSION_LEGACY} (`1`), which accepts
   * a pre-0.22.0 server that cannot negotiate at all. Raise it to
   * {@link PROTOCOL_VERSION_NEGOTIATED} when your code depends on a
   * negotiated feature set being present.
   *
   * A shortfall rejects during the handshake with a `protocol_version` error,
   * before any query frame is written.
   */
  requireProtocolVersion?: number;
  /**
   * Refuse to connect unless the server agreed to every one of these
   * {@link WIRE_FEATURE} names. Defaults to none. A pre-0.22.0 server names
   * no features at all, so requiring any implies requiring protocol v2.
   */
  requireFeatures?: readonly string[];
  /**
   * Send the pre-0.22.0 `Connect` frame with no hello block, so the frame is
   * byte-identical to what a 0.21.0 client writes. Only useful for testing
   * the legacy path; leave unset in production. Defaults to `false`.
   */
  legacyHandshake?: boolean;
}

type Pending = {
  resolve: (msg: Message) => void;
  reject: (err: Error) => void;
  /** Set to true once the promise has been resolved or rejected. */
  settled: boolean;
};

/** Module-level set of host:port pairs we've already warned about. */
const versionWarnings = new Set<string>();

/** Extract the major component of a dotted version string, e.g. "0.2.0" → "0". */
function majorOf(version: string): string {
  const dot = version.indexOf(".");
  return dot === -1 ? version : version.slice(0, dot);
}

/**
 * Build an AbortError. A caller-supplied custom `Error` reason
 * (`ctrl.abort(myError)`) passes through as-is to match DOM semantics. The
 * default abort reason — Node's `DOMException` named `"AbortError"` — and any
 * non-Error reason are wrapped in a `PowDBError` with code `"aborted"` so the
 * common `ctrl.abort()` case branches uniformly on `err.code === "aborted"`.
 */
function abortError(signal?: AbortSignal): Error {
  if (signal && signal.reason !== undefined) {
    const r = signal.reason;
    const isDefaultAbort =
      r instanceof DOMException && r.name === "AbortError";
    if (r instanceof Error && !isDefaultAbort) return r;
    return new PowDBError(
      isDefaultAbort ? "query was aborted" : String(r),
      "aborted",
    );
  }
  return new PowDBError("query was aborted", "aborted");
}

/**
 * Event map for {@link Client}. Typed so `client.on("query", ...)` gets
 * inference for the payload.
 *
 *   - `"query"`: one emission per completed (or failed) query attempt,
 *     whether via `query` or `queryTyped`. `durationMs` is the client-side
 *     round-trip including abort handling. `ok=false` means the server
 *     returned an Error frame or the client rejected locally.
 *   - `"close"`: the underlying socket has been fully torn down. Emitted
 *     once per client.
 */
export interface ClientEvents {
  query: [
    {
      query: string;
      durationMs: number;
      ok: boolean;
      kind?: "rows" | "scalar" | "ok" | "message";
      error?: Error;
    },
  ];
  sync: [
    {
      operation: "status" | "pull" | "ack";
      replicaId: string;
      durationMs: number;
      ok: boolean;
      status?: WireSyncStatus;
      stale?: boolean;
      repairAction?: SyncRepairAction;
      units?: number;
      advanced?: boolean;
      error?: Error;
    },
  ];
  close: [{ error: Error | null }];
}

export class Client extends EventEmitter<ClientEvents> {
  private readonly socket: net.Socket;
  /** FIFO of raw chunks; concatenated lazily when we try to decode. */
  private readonly chunks: Buffer[] = [];
  /** Cached length of everything currently in `chunks`. */
  private totalLen = 0;
  private readonly pending: Pending[] = [];
  private closed = false;
  private closeError: Error | null = null;
  /** Settled once the Connect→ConnectOk handshake completes (or fails). */
  private handshake: Promise<void> = Promise.resolve();
  /** True once ConnectOk has been received. */
  private handshakeComplete = false;
  private _serverVersion = "";
  private _serverHello: ServerHello = legacyServerHello();

  /**
   * Server version from the ConnectOk frame. For a client opened with
   * `eager: true` this is `""` until the handshake reply arrives (await
   * {@link ready} to guarantee it is populated).
   */
  get serverVersion(): string {
    return this._serverVersion;
  }

  /**
   * The negotiated handshake outcome: protocol version, the server's
   * supported range, its catalog format, and the agreed feature set. Before
   * the handshake settles (and against a pre-0.22.0 server that cannot
   * negotiate) this is the legacy hello: protocol v1, no features.
   */
  get serverHello(): ServerHello {
    return this._serverHello;
  }

  /** The wire protocol version in use for this connection. */
  get protocolVersion(): number {
    return this._serverHello.protocol;
  }

  /**
   * Whether the server agreed to a named {@link WIRE_FEATURE}. Always `false`
   * against a pre-0.22.0 server, which names nothing.
   */
  hasFeature(feature: string): boolean {
    return this._serverHello.features.includes(feature);
  }

  private constructor(socket: net.Socket) {
    super();
    this.socket = socket;

    this.socket.on("data", (chunk) => this.onData(socketChunkToBuffer(chunk)));
    this.socket.on("error", (err) => this.onClose(err));
    this.socket.on("close", () => this.onClose(null));
  }

  /**
   * Open a connection, send Connect, and wait for ConnectOk.
   *
   * With `eager: true`, resolve as soon as the Connect frame is written
   * instead — queries may be issued immediately and are pipelined behind
   * the handshake (see {@link ClientOptions.eager} and {@link ready}).
   */
  static async connect(opts: ClientOptions): Promise<Client> {
    const {
      host,
      port,
      path,
      dbName = "default",
      password = null,
      user,
      connectTimeoutMs = 5000,
      tls: tlsOpt = false,
      eager = false,
      requireProtocolVersion = CLIENT_CAPABILITIES.minProtocolVersion,
      requireFeatures = [],
      legacyHandshake = false,
    } = opts;

    if (path === undefined && (host === undefined || port === undefined)) {
      throw new PowDBError(
        "connect requires either { path } (Unix socket) or { host, port } (TCP)",
        "connect_failed",
      );
    }

    const socket = await openSocket(
      { host, port, path },
      connectTimeoutMs,
      tlsOpt,
    );

    const hello: ClientHello | undefined = legacyHandshake
      ? undefined
      : {
          minProtocol: CLIENT_CAPABILITIES.minProtocolVersion,
          maxProtocol: CLIENT_CAPABILITIES.maxProtocolVersion,
          catalogVersion: CLIENT_CAPABILITIES.catalogVersion,
          features: [...CLIENT_CAPABILITIES.features],
        };

    const client = new Client(socket);
    client.startHandshake(
      { type: "Connect", dbName, password, username: user ?? null, hello },
      path ?? `${host}:${port}`,
      { requireProtocolVersion, requireFeatures },
    );
    if (!eager) {
      await client.ready();
    }
    return client;
  }

  /**
   * Resolves once the Connect→ConnectOk handshake has completed; rejects
   * with the handshake error if it failed. For non-eager clients this has
   * already settled by the time {@link connect} returns. Eager callers can
   * await it to learn the handshake outcome without issuing a query.
   */
  ready(): Promise<void> {
    return this.handshake;
  }

  /**
   * Write the Connect frame and register the handshake as the first entry
   * in the pending queue. The reply-matching machinery is strictly FIFO, so
   * the ConnectOk (or Error) frame is matched to the handshake before any
   * pipelined query sees a reply. On failure, every queued query is
   * rejected with the handshake error and the socket is torn down.
   */
  private startHandshake(
    connect: Message,
    versionWarnKey: string,
    require: {
      requireProtocolVersion: number;
      requireFeatures: readonly string[];
    },
  ): void {
    this.handshake = this.send(connect).then(
      (reply) => {
        if (reply.type === "Error") {
          // A server that refused on version grounds says so with the
          // ProtocolVersion class; keep that distinct from a bad password.
          const code =
            reply.errorClass === WIRE_ERROR_CLASS.protocol_version
              ? "protocol_version"
              : "auth_failed";
          throw new PowDBError(`connect failed: ${reply.message}`, code, {
            wireErrorClass: reply.errorClass,
          });
        }
        if (reply.type !== "ConnectOk") {
          throw new PowDBError(
            `expected ConnectOk, got ${reply.type}`,
            "protocol_error",
          );
        }
        // A server that sent no hello block is pre-0.22.0: it speaks protocol
        // v1 and names no features. Judge it on that rather than assuming.
        this._serverHello = reply.hello ?? legacyServerHello();
        const mismatch = serverCapabilityMismatch(
          this._serverHello,
          require.requireProtocolVersion,
          require.requireFeatures,
          CLIENT_CAPABILITIES.catalogVersion,
        );
        if (mismatch !== null) {
          throw new PowDBError(mismatch, "protocol_version");
        }
        this.handshakeComplete = true;
        this._serverVersion = reply.version;

        // Advisory: warn once per host:port if the server's major differs
        // from the client's. Do not throw or close — this is best-effort.
        const serverMajor = majorOf(reply.version);
        const clientMajor = majorOf(CLIENT_VERSION);
        if (serverMajor !== clientMajor) {
          if (!versionWarnings.has(versionWarnKey)) {
            versionWarnings.add(versionWarnKey);
            console.warn(
              `[powdb] server version ${reply.version} major (${serverMajor}) ` +
                `differs from client ${CLIENT_VERSION} major (${clientMajor}); ` +
                `behaviour may be inconsistent.`,
            );
          }
        }
      },
      (err) => {
        // The connection dropped before a handshake reply — surface it as a
        // connect failure (transient, retryable) rather than a bare close.
        if (isPowDBError(err) && err.code === "closed") {
          throw new PowDBError(
            "connection closed during handshake",
            "connect_failed",
            { cause: err },
          );
        }
        throw err;
      },
    );

    // On handshake failure, reject everything queued behind it and tear the
    // socket down. The extra no-op catch keeps an eager caller that never
    // awaits ready() from tripping an unhandled-rejection crash.
    this.handshake.catch((err: Error) => {
      this.onClose(err);
      this.socket.destroy();
    });
  }

  /**
   * Run a PowQL statement and return the typed result.
   *
   * When `opts.signal` is provided and fires, the returned promise rejects
   * with the signal's `reason` (or an `AbortError`). The socket is NOT
   * destroyed — the server will still eventually send its reply, which we
   * silently discard so other in-flight queries keep working.
   */
  async query(
    query: string,
    paramsOrOpts?: QueryParam[] | { signal?: AbortSignal },
    maybeOpts?: { signal?: AbortSignal },
  ): Promise<QueryResult> {
    // Disambiguate the two overloads:
    //   query(q)                       — no params, no opts
    //   query(q, opts)                 — legacy 2-arg opts form (back-compat)
    //   query(q, params)               — positional $N parameters
    //   query(q, params, opts)         — params + opts
    const hasParams = Array.isArray(paramsOrOpts);
    const params = hasParams ? (paramsOrOpts as QueryParam[]) : undefined;
    const opts = hasParams
      ? maybeOpts
      : (paramsOrOpts as { signal?: AbortSignal } | undefined);

    const start = Date.now();
    try {
      const request: Message =
        params === undefined
          ? { type: "Query", query }
          : { type: "QueryWithParams", query, params: params.map(toWireParam) };
      const reply = await this.send(request, opts);
      let result: QueryResult;
      switch (reply.type) {
        case "ResultRows":
          result = { kind: "rows", columns: reply.columns, rows: reply.rows };
          break;
        case "ResultScalar":
          result = { kind: "scalar", value: reply.value };
          break;
        case "ResultOk":
          result = { kind: "ok", affected: reply.affected };
          break;
        case "ResultMessage":
          result = { kind: "message", message: reply.message };
          break;
        case "Error":
          throw new PowDBError(
            `query failed: ${reply.message}`,
            errorCodeForWireClass(reply.errorClass),
            { wireErrorClass: reply.errorClass },
          );
        default:
          throw new PowDBError(`unexpected reply: ${reply.type}`, "protocol_error");
      }
      this.emit("query", {
        query,
        durationMs: Date.now() - start,
        ok: true,
        kind: result.kind,
      });
      return result;
    } catch (err) {
      this.emit("query", {
        query,
        durationMs: Date.now() - start,
        ok: false,
        error: err as Error,
      });
      throw err;
    }
  }

  /**
   * Run PowQL over the lossless typed wire surface. Unlike {@link query},
   * cells are not stringified: bytes remain bytes, JSON is recursive data,
   * and unsafe integers remain bigint. This method never retries as a legacy
   * query, because replaying a mutation after an ambiguous response is unsafe.
   */
  async queryNative(
    query: string,
    paramsOrOpts?: QueryParam[] | { signal?: AbortSignal },
    maybeOpts?: { signal?: AbortSignal },
  ): Promise<NativeQueryResult> {
    const hasParams = Array.isArray(paramsOrOpts);
    const params = hasParams ? (paramsOrOpts as QueryParam[]) : undefined;
    const opts = hasParams
      ? maybeOpts
      : (paramsOrOpts as { signal?: AbortSignal } | undefined);
    const start = Date.now();
    try {
      const request: Message =
        params === undefined
          ? { type: "QueryNative", query }
          : {
              type: "QueryWithParamsNative",
              query,
              params: params.map(toWireParam),
            };
      const result = nativeQueryResult(await this.send(request, opts));
      this.emit("query", {
        query,
        durationMs: Date.now() - start,
        ok: true,
        kind: result.kind,
      });
      return result;
    } catch (err) {
      this.emit("query", {
        query,
        durationMs: Date.now() - start,
        ok: false,
        error: err as Error,
      });
      throw err;
    }
  }

  /**
   * Like {@link queryNative}, but returns every cell as the raw
   * {@link WireValue} tagged union with no conversion to {@link NativeValue}.
   *
   * Reach for this only when the convenience conversion would erase something
   * you need: the raw PJ1 bytes of a JSON cell (`pj1`), or the distinction
   * between an absent value (`{ type: "empty" }`), the string `"null"`, and a
   * JSON null (all three of which {@link queryNative} maps to `null`). For
   * ordinary reads, {@link queryNative} is friendlier.
   */
  async queryNativeRaw(
    query: string,
    paramsOrOpts?: QueryParam[] | { signal?: AbortSignal },
    maybeOpts?: { signal?: AbortSignal },
  ): Promise<RawNativeQueryResult> {
    const hasParams = Array.isArray(paramsOrOpts);
    const params = hasParams ? (paramsOrOpts as QueryParam[]) : undefined;
    const opts = hasParams
      ? maybeOpts
      : (paramsOrOpts as { signal?: AbortSignal } | undefined);
    const start = Date.now();
    try {
      const request: Message =
        params === undefined
          ? { type: "QueryNative", query }
          : {
              type: "QueryWithParamsNative",
              query,
              params: params.map(toWireParam),
            };
      const result = rawNativeQueryResult(await this.send(request, opts));
      this.emit("query", {
        query,
        durationMs: Date.now() - start,
        ok: true,
        kind: result.kind,
      });
      return result;
    } catch (err) {
      this.emit("query", {
        query,
        durationMs: Date.now() - start,
        ok: false,
        error: err as Error,
      });
      throw err;
    }
  }

  /**
   * Run a SQL statement through the server-side SQL frontend. The plain
   * {@link query} method remains PowQL for wire compatibility.
   */
  async querySql(
    query: string,
    opts?: { signal?: AbortSignal },
  ): Promise<QueryResult> {
    const start = Date.now();
    try {
      const reply = await this.send({ type: "QuerySql", query }, opts);
      let result: QueryResult;
      switch (reply.type) {
        case "ResultRows":
          result = { kind: "rows", columns: reply.columns, rows: reply.rows };
          break;
        case "ResultScalar":
          result = { kind: "scalar", value: reply.value };
          break;
        case "ResultOk":
          result = { kind: "ok", affected: reply.affected };
          break;
        case "ResultMessage":
          result = { kind: "message", message: reply.message };
          break;
        case "Error":
          throw new PowDBError(
            `query failed: ${reply.message}`,
            errorCodeForWireClass(reply.errorClass),
            { wireErrorClass: reply.errorClass },
          );
        default:
          throw new PowDBError(`unexpected reply: ${reply.type}`, "protocol_error");
      }
      this.emit("query", {
        query,
        durationMs: Date.now() - start,
        ok: true,
        kind: result.kind,
      });
      return result;
    } catch (err) {
      this.emit("query", {
        query,
        durationMs: Date.now() - start,
        ok: false,
        error: err as Error,
      });
      throw err;
    }
  }

  /** Run SQL through the lossless typed wire surface without legacy replay. */
  async querySqlNative(
    query: string,
    opts?: { signal?: AbortSignal },
  ): Promise<NativeQueryResult> {
    const start = Date.now();
    try {
      const result = nativeQueryResult(
        await this.send({ type: "QuerySqlNative", query }, opts),
      );
      this.emit("query", {
        query,
        durationMs: Date.now() - start,
        ok: true,
        kind: result.kind,
      });
      return result;
    } catch (err) {
      this.emit("query", {
        query,
        durationMs: Date.now() - start,
        ok: false,
        error: err as Error,
      });
      throw err;
    }
  }

  /**
   * Fetch primary-side sync status for one embedded replica cursor.
   *
   * This speaks the private authenticated sync frame added for the embedded
   * replica product. Servers without sync support return a protocol/query
   * error; unauthenticated or readonly users are rejected server-side.
   */
  async syncStatus(
    replicaId: string,
    opts?: { signal?: AbortSignal },
  ): Promise<WireSyncStatus> {
    const start = Date.now();
    try {
      const reply = await this.send({ type: "SyncStatus", replicaId }, opts);
      if (reply.type === "Error") {
        throw new PowDBError(
          `sync status failed: ${reply.message}`,
          errorCodeForWireClass(reply.errorClass),
          { wireErrorClass: reply.errorClass },
        );
      }
      if (reply.type !== "SyncStatusResult") {
        throw new PowDBError(
          `unexpected reply: ${reply.type}`,
          "protocol_error",
        );
      }
      this.emit("sync", {
        operation: "status",
        replicaId,
        durationMs: Date.now() - start,
        ok: true,
        status: reply.status,
        stale: reply.status.stale,
        repairAction: reply.status.repairAction,
      });
      return reply.status;
    } catch (err) {
      this.emit("sync", {
        operation: "status",
        replicaId,
        durationMs: Date.now() - start,
        ok: false,
        error: err as Error,
      });
      throw err;
    }
  }

  /**
   * Pull a bounded retained-unit chunk after this replica's primary-side cursor.
   *
   * The caller supplies the database identity and format versions from the
   * sync bootstrap metadata. The server rejects mismatches and non-applyable
   * transaction cuts instead of returning ambiguous history.
   */
  async syncPull(
    request: SyncPullRequest,
    opts?: { signal?: AbortSignal },
  ): Promise<SyncPullResult> {
    const start = Date.now();
    try {
      const reply = await this.send(
        {
          type: "SyncPull",
          replicaId: request.replicaId,
          sinceLsn: toU64(request.sinceLsn, "sinceLsn"),
          maxUnits: toSyncMaxUnits(request.maxUnits),
          maxBytes: toSyncMaxBytes(request.maxBytes),
          databaseId: toDatabaseId(request.databaseId),
          primaryGeneration: toU64(
            request.primaryGeneration,
            "primaryGeneration",
          ),
          walFormatVersion: toU16(
            request.walFormatVersion,
            "walFormatVersion",
          ),
          catalogVersion: toU16(request.catalogVersion, "catalogVersion"),
          segmentFormatVersion: toU16(
            request.segmentFormatVersion,
            "segmentFormatVersion",
          ),
        },
        opts,
      );
      if (reply.type === "Error") {
        throw new PowDBError(
          `sync pull failed: ${reply.message}`,
          errorCodeForWireClass(reply.errorClass),
          { wireErrorClass: reply.errorClass },
        );
      }
      if (reply.type !== "SyncPullResult") {
        throw new PowDBError(
          `unexpected reply: ${reply.type}`,
          "protocol_error",
        );
      }
      this.emit("sync", {
        operation: "pull",
        replicaId: request.replicaId,
        durationMs: Date.now() - start,
        ok: true,
        status: reply.status,
        stale: reply.status.stale,
        repairAction: reply.status.repairAction,
        units: reply.units.length,
      });
      return {
        status: reply.status,
        units: reply.units,
        hasMore: reply.hasMore,
      };
    } catch (err) {
      this.emit("sync", {
        operation: "pull",
        replicaId: request.replicaId,
        durationMs: Date.now() - start,
        ok: false,
        error: err as Error,
      });
      throw err;
    }
  }

  /** Acknowledge that the replica applied retained history through `appliedLsn`. */
  async syncAck(
    request: SyncAckRequest,
    opts?: { signal?: AbortSignal },
  ): Promise<SyncAckResult> {
    const start = Date.now();
    try {
      const reply = await this.send(
        {
          type: "SyncAck",
          replicaId: request.replicaId,
          appliedLsn: toU64(request.appliedLsn, "appliedLsn"),
          remoteLsn: toU64(request.remoteLsn, "remoteLsn"),
        },
        opts,
      );
      if (reply.type === "Error") {
        throw new PowDBError(
          `sync ack failed: ${reply.message}`,
          errorCodeForWireClass(reply.errorClass),
          { wireErrorClass: reply.errorClass },
        );
      }
      if (reply.type !== "SyncAckResult") {
        throw new PowDBError(
          `unexpected reply: ${reply.type}`,
          "protocol_error",
        );
      }
      this.emit("sync", {
        operation: "ack",
        replicaId: request.replicaId,
        durationMs: Date.now() - start,
        ok: true,
        status: reply.status,
        stale: reply.status.stale,
        repairAction: reply.status.repairAction,
        advanced: reply.advanced,
      });
      return {
        previousAppliedLsn: reply.previousAppliedLsn,
        appliedLsn: reply.appliedLsn,
        remoteLsn: reply.remoteLsn,
        advanced: reply.advanced,
        status: reply.status,
      };
    } catch (err) {
      this.emit("sync", {
        operation: "ack",
        replicaId: request.replicaId,
        durationMs: Date.now() - start,
        ok: false,
        error: err as Error,
      });
      throw err;
    }
  }

  /**
   * Like {@link query}, but coerces string result columns to typed JS values
   * using the caller-supplied schema. See `./typed.ts` for the coercion
   * rules and supported column types.
   *
   * Returns `Row[]`, an array of objects keyed by column name (`TypedRow[]`
   * when no generic is supplied). The generic is an unchecked assertion about
   * the query's shape: the schema drives coercion, nothing validates the type.
   * Positional `$N` parameters are accepted in the same position as
   * {@link query}, so typed rows and injection-safe binding compose.
   *
   * This is the LEGACY stringly-typed path plus schema coercion. For lossless
   * object rows with no schema at all, use {@link queryObjects}.
   *
   * Throws `PowDBError(code="query_failed")` if the query is not a
   * rows-returning query.
   */
  async queryTyped<Row extends TypedRow = TypedRow>(
    query: string,
    schema: TypedSchema,
    paramsOrOpts?: QueryParam[] | { signal?: AbortSignal },
    maybeOpts?: { signal?: AbortSignal },
  ): Promise<Row[]> {
    // Same overload disambiguation as `query`, so typed rows and `$N`
    // parameter binding are usable together:
    //   queryTyped(q, schema)                  no params, no opts
    //   queryTyped(q, schema, opts)            legacy 3-arg opts form
    //   queryTyped(q, schema, params)          positional $N parameters
    //   queryTyped(q, schema, params, opts)    params plus opts
    const result = Array.isArray(paramsOrOpts)
      ? await this.query(query, paramsOrOpts, maybeOpts)
      : await this.query(query, paramsOrOpts);
    if (result.kind !== "rows") {
      throw new PowDBError(
        `queryTyped: expected rows result, got ${result.kind}`,
        "query_failed",
      );
    }
    return coerceRows(result.columns, result.rows, schema) as Row[];
  }

  /**
   * Run PowQL on the LOSSLESS native wire surface and return object rows keyed
   * by column name.
   *
   * This is {@link queryNative} plus the row-to-object step, so no schema is
   * needed: the server states each cell's type, bytes stay bytes, JSON stays
   * recursive data, and out-of-range integers stay `bigint`. Contrast with
   * {@link queryTyped}, which coerces the legacy stringly-typed path using a
   * caller-supplied schema.
   *
   *     interface User { name: string; age: number }
   *     const users = await client.queryObjects<User>(
   *       "User filter .age > $1 { .name, .age }",
   *       [25],
   *     );
   *
   * The generic is an unchecked assertion about the query's shape, exactly like
   * a SQL driver's row type: nothing validates it at runtime. Duplicate column
   * names collapse (last one wins), so alias them in the projection.
   *
   * Throws `PowDBError(code="query_failed")` if the query is not rows-returning.
   */
  async queryObjects<Row = NativeRow>(
    query: string,
    paramsOrOpts?: QueryParam[] | { signal?: AbortSignal },
    maybeOpts?: { signal?: AbortSignal },
  ): Promise<Row[]> {
    const result = Array.isArray(paramsOrOpts)
      ? await this.queryNative(query, paramsOrOpts, maybeOpts)
      : await this.queryNative(query, paramsOrOpts);
    return nativeRowObjects<Row>(result, "queryObjects");
  }

  /**
   * SQL counterpart of {@link queryObjects}: runs SQL on the lossless native
   * surface and returns object rows keyed by column name.
   *
   * The SQL frontend cannot bind parameters on any wire frame yet, so build SQL
   * with {@link escapeSqlLiteral} / {@link sqlIdent} (or the `sql` tagged
   * template) rather than string concatenation. For parameter binding, use the
   * PowQL surface, which has real `$N` placeholders.
   */
  async querySqlObjects<Row = NativeRow>(
    query: string,
    opts?: { signal?: AbortSignal },
  ): Promise<Row[]> {
    const result = await this.querySqlNative(query, opts);
    return nativeRowObjects<Row>(result, "querySqlObjects");
  }

  /**
   * Execute a multi-statement PowQL script on this connection, pipelined.
   *
   * The script is split with the same statement-aware semantics as the CLI
   * (see {@link splitStatements}: `;` inside string literals and `#`
   * comments never splits; empty statements are dropped). All statements
   * are then written down the single connection back-to-back WITHOUT
   * waiting for each reply — the server reads frames sequentially, so an
   * N-statement script costs one round trip instead of N. Results are
   * collected in statement order.
   *
   * Error handling:
   *   - Default (fail-fast): resolve with `QueryResult[]` (one entry per
   *     statement). On the first failed statement, stop dispatching further
   *     statements and reject with a {@link PowDBScriptError} carrying the
   *     failing `statementIndex`, the `statement` text, and the successful
   *     `results` so far. NOTE: because dispatch is pipelined, statements
   *     already written when the error reply arrives (typically the whole
   *     script) still execute server-side. If you need all-or-nothing
   *     behavior use `transactional: true` — do NOT embed `begin`/`commit`
   *     in the script yourself: the trailing `commit` is already on the
   *     wire when an error reply arrives, so it commits the partial work.
   *   - `transactional: true`: `execScript` opens the transaction itself,
   *     waits for every statement's reply, and only then sends `commit`
   *     (or `rollback` if any statement failed), so no statement's effect
   *     survives a failure. On failure the {@link PowDBScriptError}'s
   *     `results` are the replies received before rollback — their effects
   *     are NOT persisted.
   *   - `continueOnError: true`: dispatch every statement regardless of
   *     failures and resolve with a dense {@link ScriptStatementOutcome}
   *     array recording each statement's result or error.
   *
   * Each statement emits the usual `"query"` event.
   */
  async execScript(
    script: string,
    opts?: {
      continueOnError?: false;
      transactional?: boolean;
      signal?: AbortSignal;
    },
  ): Promise<QueryResult[]>;
  async execScript(
    script: string,
    opts: { continueOnError: true; signal?: AbortSignal },
  ): Promise<ScriptStatementOutcome[]>;
  async execScript(
    script: string,
    opts?: ExecScriptOptions,
  ): Promise<QueryResult[] | ScriptStatementOutcome[]> {
    const statements = splitStatements(script);
    const continueOnError = opts?.continueOnError === true;
    const transactional = opts?.transactional === true;
    const signal = opts?.signal;

    if (transactional && continueOnError) {
      throw new PowDBError(
        "execScript: `transactional` and `continueOnError` are mutually exclusive",
        "protocol_error",
      );
    }
    if (transactional) {
      for (let i = 0; i < statements.length; i++) {
        if (TX_CONTROL_RE.test(statements[i]!.replace(LEADING_TRIVIA_RE, ""))) {
          throw new PowDBError(
            `execScript: a transactional script may not contain its own transaction control (statement ${i + 1}: ${statements[i]!})`,
            "protocol_error",
          );
        }
      }
      // Open the transaction and WAIT for its reply before dispatching
      // anything: if `begin` fails there must be nothing else on the wire.
      // Deliberately no signal — an abort after `begin` is on the wire
      // would leave the transaction open with no rollback; an
      // already-aborted signal stops the loop below at statement 0 and
      // takes the rollback path.
      await this.query("begin");
    }

    const outcomes: (ScriptStatementOutcome | undefined)[] = new Array(
      statements.length,
    );
    const inFlight: Promise<void>[] = [];
    let firstFailureIndex = -1;
    let firstFailureError: Error | null = null;
    let dispatched = 0;

    for (let i = 0; i < statements.length; i++) {
      // Stop dispatching when the script is already doomed: fail-fast saw
      // an error, the caller aborted, or the connection is gone.
      if (this.closed || signal?.aborted) break;
      if (!continueOnError && firstFailureIndex !== -1) break;

      const statement = statements[i]!;
      const idx = i;
      dispatched++;
      // query() writes the frame synchronously before its first await, so
      // this loop puts every statement on the wire without waiting for any
      // reply; the FIFO pending queue matches replies back in order.
      inFlight.push(
        this.query(statement, signal ? { signal } : undefined).then(
          (result) => {
            outcomes[idx] = { statement, ok: true, result };
          },
          (error: unknown) => {
            const err =
              error instanceof Error ? error : new Error(String(error));
            outcomes[idx] = { statement, ok: false, error: err };
            // Replies are FIFO so failures normally arrive in statement
            // order; keep the earliest defensively regardless.
            if (firstFailureIndex === -1 || idx < firstFailureIndex) {
              firstFailureIndex = idx;
              firstFailureError = err;
            }
          },
        ),
      );
      // Backpressure: only yield when the kernel buffer is full. These
      // yields are also the windows in which an already-arrived error reply
      // or abort can stop further dispatch (the checks at the loop head).
      if (this.socket.writableNeedDrain) await this.drained();
    }

    // Every dispatched statement has a handler attached, so this never
    // rejects; it resolves once all in-flight replies have settled.
    await Promise.all(inFlight);

    if (!continueOnError) {
      if (firstFailureIndex === -1 && dispatched < statements.length) {
        // Dispatch stopped without any statement failing — e.g. a signal
        // that was already aborted before the first write. Treat the first
        // undispatched statement as the failure point.
        firstFailureIndex = dispatched;
        firstFailureError = signal?.aborted
          ? abortError(signal)
          : (this.closeError ?? new PowDBError("client is closed", "closed"));
      }
      if (firstFailureIndex !== -1) {
        if (transactional) {
          // Every dispatched reply has settled, so the connection is quiet
          // and the transaction is still open. Best-effort rollback — the
          // throw below is what callers act on either way, and if the
          // connection died the server aborts the transaction itself.
          try {
            await this.query("rollback");
          } catch {
            /* connection may already be gone */
          }
        }
        const cause = firstFailureError!;
        const results: QueryResult[] = [];
        for (let i = 0; i < firstFailureIndex; i++) {
          const o = outcomes[i];
          if (o !== undefined && o.ok) results.push(o.result);
        }
        throw new PowDBScriptError(
          `script failed at statement ${firstFailureIndex + 1}/${statements.length}: ${cause.message}`,
          isPowDBError(cause) ? cause.code : "query_failed",
          {
            statementIndex: firstFailureIndex,
            statement: statements[firstFailureIndex]!,
            results,
            cause,
          },
        );
      }
      if (transactional) {
        // Commit only after every statement's reply settled successfully —
        // this is the all-or-nothing guarantee. The commit reply is not
        // part of the returned results. No signal here either: aborting a
        // commit already on the wire buys nothing but ambiguity.
        try {
          await this.query("commit");
        } catch (err) {
          try {
            await this.query("rollback");
          } catch {
            /* connection may already be gone */
          }
          throw err;
        }
      }
      return outcomes.map(
        (o) => (o as Extract<ScriptStatementOutcome, { ok: true }>).result,
      );
    }

    // continueOnError: synthesize outcomes for statements that were never
    // dispatched (abort or connection loss) so the array stays dense.
    if (dispatched < statements.length) {
      const reason: Error = signal?.aborted
        ? abortError(signal)
        : (this.closeError ?? new PowDBError("client is closed", "closed"));
      for (let i = dispatched; i < statements.length; i++) {
        outcomes[i] = { statement: statements[i]!, ok: false, error: reason };
      }
    }
    return outcomes as ScriptStatementOutcome[];
  }

  /**
   * Poll a query on an interval and invoke `onRows` for every successful
   * run. Returns an unsubscribe function. Does NOT deduplicate results —
   * `onRows` fires every interval, even if the rows are unchanged.
   *
   * Pragmatic first-cut live-data. If a query takes longer than
   * `intervalMs`, the next tick waits for the in-flight one to finish
   * (no pile-up). Errors fire `onError` (if provided) without stopping
   * the watcher unless `stopOnError: true`.
   */
  watch(
    query: string,
    opts: {
      intervalMs: number;
      onRows: (rows: QueryResult) => void;
      onError?: (err: Error) => void;
      stopOnError?: boolean;
    },
  ): { stop: () => void } {
    if (!(opts.intervalMs > 0)) {
      throw new PowDBError(
        `watch: intervalMs must be > 0, got ${opts.intervalMs}`,
        "protocol_error",
      );
    }
    let stopped = false;
    let inFlight = false;

    const tick = async () => {
      if (stopped || inFlight) return;
      inFlight = true;
      try {
        const r = await this.query(query);
        if (!stopped) opts.onRows(r);
      } catch (err) {
        if (stopped) return;
        if (opts.onError) opts.onError(err as Error);
        if (opts.stopOnError) {
          stopped = true;
          clearInterval(handle);
          return;
        }
      } finally {
        inFlight = false;
      }
    };

    // Run immediately so callers don't wait a full interval for the first
    // emission, then every `intervalMs`.
    void tick();
    const handle: NodeJS.Timeout = setInterval(tick, opts.intervalMs);
    if (typeof handle.unref === "function") handle.unref();

    return {
      stop: () => {
        stopped = true;
        clearInterval(handle);
      },
    };
  }

  /**
   * Send Disconnect and tear down the socket. Waits for the remote FIN-ack
   * (`socket.end` callback) but falls back to `destroy()` after a bounded
   * timeout so an unresponsive peer cannot make `close()` hang forever.
   */
  async close(): Promise<void> {
    if (this.closed) {
      // Already torn down (e.g. an error in onData/onClose closed the client
      // without destroying the socket). Ensure the socket is released so a
      // post-teardown close() resolves instead of keeping the event loop
      // alive. `writableEnded` distinguishes that case from a concurrent
      // close() mid-graceful-shutdown, which must not be force-destroyed.
      if (!this.socket.destroyed && !this.socket.writableEnded) {
        this.socket.destroy();
      }
      return;
    }
    try {
      this.socket.write(encode({ type: "Disconnect" }));
    } catch {
      // socket may already be half-closed; ignore
    }
    this.closed = true;
    await new Promise<void>((resolve) => {
      let done = false;
      const finish = () => {
        if (done) return;
        done = true;
        clearTimeout(timer);
        resolve();
      };
      const timer = setTimeout(() => {
        // Peer didn't ack FIN in time — force-close so we don't hang.
        this.socket.destroy();
        finish();
      }, 5_000);
      if (typeof timer.unref === "function") timer.unref();
      this.socket.end(finish);
    });
  }

  // ───── internals ─────────────────────────────────────────────────────────

  private send(
    msg: Message,
    opts?: { signal?: AbortSignal },
  ): Promise<Message> {
    if (this.closed) {
      return Promise.reject(
        this.closeError ?? new PowDBError("client is closed", "closed"),
      );
    }

    const signal = opts?.signal;

    // Pre-check: if already aborted, reject immediately and do not enqueue.
    // This matches fetch() semantics for pre-aborted signals.
    if (signal?.aborted) {
      return Promise.reject(abortError(signal));
    }

    return new Promise((resolve, reject) => {
      const entry: Pending = {
        resolve: (m) => {
          entry.settled = true;
          resolve(m);
        },
        reject: (e) => {
          entry.settled = true;
          reject(e);
        },
        settled: false,
      };
      this.pending.push(entry);

      let onAbort: (() => void) | null = null;
      if (signal) {
        onAbort = () => {
          if (entry.settled) return;
          // Mark settled but DO NOT remove the entry from the queue — the
          // server will still send a reply, and onData drops replies for
          // already-settled entries at the head of the queue.
          entry.settled = true;
          reject(abortError(signal));
        };
        signal.addEventListener("abort", onAbort, { once: true });
        // Strip the listener once the entry resolves/rejects naturally.
        const origResolve = entry.resolve;
        const origReject = entry.reject;
        entry.resolve = (m) => {
          if (onAbort) signal.removeEventListener("abort", onAbort);
          origResolve(m);
        };
        entry.reject = (e) => {
          if (onAbort) signal.removeEventListener("abort", onAbort);
          origReject(e);
        };
      }

      this.socket.write(encode(msg), (err) => {
        if (err) {
          if (entry.settled) return;
          // Writer error — the promise will also be rejected by onClose,
          // but rejecting here gives a faster, more specific failure.
          //
          // Splicing an entry from the middle of `pending` is only safe
          // because a write-callback error implies the bytes never reached
          // the server — no reply will ever come for this slot, so FIFO
          // alignment with subsequent entries is preserved.
          const idx = this.pending.indexOf(entry);
          if (idx !== -1) this.pending.splice(idx, 1);
          entry.reject(err);
        }
      });
    });
  }

  /**
   * Resolve once the socket's write buffer has drained. Also resolves if
   * the client closes while waiting, so a dead peer cannot hang a
   * backpressured `execScript` dispatch loop forever.
   */
  private drained(): Promise<void> {
    return new Promise((resolve) => {
      if (!this.socket.writableNeedDrain || this.closed) {
        resolve();
        return;
      }
      const done = () => {
        this.socket.removeListener("drain", done);
        this.removeListener("close", done);
        resolve();
      };
      this.socket.on("drain", done);
      this.on("close", done);
    });
  }

  private onData(chunk: Buffer): void {
    // Append to the chunk queue — O(1) — and lazily concat only when
    // we actually need contiguous bytes to decode.
    this.chunks.push(chunk);
    this.totalLen += chunk.length;

    while (this.totalLen > 0) {
      // Fast path: if the first chunk already contains a full frame, we
      // can decode without concatenating.
      let view: Buffer;
      if (this.chunks.length === 1) {
        view = this.chunks[0]!;
      } else {
        // Peek at the header if we don't already have >=6 bytes up front.
        // We need up to 6 bytes to read payloadLen, then enough to hold
        // the full frame. Coalesce lazily.
        if (this.chunks[0]!.length < 6 && this.totalLen >= 6) {
          this.coalesce();
        }
        // If the first chunk still has a full frame, great. Otherwise
        // coalesce the whole queue so tryDecode sees contiguous bytes.
        const first = this.chunks[0]!;
        if (first.length >= 6) {
          const payloadLen = first.readUInt32LE(2);
          if (first.length >= 6 + payloadLen) {
            view = first;
          } else if (this.totalLen >= 6 + payloadLen) {
            this.coalesce();
            view = this.chunks[0]!;
          } else {
            // Not enough bytes yet for the full frame — wait for more data.
            break;
          }
        } else {
          // Still short of a header even after coalesce attempt above.
          break;
        }
      }

      let decoded: { msg: Message; consumed: number } | null;
      try {
        decoded = tryDecode(view);
      } catch (err) {
        this.onClose(err as Error);
        return;
      }
      if (decoded === null) break;

      // Advance past the consumed bytes without copying the trailing data.
      this.consume(decoded.consumed);

      // Handle Ping frames: auto-reply with Pong and continue decoding.
      if (decoded.msg.type === "Ping") {
        this.socket.write(encode({ type: "Pong" }));
        continue;
      }

      // Replies are strictly FIFO: this frame belongs to exactly one pending
      // entry — the head. Consume one entry per frame. If it was aborted
      // (settled), its reply is arriving now; drop the frame and keep decoding
      // rather than delivering it to a later, live query.
      const entry = this.pending.shift();
      if (entry === undefined) {
        // No pending entry at all — the server sent an unsolicited frame.
        this.onClose(
          new PowDBError("received unexpected frame from server", "protocol_error"),
        );
        return;
      }
      if (entry.settled) continue;
      entry.resolve(decoded.msg);
    }
  }

  /** Collapse the chunk queue into a single Buffer. */
  private coalesce(): void {
    if (this.chunks.length <= 1) return;
    const merged = Buffer.concat(this.chunks, this.totalLen);
    this.chunks.length = 0;
    this.chunks.push(merged);
  }

  /** Drop the first `n` bytes off the chunk queue. */
  private consume(n: number): void {
    let remaining = n;
    while (remaining > 0 && this.chunks.length > 0) {
      const head = this.chunks[0]!;
      if (head.length <= remaining) {
        remaining -= head.length;
        this.totalLen -= head.length;
        this.chunks.shift();
      } else {
        this.chunks[0] = head.subarray(remaining);
        this.totalLen -= remaining;
        remaining = 0;
      }
    }
  }

  private onClose(err: Error | null): void {
    const firstClose = !this.closed;
    if (this.closed && err === null) return;
    this.closed = true;
    let error = err ?? new PowDBError("connection closed", "closed");
    // A teardown before ConnectOk is a handshake failure: everything queued
    // (the handshake entry and any eagerly pipelined queries) rejects with
    // the handshake error. Auth/protocol errors already carry the precise
    // cause; anything else becomes a retryable connect failure.
    if (
      !this.handshakeComplete &&
      !(
        isPowDBError(error) &&
        (error.code === "auth_failed" || error.code === "protocol_error")
      )
    ) {
      error = new PowDBError(
        "connection closed during handshake",
        "connect_failed",
        { cause: err ?? undefined },
      );
    }
    this.closeError = error;
    while (this.pending.length > 0) {
      const entry = this.pending.shift()!;
      if (!entry.settled) {
        entry.reject(error);
      }
    }
    if (firstClose) {
      // Best-effort: surface the close event once. Late listeners on a
      // closed client miss it, which matches Node socket semantics.
      this.emit("close", { error: err });
    }
  }
}

interface SocketTarget {
  host?: string;
  port?: number;
  /** Unix domain socket path. When set, takes precedence over host/port. */
  path?: string;
}

function openSocket(
  target: SocketTarget,
  timeoutMs: number,
  tlsOpt: boolean | tls.ConnectionOptions,
): Promise<net.Socket> {
  return new Promise((resolve, reject) => {
    let socket: net.Socket;
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new PowDBError(`connect timeout after ${timeoutMs}ms`, "timeout"));
    }, timeoutMs);

    const onConnect = () => {
      clearTimeout(timer);
      socket.setNoDelay(true);
      // Enable keepalive on the underlying OS socket so dead peers are detected
      // even when the app is otherwise idle. (No-op for Unix sockets.)
      socket.setKeepAlive(true, 30_000);
      resolve(socket);
    };
    const onError = (err: Error) => {
      clearTimeout(timer);
      // Wrap raw socket errors in the PowDBError taxonomy — callers can
      // branch on `.code === "connect_failed"` rather than string-matching.
      reject(new PowDBError(`connect failed: ${err.message}`, "connect_failed", { cause: err }));
    };

    if (target.path !== undefined) {
      // Unix domain socket: no TLS (local, same-host), no host/port.
      socket = new net.Socket();
      socket.once("connect", onConnect);
      socket.once("error", onError);
      socket.connect(target.path);
    } else if (tlsOpt) {
      // TLS path: `tls.connect` wraps an underlying net.Socket. `secureConnect`
      // fires once the TLS handshake is complete — that is the right hook for
      // "ready to send application data".
      const tlsOptions: tls.ConnectionOptions =
        tlsOpt === true ? {} : tlsOpt;
      const tlsSock = tls.connect(target.port!, target.host!, tlsOptions);
      socket = tlsSock;
      tlsSock.once("secureConnect", onConnect);
      tlsSock.once("error", onError);
    } else {
      socket = new net.Socket();
      socket.once("connect", onConnect);
      socket.once("error", onError);
      socket.connect(target.port!, target.host!);
    }
  });
}

export { encode, tryDecode } from "./protocol.js";
export type {
  Message,
  SyncRepairAction,
  WireValue,
  WireParam,
  WireRetainedUnit,
  WireSyncStatus,
} from "./protocol.js";
export {
  MAX_PAYLOAD_SIZE,
  MAX_ROWS,
  MAX_RESULT_CELLS,
  MAX_COLUMNS,
  MAX_PARAMS,
  MAX_SYNC_UNITS,
  MAX_SYNC_PULL_UNITS,
  MAX_SYNC_PULL_BYTES,
} from "./protocol.js";

export {
  escapeLiteral,
  escapeIdent,
  ident,
  powql,
  PowqlIdent,
  escapeSqlLiteral,
  escapeSqlIdent,
  sqlIdent,
  sql,
  SqlIdent,
} from "./escape.js";

export {
  legacyServerHello,
  PROTOCOL_VERSION_LEGACY,
  PROTOCOL_VERSION_NEGOTIATED,
  WIRE_FEATURE,
} from "./protocol.js";
export type { ClientHello, ServerHello } from "./protocol.js";

export { Pool } from "./pool.js";
export type { PoolOptions } from "./pool.js";

export { splitStatements } from "./script.js";

export {
  errorCodeForWireClass,
  PowDBError,
  isPowDBError,
  PowDBScriptError,
  isPowDBScriptError,
  WIRE_ERROR_CLASS,
} from "./errors.js";
export type { PowDBErrorCode } from "./errors.js";

export {
  coerceValue,
  coerceRow,
  coerceRows,
} from "./typed.js";
export type {
  ColumnType,
  TypedSchema,
  TypedRow,
  Coerced,
  JsonValue,
} from "./typed.js";
