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
  type Message,
  type WireParam,
} from "./protocol.js";
import { PowDBError } from "./errors.js";
import {
  coerceRows,
  type TypedRow,
  type TypedSchema,
} from "./typed.js";

/** Client library version. Compared to the server's reported version. */
export const CLIENT_VERSION = "0.5.1";

export type QueryResult =
  | { kind: "rows"; columns: string[]; rows: string[][] }
  | { kind: "scalar"; value: string }
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

export interface ClientOptions {
  host: string;
  port: number;
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
 * Build an AbortError. Prefers the caller-supplied `signal.reason` if it is
 * itself an `Error` (matches DOM semantics); otherwise wraps it in a
 * `PowDBError` with code `"aborted"` so catch-blocks can branch uniformly.
 */
function abortError(signal?: AbortSignal): Error {
  if (signal && signal.reason !== undefined) {
    const r = signal.reason;
    if (r instanceof Error) return r;
    return new PowDBError(String(r), "aborted");
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

  readonly serverVersion: string;

  private constructor(socket: net.Socket, serverVersion: string) {
    super();
    this.socket = socket;
    this.serverVersion = serverVersion;

    this.socket.on("data", (chunk) => this.onData(chunk));
    this.socket.on("error", (err) => this.onClose(err));
    this.socket.on("close", () => this.onClose(null));
  }

  /** Open a connection, send Connect, wait for ConnectOk. */
  static async connect(opts: ClientOptions): Promise<Client> {
    const {
      host,
      port,
      dbName = "default",
      password = null,
      user,
      connectTimeoutMs = 5000,
      tls: tlsOpt = false,
    } = opts;

    const socket = await openSocket(host, port, connectTimeoutMs, tlsOpt);

    // We need to read the initial ConnectOk before wiring up the normal
    // pending-queue machinery, so we do a one-shot handshake here.
    const handshake = new Promise<Message>((resolve, reject) => {
      let scratch = Buffer.alloc(0);
      const cleanup = () => {
        socket.removeListener("data", onData);
        socket.removeListener("error", onError);
        socket.removeListener("close", onClose);
      };
      const onData = (chunk: Buffer) => {
        scratch = Buffer.concat([scratch, chunk]);
        let decoded: { msg: Message; consumed: number } | null;
        try {
          decoded = tryDecode(scratch);
        } catch (err) {
          cleanup();
          reject(err as Error);
          return;
        }
        if (decoded !== null) {
          cleanup();
          // Any bytes past the handshake frame belong to later responses.
          // This should not happen in practice, but handle it defensively.
          const leftover = scratch.subarray(decoded.consumed);
          if (leftover.length > 0) {
            socket.unshift(leftover);
          }
          resolve(decoded.msg);
        }
      };
      const onError = (err: Error) => {
        cleanup();
        reject(err);
      };
      // Without this, a peer that FINs the connection mid-handshake (no
      // explicit error event) would leave this promise pending forever.
      const onClose = () => {
        cleanup();
        reject(new PowDBError("connection closed during handshake", "connect_failed"));
      };
      socket.on("data", onData);
      socket.on("error", onError);
      socket.on("close", onClose);
    });

    socket.write(
      encode({ type: "Connect", dbName, password, username: user ?? null }),
    );
    const reply = await handshake;

    if (reply.type === "Error") {
      socket.destroy();
      throw new PowDBError(`connect failed: ${reply.message}`, "auth_failed");
    }
    if (reply.type !== "ConnectOk") {
      socket.destroy();
      throw new PowDBError(`expected ConnectOk, got ${reply.type}`, "protocol_error");
    }

    // Advisory: warn once per host:port if the server's major differs
    // from the client's. Do not throw or close — this is best-effort.
    const serverMajor = majorOf(reply.version);
    const clientMajor = majorOf(CLIENT_VERSION);
    if (serverMajor !== clientMajor) {
      const key = `${host}:${port}`;
      if (!versionWarnings.has(key)) {
        versionWarnings.add(key);
        console.warn(
          `[powdb] server version ${reply.version} major (${serverMajor}) ` +
            `differs from client ${CLIENT_VERSION} major (${clientMajor}); ` +
            `behaviour may be inconsistent.`,
        );
      }
    }

    return new Client(socket, reply.version);
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
          throw new PowDBError(`query failed: ${reply.message}`, "query_failed");
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
          throw new PowDBError(`query failed: ${reply.message}`, "query_failed");
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
   * Like {@link query}, but coerces string result columns to typed JS values
   * using the caller-supplied schema. See `./typed.ts` for the coercion
   * rules and supported column types.
   *
   * Returns a `TypedRow[]` — an array of objects keyed by column name.
   * Throws `PowDBError(code="query_failed")` if the query is not a
   * rows-returning query.
   */
  async queryTyped(
    query: string,
    schema: TypedSchema,
    opts?: { signal?: AbortSignal },
  ): Promise<TypedRow[]> {
    const result = await this.query(query, opts);
    if (result.kind !== "rows") {
      throw new PowDBError(
        `queryTyped: expected rows result, got ${result.kind}`,
        "query_failed",
      );
    }
    return coerceRows(result.columns, result.rows, schema);
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
    if (this.closed) return;
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

      // Find the next non-settled pending entry and hand it the reply.
      // Settled entries at the head were aborted by the caller but their
      // reply is arriving now — drop it silently.
      let entry = this.pending.shift();
      while (entry && entry.settled) {
        entry = this.pending.shift();
      }
      if (!entry) {
        // Server sent an unsolicited frame (or we got extra after aborts
        // with no replacement). Treat as protocol error.
        this.onClose(
          new PowDBError("received unexpected frame from server", "protocol_error"),
        );
        return;
      }
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
    this.closeError = err;
    const error = err ?? new PowDBError("connection closed", "closed");
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

function openSocket(
  host: string,
  port: number,
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
      // Enable TCP keepalive on the underlying OS socket so dead peers are
      // detected even when the app is otherwise idle.
      socket.setKeepAlive(true, 30_000);
      resolve(socket);
    };
    const onError = (err: Error) => {
      clearTimeout(timer);
      // Wrap raw socket errors in the PowDBError taxonomy — callers can
      // branch on `.code === "connect_failed"` rather than string-matching.
      reject(new PowDBError(`connect failed: ${err.message}`, "connect_failed", { cause: err }));
    };

    if (tlsOpt) {
      // TLS path: `tls.connect` wraps an underlying net.Socket. `secureConnect`
      // fires once the TLS handshake is complete — that is the right hook for
      // "ready to send application data".
      const tlsOptions: tls.ConnectionOptions =
        tlsOpt === true ? {} : tlsOpt;
      const tlsSock = tls.connect(port, host, tlsOptions);
      socket = tlsSock;
      tlsSock.once("secureConnect", onConnect);
      tlsSock.once("error", onError);
    } else {
      socket = new net.Socket();
      socket.once("connect", onConnect);
      socket.once("error", onError);
      socket.connect(port, host);
    }
  });
}

export { encode, tryDecode } from "./protocol.js";
export type { Message, WireParam } from "./protocol.js";
export {
  MAX_PAYLOAD_SIZE,
  MAX_ROWS,
  MAX_COLUMNS,
  MAX_PARAMS,
} from "./protocol.js";

export {
  escapeLiteral,
  escapeIdent,
  ident,
  powql,
  PowqlIdent,
} from "./escape.js";

export { Pool } from "./pool.js";
export type { PoolOptions } from "./pool.js";

export { PowDBError, isPowDBError } from "./errors.js";
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
} from "./typed.js";
