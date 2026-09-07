//! Client-facing error rendering: which typed failures reach the client
//! verbatim, and the mapping from a typed failure to its stable wire
//! [`ErrorClass`].

use crate::protocol::{ErrorClass, Message};
use powdb_query::result::QueryError;
use powdb_storage::error::StorageErrorKind;

/// What a client is told when the real message may carry internal state.
pub(super) const REDACTED_MESSAGE: &str = "query execution error";

/// Refusals the storage engine still raises as a bare [`std::io::Error`]
/// string, so they reach the query layer as [`QueryError::StorageError`] with
/// no [`StorageErrorKind`] to decide on.
///
/// Every other variant is decided by its type (see [`is_client_derived`]).
/// This list exists only because those producers have not been typed yet: a
/// plain I/O failure and a refusal the caller can fix arrive in the same
/// variant, and a prefix is the only thing left that tells them apart. Adding
/// a `StorageError` variant for a producer is what removes an entry here.
const UNTYPED_STORAGE_SAFE_PREFIXES: &[&str] = &[
    // Identifier and schema validation (`catalog::validate_identifier`,
    // column/index/link DDL). Every one names only what the client wrote.
    "invalid ",
    "column ",
    "table ",
    "type ",
    "index ",
    "expression index ",
    "unique ",
    "cannot ",
    "duplicate ",
    "already exists",
    "no such ",
    "missing ",
    "unknown ",
    "row too large",
    "value too large",
    // Entity-link diagnostics raised by the catalog: `link '<name>' not found
    // on owner type '<T>'`, `link local key ...`, `link target key ...`,
    // `link name '<name>' collides with a column ...`.
    "link ",
    "links ",
    // The catalog's refusal of a second `begin` on the same engine.
    "explicit transaction is already active",
    // Read-only (snapshot-serving) refusal from the data-file layer.
    "cannot write: data file was opened read-only",
];

/// Build a client-facing error frame from an already-decided message and
/// class.
pub(super) fn error_response(message: impl Into<String>, class: ErrorClass) -> Message {
    Message::ErrorWithClass {
        message: message.into(),
        class,
    }
}

/// The client-facing frame for a typed query failure: the message the client
/// reads plus the class byte a driver branches on.
pub(super) fn query_error_response(e: &QueryError) -> Message {
    error_response(client_facing_message(e), classify_query_error(e))
}

/// The text a client is shown for a typed query failure.
///
/// Every failure the engine phrases from the client's own statement crosses
/// verbatim; only messages that can carry internal state (a plain I/O error,
/// corruption detail, the read-only retry sentinel) are replaced. The decision
/// is made on the variant, not on the message, so rewording a diagnostic can
/// never silently mask it — which is exactly how `column '<x>' not found`,
/// every uuid/bytes parse error, `commit` with no transaction, and a dozen
/// other everyday mistakes used to reach remote clients as
/// [`REDACTED_MESSAGE`] while an embedded caller saw the real text.
pub(super) fn client_facing_message(e: &QueryError) -> String {
    if is_client_derived(e) {
        e.to_string()
    } else {
        REDACTED_MESSAGE.to_string()
    }
}

/// Whether this failure's message is derived from the client's own statement
/// (or from configuration the operator chose), and therefore safe to forward.
///
/// The match is exhaustive with no wildcard arm: a new [`QueryError`] variant
/// does not compile until someone decides whether its text may cross the wire.
fn is_client_derived(e: &QueryError) -> bool {
    match e {
        // Phrased from the statement the client sent, the schema it just
        // named, or a budget the operator configured.
        QueryError::TableNotFound(_)
        | QueryError::ColumnNotFound { .. }
        | QueryError::TypeError(_)
        | QueryError::IndexError(_)
        | QueryError::ViewError(_)
        | QueryError::Parse(_)
        | QueryError::Execution(_)
        | QueryError::JoinLimitExceeded
        | QueryError::NestedLoopPairLimitExceeded { .. }
        | QueryError::SortLimitExceeded
        | QueryError::MemoryLimitExceeded { .. }
        | QueryError::ReadonlyMode
        | QueryError::Timeout { .. }
        | QueryError::Cancelled => true,
        // A storage refusal that kept its kind: the kind says whether the
        // message describes the caller's request or the server's disk.
        QueryError::Storage { kind, .. } => storage_message_is_client_derived(*kind),
        // No kind survived, so a caller-fixable refusal and a disk failure are
        // indistinguishable by type. Fall back to the prefix list.
        QueryError::StorageError(message) => {
            let lower = message.to_lowercase();
            UNTYPED_STORAGE_SAFE_PREFIXES
                .iter()
                .any(|prefix| lower.starts_with(prefix))
        }
        // An internal retry sentinel. Its Display is a marker string the
        // server intercepts before it can reach a client.
        QueryError::ReadonlyNeedsWrite => false,
    }
}

