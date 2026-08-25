//! Client-facing error rendering: the [`SAFE_ERROR_PREFIXES`] egress
//! allowlist and the mapping from a typed failure to its stable wire
//! [`ErrorClass`].

use crate::protocol::{ErrorClass, Message};
use powdb_query::result::QueryError;
use powdb_storage::error::StorageErrorKind;

/// Error messages that are safe to forward to the client verbatim.
pub(super) const SAFE_ERROR_PREFIXES: &[&str] = &[
    "table not found",
    // The executor's actual phrasing is `table 'X' not found`, which the
    // bare prefix above never matches — keep both so the real message
    // reaches clients.
    "table '",
    "type '",
    "column not found",
    // Lexer diagnostics (`at position N: unterminated quoted identifier`)
    // are derived purely from the client's own query text.
    "at position",
    "parse error",
    "type mismatch",
    "unknown table",
    "unknown column",
    "unknown function",
    "syntax error",
    "expected",
    "unexpected",
    "missing",
    "duplicate",
    "invalid",
    "cannot",
    "no such",
    "already exists",
    "permission denied",
    "row too large",
    "unique constraint violation",
    // The expression-index twin of the line above. Both name only what the
    // client itself declared (a column, or the indexed expression), so the
    // expression text is exactly as safe to echo as "User.email" is. Without
    // this entry the caller is told class 8 (a constraint rejected the write)
    // over a generic message that names no constraint, which is worse than
    // useless for fixing the data.
    "unique expression index violation",
    // Resource-limit errors carry actionable guidance (e.g. "add a LIMIT
    // clause") and leak no internal state, so surface them verbatim instead
    // of masking them to the generic message. See QueryError::{SortLimit,
    // JoinLimit,MemoryLimit}Exceeded in crates/query/src/result.rs.
    "sort input exceeds",
    "join result exceeds",
    "query exceeded memory budget",
    "result too large",
    // A failed covering fsync means the statement executed in memory but was
    // never made durable — the client MUST be able to distinguish this from
    // an ordinary failed query (the statement may still be visible until the
    // server restarts). The io::Error detail leaks no internal state.
    "wal durability sync failed",
    // Cooperative query cancellation. Both messages are derived purely from
    // the configured timeout / a client disconnect and leak no internal state.
    // See QueryError::{Timeout,Cancelled} in crates/query/src/result.rs.
    "query timeout after",
    "query cancelled",
    // Read-only snapshot serving refuses mutations (and reads needing a writer,
    // e.g. a stale materialized view) with an operator-facing message that names
    // the mode and the fix. It leaks no internal state.
    // See QueryError::ReadonlyMode in crates/query/src/result.rs.
    "readonly mode",
    // Entity-link diagnostics. Every one of these is derived from the client's
    // own statement plus catalog names the client just used, exactly like the
    // `table '...'` / `column not found` entries above, and every one names the
    // fix. Without these prefixes a remote client saw "query execution error"
    // for the whole link feature while an embedded caller saw the real message,
    // so a driver could not tell a typo from a server fault.
    // Covers the catalog's `link '<name>' not found on owner type '<T>'`,
    // `link '<name>' already exists on owner type '<T>'`, `link local key ...`,
    // `link target key ...`, `link name '<name>' collides with a column ...`,
    // the planner's `link path starts at unknown alias ...`, and the executor's
    // `link traversal requires ...`.
    "link ",
    "links ",
    // The executor's own phrasing for a link that was never declared
    // (`unknown link `x` on type `T``), which the bare `unknown table` /
    // `unknown column` entries above never matched.
    "unknown link",
    // The planner's correct-by-default refusal of an aggregate over a nested or
    // link projection: it names the statement the client sent and the rewrite
    // that works. See crates/query/src/planner.rs.
    "aggregates over",
    // The SQL frontend's subset walls (the table in docs/SQL.md): every one
    // is a static diagnostic naming the unsupported construct the client
    // itself wrote and the working alternative, exactly like the link
    // prefixes above. Without these a remote SQL user got the generic
    // "query execution error" for CASE, COALESCE, COUNT(DISTINCT), CAST,
    // OVER, IN, EXISTS, scalar subqueries, BETWEEN, and table constraints,
    // while an embedded caller saw the real message (docs/SQL.md used to
    // document that gap as a caveat).
    "sql ",
    // The frontend's `RETURNING currently supports only \`RETURNING *\``
    // wall, phrased from the client's own clause.
    "returning ",
];

/// Build the client-facing error frame: sanitized message plus the stable
/// 1-byte [`ErrorClass`]. The class is orthogonal to the message text: it is
/// derived from the typed error (or the call site), never from the message,
/// so sanitization to a generic string does not degrade it.
pub(super) fn error_response(message: impl Into<String>, class: ErrorClass) -> Message {
    Message::ErrorWithClass {
        message: message.into(),
        class,
    }
}

