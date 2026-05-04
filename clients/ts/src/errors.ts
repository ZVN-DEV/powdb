/**
 * Structured error taxonomy for the PowDB client.
 *
 * Every error thrown by the client is a `PowDBError` with a stable `.code`
 * so callers can branch without string-matching the message. The taxonomy
 * is intentionally small — new codes are added only when a caller has a
 * legitimate reason to handle them differently.
 */

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