/// Whether a storage refusal of this kind describes the caller's own request.
fn storage_message_is_client_derived(kind: StorageErrorKind) -> bool {
    match kind {
        StorageErrorKind::UniqueConstraintViolation
        | StorageErrorKind::UniqueExpressionIndexViolation
        | StorageErrorKind::DdlInTransaction
        | StorageErrorKind::TransactionTooLarge
        | StorageErrorKind::InvalidIdentifier
        | StorageErrorKind::TableNotFound
        | StorageErrorKind::RowTooLarge
        | StorageErrorKind::ValueTooLarge => true,
        StorageErrorKind::Io
        | StorageErrorKind::CorruptData
        | StorageErrorKind::CorruptCrc
        | StorageErrorKind::WalReplay
        | StorageErrorKind::CatalogCorrupt
        | StorageErrorKind::PageCorrupt
        | StorageErrorKind::OverflowCorrupt => false,
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
        | QueryError::ViewError(_)
        | QueryError::Execution(_) => ErrorClass::Execution,
        // A storage refusal that kept its kind is classified from the kind.
        QueryError::Storage { kind, .. } => class_for_storage_kind(*kind),
        // A storage failure with no [`StorageErrorKind`] to classify on. Since
        // `From<StorageError> for io::Error` carries the typed error as the
        // source (error.rs), the only way to land here is a producer that
        // raised a bare `io::Error`: a genuine server-side fault as far as the
        // type system can tell, which is what class 0 tells a driver.
        QueryError::StorageError(_) => ErrorClass::Internal,
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
pub(super) fn class_for_storage_kind(kind: StorageErrorKind) -> ErrorClass {
    match kind {
        // A constraint rejected the write. The caller's data is the problem
        // and the caller can fix it. docs/errors.md class 8.
        StorageErrorKind::UniqueConstraintViolation
        | StorageErrorKind::UniqueExpressionIndexViolation => ErrorClass::ConstraintViolation,
        // A size budget was exceeded, with actionable guidance in the message.
        // `RowTooLarge` and `ValueTooLarge` are the caller's data being too
        // big for a documented cap, not a server fault: a driver told class 0
        // retries or pages the operator, when the fix is to shrink the row.
        // docs/errors.md class 4.
        StorageErrorKind::TransactionTooLarge
        | StorageErrorKind::RowTooLarge
        | StorageErrorKind::ValueTooLarge => ErrorClass::LimitExceeded,
        // The statement is not allowed here, or names something the caller
        // spelled wrong, and the message says what to do instead.
        // docs/errors.md class 2.
        StorageErrorKind::DdlInTransaction
        | StorageErrorKind::InvalidIdentifier
        | StorageErrorKind::TableNotFound => ErrorClass::Execution,
        // Genuine server-side faults: disk failures and corruption, which no
        // client action resolves.
        StorageErrorKind::Io
        | StorageErrorKind::CorruptData
        | StorageErrorKind::CorruptCrc
        | StorageErrorKind::WalReplay
        | StorageErrorKind::CatalogCorrupt
        | StorageErrorKind::PageCorrupt
        | StorageErrorKind::OverflowCorrupt => ErrorClass::Internal,
    }
}