/// Map a [`QueryError`] to its stable wire [`ErrorClass`].
///
/// [`QueryError::ReadonlyNeedsWrite`] is an internal retry sentinel the
/// server intercepts before Display; if it ever reaches classification it is
/// reported as [`ErrorClass::Internal`], matching the generic message the
/// caller sends for that path.
pub(super) fn classify_query_error(e: &QueryError) -> ErrorClass {
    match e {
        QueryError::Parse(_) => ErrorClass::Parse,
        QueryError::Timeout { .. } => ErrorClass::Timeout,
        QueryError::Cancelled => ErrorClass::Cancelled,
        QueryError::ReadonlyMode => ErrorClass::ReadonlyRefused,
        QueryError::ReadonlyNeedsWrite => ErrorClass::Internal,
        QueryError::JoinLimitExceeded
        | QueryError::NestedLoopPairLimitExceeded { .. }
        | QueryError::SortLimitExceeded
        | QueryError::MemoryLimitExceeded { .. } => ErrorClass::LimitExceeded,
        QueryError::TableNotFound(_)
        | QueryError::ColumnNotFound { .. }
        | QueryError::TypeError(_)
        | QueryError::IndexError(_)
        | QueryError::ViewError(_) => ErrorClass::Execution,
        // A storage refusal that kept its kind is classified from the kind.
        QueryError::Storage { kind, .. } => class_for_storage_kind(*kind),
        // A storage failure with no [`StorageErrorKind`] to classify on. Since
        // `From<StorageError> for io::Error` carries the typed error as the
        // source (error.rs), the only way to land here is a plain I/O failure
        // (disk error, unexpected EOF): a genuine server-side fault, which is
        // exactly what class 0 tells a driver. The substring fallback that
        // used to sit here (`class_for_legacy_storage_text`) is gone with the
        // last path that stringified a refusal early; the binary-level suite
        // `wire_error_class_from_type.rs` pins every classified refusal to
        // its class end to end, so a new early-stringify path fails there.
        QueryError::StorageError(_) => ErrorClass::Internal,
        QueryError::Execution(msg) => {
            if msg.starts_with("unique constraint violation") {
                ErrorClass::ConstraintViolation
            } else if msg.starts_with("result too large") {
                ErrorClass::LimitExceeded
            } else {
                ErrorClass::Execution
            }
        }
    }
}

/// The wire [`ErrorClass`] for a storage refusal, decided by its
/// [`StorageErrorKind`].
///
/// The match is exhaustive with no wildcard arm: a new storage variant fails
/// to compile here until someone decides what a client should do about it.
/// That is the point of routing classification through the type. The class
/// byte is what a driver branches on, so defaulting a new refusal to
/// [`ErrorClass::Internal`] ("the server broke, nothing to fix on your side")
/// is a wrong answer, not a safe one.
fn class_for_storage_kind(kind: StorageErrorKind) -> ErrorClass {
    match kind {
        // A constraint rejected the write. The caller's data is the problem
        // and the caller can fix it. docs/errors.md class 8.
        StorageErrorKind::UniqueConstraintViolation
        | StorageErrorKind::UniqueExpressionIndexViolation => ErrorClass::ConstraintViolation,
        // A size budget was exceeded, with actionable guidance in the message
        // (commit more often). docs/errors.md class 4.
        StorageErrorKind::TransactionTooLarge => ErrorClass::LimitExceeded,
        // The statement is not allowed here, and the message says what to do
        // instead (commit or roll back first). docs/errors.md class 2.
        StorageErrorKind::DdlInTransaction => ErrorClass::Execution,
        // Genuine server-side faults: disk failures, corruption, and the
        // physical row/value caps, which no client action resolves.
        StorageErrorKind::Io
        | StorageErrorKind::CorruptData
        | StorageErrorKind::CorruptCrc
        | StorageErrorKind::WalReplay
        | StorageErrorKind::CatalogCorrupt
        | StorageErrorKind::PageCorrupt
        | StorageErrorKind::InvalidIdentifier
        | StorageErrorKind::RowTooLarge
        | StorageErrorKind::ValueTooLarge
        | StorageErrorKind::OverflowCorrupt => ErrorClass::Internal,
    }
}

/// Sanitize an error message before sending it to the client.
/// Known safe errors are passed through; everything else is replaced
/// with a generic message to avoid leaking internal details.
pub(super) fn sanitize_error(e: &str) -> String {
    let lower = e.to_lowercase();
    for prefix in SAFE_ERROR_PREFIXES {
        if lower.starts_with(prefix) {
            return e.to_string();
        }
    }
    "query execution error".into()
}
