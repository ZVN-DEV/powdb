/**
 * Structured error taxonomy for the PowDB client.
 *
 * Every error thrown by the client is a `PowDBError` with a stable `.code`
 * so callers can branch without string-matching the message. The taxonomy
 * is intentionally small — new codes are added only when a caller has a
 * legitimate reason to handle them differently.
 */

// Type-only import — erased at compile time, so no runtime cycle with index.
import type { QueryResult } from "./index.js";

export type PowDBErrorCode =
  /** TCP/TLS connect, DNS, connect timeout. Transient — safe to retry. */
  | "connect_failed"
  /** Server rejected the Connect handshake (bad password, unknown db). Not transient. */
  | "auth_failed"
  /** Server returned an `Error` frame in response to a query. Not transient. */
  | "query_failed"
  /** Caller's AbortSignal fired. Never retry — the caller asked to stop. */
  | "aborted"
  /** Peer sent a frame that exceeds one of the configured caps. Likely a bug/attack — do not retry. */
  | "size_exceeded"
  /** Wire protocol violation (bad framing, unknown message type, truncated payload). */
  | "protocol_error"
  /** `close()` has been called on the client or pool. */
  | "closed"
  /** An operation exceeded its configured time budget. */
  | "timeout"
  /** Type coercion on a row failed (queryTyped). */
  | "type_coercion_failed";

/**
 * All errors thrown by `@zvndev/powdb-client` are instances of this class.
 * Use `err.code` to branch; `err.cause` optionally carries the underlying
 * cause (e.g. the Node socket error).
 */
export class PowDBError extends Error {
  readonly code: PowDBErrorCode;

  constructor(message: string, code: PowDBErrorCode, options?: { cause?: unknown }) {
    // ErrorOptions.cause is supported in Node 16.9+; we require Node 18+.
    super(message, options as ErrorOptions);
    this.name = "PowDBError";
    this.code = code;
    // Preserve the prototype chain across `target: es2020` and friends.
    Object.setPrototypeOf(this, PowDBError.prototype);
  }
}

/**
 * Narrow `unknown` to a PowDBError. Useful in catch blocks where you want
 * to branch on `.code` but TypeScript sees `unknown`.
 */
export function isPowDBError(err: unknown): err is PowDBError {
  return err instanceof PowDBError;
}

/**
 * Failure of one statement inside `client.execScript(...)` (fail-fast mode).
 *
 * `code` mirrors the failing statement's error code (`"query_failed"` for a
 * server Error frame, `"aborted"` for a fired AbortSignal, ...), so the usual
 * `.code` branching keeps working; `cause` carries the underlying error.
 * `statementIndex`/`statement` identify the failing statement within the
 * split script, and `results` holds the successful results of every
 * statement before it, in order.
 */
export class PowDBScriptError extends PowDBError {
  /** Zero-based index of the failing statement within the split script. */
  readonly statementIndex: number;
  /** Text of the failing statement (as split, trimmed). */
  readonly statement: string;
  /** Results of the statements before the failing one, in order. */
  readonly results: QueryResult[];

  constructor(
    message: string,
    code: PowDBErrorCode,
    details: {
      statementIndex: number;
      statement: string;
      results: QueryResult[];
      cause?: unknown;
    },
  ) {
    super(message, code, { cause: details.cause });
    this.name = "PowDBScriptError";
    this.statementIndex = details.statementIndex;
    this.statement = details.statement;
    this.results = details.results;
    Object.setPrototypeOf(this, PowDBScriptError.prototype);
  }
}

/** Narrow `unknown` to a PowDBScriptError. */
export function isPowDBScriptError(err: unknown): err is PowDBScriptError {
  return err instanceof PowDBScriptError;
}
