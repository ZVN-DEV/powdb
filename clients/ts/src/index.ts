/**
 * PowDB TypeScript client.
 *
 * Thin async wrapper around a TCP (or TLS) socket speaking the PowDB wire
 * protocol.
 *
 *     const client = await Client.connect({
 *       host: "213.188.194.202",
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
import { encode, tryDecode, type Message } from "./protocol.js";

/** Client library version. Compared to the server's reported version. */
export const CLIENT_VERSION = "0.2.0";

export type QueryResult =
  | { kind: "rows"; columns: string[]; rows: string[][] }
  | { kind: "scalar"; value: string }
  | { kind: "ok"; affected: bigint };

export interface ClientOptions {
  host: string;
  port: number;
  dbName?: string;
  password?: string | null;
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

/** Build an AbortError matching the DOM's `signal.reason` default. */
function abortError(signal?: AbortSignal): Error {
  if (signal && signal.reason !== undefined) {
    const r = signal.reason;
    return r instanceof Error ? r : new Error(String(r));
  }
  // Match the runtime's built-in AbortError when a reason wasn't set.
  if (typeof DOMException !== "undefined") {
    return new DOMException("The operation was aborted.", "AbortError");
  }
  const err = new Error("The operation was aborted.");
  err.name = "AbortError";
  return err;
}

export class Client {
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
      connectTimeoutMs = 5000,
      tls: tlsOpt = false,
    } = opts;

    const socket = await openSocket(host, port, connectTimeoutMs, tlsOpt);

    // We need to read the initial ConnectOk before wiring up the normal
    // pending-queue machinery, so we do a one-shot handshake here.
    const handshake = new Promise<Message>((resolve, reject) => {
      let scratch = Buffer.alloc(0);
      const onData = (chunk: Buffer) => {
        scratch = Buffer.concat([scratch, chunk]);
        let decoded: { msg: Message; consumed: number } | null;
        try {
          decoded = tryDecode(scratch);
        } catch (err) {
          socket.removeListener("data", onData);
          socket.removeListener("error", onError);
          reject(err as Error);
          return;
        }
        if (decoded !== null) {
          socket.removeListener("data", onData);
          socket.removeListener("error", onError);
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
        socket.removeListener("data", onData);
        reject(err);
      };
      socket.on("data", onData);
      socket.on("error", onError);
    });

    socket.write(encode({ type: "Connect", dbName, password }));
    const reply = await handshake;

    if (reply.type === "Error") {
      socket.destroy();
      throw new Error(`connect failed: ${reply.message}`);
    }
    if (reply.type !== "ConnectOk") {
      socket.destroy();
      throw new Error(`expected ConnectOk, got ${reply.type}`);
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
    opts?: { signal?: AbortSignal },
  ): Promise<QueryResult> {
    const reply = await this.send({ type: "Query", query }, opts);
    switch (reply.type) {
      case "ResultRows":
        return { kind: "rows", columns: reply.columns, rows: reply.rows };
      case "ResultScalar":
        return { kind: "scalar", value: reply.value };
      case "ResultOk":
        return { kind: "ok", affected: reply.affected };
      case "Error":
        throw new Error(`query failed: ${reply.message}`);
      default:
        throw new Error(`unexpected reply: ${reply.type}`);
    }
  }

  /** Send Disconnect and tear down the socket. */
  async close(): Promise<void> {
    if (this.closed) return;
    try {
      this.socket.write(encode({ type: "Disconnect" }));
    } catch {
      // socket may already be half-closed; ignore
    }
    this.closed = true;
    await new Promise<void>((resolve) => {
      this.socket.end(() => resolve());
    });
  }

  // ───── internals ─────────────────────────────────────────────────────────

  private send(
    msg: Message,
    opts?: { signal?: AbortSignal },
  ): Promise<Message> {
    if (this.closed) {
      return Promise.reject(
        this.closeError ?? new Error("client is closed"),
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
        this.onClose(new Error("received unexpected frame from server"));
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
    if (this.closed && err === null) return;
    this.closed = true;
    this.closeError = err;
    const error = err ?? new Error("connection closed");
    while (this.pending.length > 0) {
      const entry = this.pending.shift()!;
      if (!entry.settled) {
        entry.reject(error);
      }
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
      reject(new Error(`connect timeout after ${timeoutMs}ms`));
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
      reject(err);
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
export type { Message } from "./protocol.js";
export {
  MAX_PAYLOAD_SIZE,
  MAX_ROWS,
  MAX_COLUMNS,
} from "./protocol.js";
