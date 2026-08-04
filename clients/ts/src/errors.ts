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
  | "type_coercion_failed"
  /**
   * Client and server could not agree on a wire protocol version or feature
   * set. Raised only during the handshake, never mid-session. Not transient:
   * one side has to be upgraded (the message says which).
   */
  | "protocol_version";

/**
 * Stable wire error classes appended by 0.17+ servers to Error frames
 * (see docs/errors.md in the PowDB repo). These numeric values never change;
 * new classes are only appended. Treat unknown values as `internal`.
 */
export const WIRE_ERROR_CLASS = {
  /** Unclassified or internal server error. */
  internal: 0,
  /** The query text failed to lex or parse. */
  parse: 1,
  /** Planning or execution failed (unknown table/column, type mismatch, ...). */
  execution: 2,
  /** A time budget elapsed (query timeout, gate wait, idle timeout). */
  timeout: 3,
  /** A memory or size limit was exceeded. */
  limit_exceeded: 4,
  /** The server is read-only and the statement requires a writer. */
  readonly_refused: 5,
  /** Authentication or database selection failed at CONNECT time. */
  auth_failed: 6,
  /** Too many failed authentication attempts. */
  rate_limited: 7,
  /** A constraint (e.g. unique index) rejected the write. */
  constraint_violation: 8,
  /** Execution was cancelled cooperatively (client disconnect). */
  cancelled: 9,
  /** Wire protocol version or feature-set negotiation failed at CONNECT. */
  protocol_version: 10,
} as const;

/**
 * Map a server wire error class to the client's `PowDBErrorCode` taxonomy.
 * `undefined` (legacy server, no class byte) and unknown future bytes fall
 * back to `"query_failed"`, preserving pre-class behavior.
 */
export function errorCodeForWireClass(
  errorClass: number | undefined,
): PowDBErrorCode {
  switch (errorClass) {
    case WIRE_ERROR_CLASS.timeout:
      return "timeout";
    case WIRE_ERROR_CLASS.limit_exceeded:
      return "size_exceeded";
    case WIRE_ERROR_CLASS.auth_failed:
    case WIRE_ERROR_CLASS.rate_limited:
      return "auth_failed";
    case WIRE_ERROR_CLASS.protocol_version:
      return "protocol_version";
    default:
      return "query_failed";
  }
}

/**
 * All errors thrown by `@zvndev/powdb-client` are instances of this class.
 * Use `err.code` to branch; `err.cause` optionally carries the underlying
 * cause (e.g. the Node socket error).
 */
export class PowDBError extends Error {
  readonly code: PowDBErrorCode;
  /**
   * The raw server wire error class (see `WIRE_ERROR_CLASS`), when the error
   * came from a server Error frame that carried one. Finer-grained than
   * `code` (e.g. it distinguishes readonly refusals and constraint
   * violations); `undefined` for client-side errors and legacy servers.
   */
  readonly wireErrorClass?: number;

  constructor(
    message: string,
    code: PowDBErrorCode,
    options?: { cause?: unknown; wireErrorClass?: number },
  ) {
    // ErrorOptions.cause is supported in Node 16.9+; we require Node 18+.
    // Only forward `cause` when the caller supplied one, so a bare options
    // object never materializes an own `cause: undefined` property.
    super(
      message,
      options && "cause" in options
        ? ({ cause: options.cause } as ErrorOptions)
        : undefined,
    );
    this.name = "PowDBError";
    this.code = code;
    this.wireErrorClass = options?.wireErrorClass;
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
