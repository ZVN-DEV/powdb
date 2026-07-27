/// Structured error type for the storage crate.
///
/// Replaces raw `io::Error` at major public API boundaries (catalog
/// create/open/flush, WAL replay/append) while remaining backward-
/// compatible via the `From<StorageError> for io::Error` direction
/// (internal callers that still use `io::Result` can `?`-propagate).
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("corrupt data: {0}")]
    CorruptData(String),

    #[error("CRC32 mismatch: expected {expected:#010x}, got {actual:#010x}")]
    CorruptCrc { expected: u32, actual: u32 },

    #[error("WAL replay error: {0}")]
    WalReplay(String),

    #[error("catalog corrupt: {0}")]
    CatalogCorrupt(String),

    #[error("page corrupt: {0}")]
    PageCorrupt(String),

    #[error("invalid identifier: {0}")]
    InvalidIdentifier(String),

    /// An encoded row exceeds the single-page capacity. Returned cleanly at
    /// the heap insert/update boundary instead of panicking — with
    /// `panic = "abort"` a panic here would take down the whole server.
    #[error("row too large: {size} bytes exceeds max {max} bytes")]
    RowTooLarge { size: usize, max: usize },

    /// A single value exceeds `MAX_VALUE_SIZE` (the config-raisable 64MB
    /// engine limit on out-of-line values). Distinct from `RowTooLarge`,
    /// which is the physical single-page inline cap.
    #[error("value too large: {size} bytes exceeds max {max} bytes")]
    ValueTooLarge { size: usize, max: usize },

    /// An overflow chain failed to reassemble into the value the stub
    /// promised: either the whole-value CRC32 did not match (torn or
    /// cross-linked chain) or the chain length disagreed with the stub's
    /// total_len. Surfaces as a typed error at read time.
    #[error("overflow chain corrupt: {0}")]
    OverflowCorrupt(String),

    /// A DDL statement was issued inside an explicit transaction. DDL is not
    /// transactional: it unlinks files and rewrites the catalog immediately,
    /// so a later ROLLBACK cannot undo it and would silently destroy data.
    /// The statement is refused instead.
    #[error(
        "cannot run {verb} inside an explicit transaction: DDL is not transactional in PowDB, commit or roll back first"
    )]
    DdlInTransaction { verb: &'static str },

    /// A transaction buffered more unflushed heap pages than the dirty-page
    /// budget allows. The buffer cannot be spilled to disk mid-transaction
    /// without breaking ROLLBACK, so the statement is refused cleanly rather
    /// than growing until the process is OOM-killed (fatal under
    /// `panic = "abort"`).
    #[error(
        "cannot buffer more of this transaction: {pages} unflushed pages exceed the {limit_bytes} byte dirty-page budget, commit or roll back"
    )]
    TransactionTooLarge { pages: usize, limit_bytes: usize },
}

impl StorageError {
    /// Whether `message` is the rendered form of [`Self::DdlInTransaction`].
    ///
    /// The variant itself never reaches the server: storage raises it inside
    /// an `io::Error`, and the query crate stores that as
    /// `QueryError::StorageError(e.to_string())`, so by the time a wire error
    /// class is picked the only evidence left is the text produced here.
    /// Recovering it in this file, beside the `#[error(...)]` string it reads,
    /// is what keeps the two in step: `rendered_messages_identify_exactly_
    /// their_own_variant` renders a real instance of every variant and fails
    /// if a reworded message stops matching, or starts matching a sibling.
    pub fn is_ddl_in_transaction_message(message: &str) -> bool {
        message.contains("inside an explicit transaction: DDL is not transactional")
    }

    /// Whether `message` is the rendered form of [`Self::TransactionTooLarge`].
    /// See [`Self::is_ddl_in_transaction_message`] for why the classification
    /// is recovered from text rather than from the variant.
    pub fn is_transaction_too_large_message(message: &str) -> bool {
        message.contains("cannot buffer more of this transaction:")
    }
}

/// Convenience alias used throughout the storage crate.
pub type Result<T> = std::result::Result<T, StorageError>;

impl From<StorageError> for std::io::Error {
    fn from(e: StorageError) -> Self {
        match e {
            StorageError::Io(io) => io,
            other => std::io::Error::other(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One instance of every variant. The exhaustive match in
    /// [`expected_predicates`] fails to compile when a variant is added, which
    /// is the reminder to extend this list with it.
    fn one_of_every_variant() -> Vec<StorageError> {
        vec![
            StorageError::Io(std::io::Error::other("disk gone")),
            StorageError::CorruptData("row 3".into()),
            StorageError::CorruptCrc {
                expected: 1,
                actual: 2,
            },
            StorageError::WalReplay("truncated record".into()),
            StorageError::CatalogCorrupt("bad magic".into()),
            StorageError::PageCorrupt("slot past end".into()),
            StorageError::InvalidIdentifier("a b".into()),
            StorageError::RowTooLarge {
                size: 8192,
                max: 4070,
            },
            StorageError::ValueTooLarge { size: 1, max: 0 },
            StorageError::OverflowCorrupt("chain length".into()),
            StorageError::DdlInTransaction { verb: "drop" },
            StorageError::TransactionTooLarge {
                pages: 65_536,
                limit_bytes: 268_435_456,
            },
        ]
    }

    /// `(is_ddl_in_transaction, is_transaction_too_large)` for a variant.
    fn expected_predicates(err: &StorageError) -> (bool, bool) {
        match err {
            StorageError::DdlInTransaction { .. } => (true, false),
            StorageError::TransactionTooLarge { .. } => (false, true),
            StorageError::Io(_)
            | StorageError::CorruptData(_)
            | StorageError::CorruptCrc { .. }
            | StorageError::WalReplay(_)
            | StorageError::CatalogCorrupt(_)
            | StorageError::PageCorrupt(_)
            | StorageError::InvalidIdentifier(_)
            | StorageError::RowTooLarge { .. }
            | StorageError::ValueTooLarge { .. }
            | StorageError::OverflowCorrupt(_) => (false, false),
        }
    }

    #[test]
    fn rendered_messages_identify_exactly_their_own_variant() {
        for err in one_of_every_variant() {
            let rendered = err.to_string();
            let (ddl, too_large) = expected_predicates(&err);
            assert_eq!(
                StorageError::is_ddl_in_transaction_message(&rendered),
                ddl,
                "wrong DDL-in-transaction verdict for {err:?} rendered as {rendered:?}"
            );
            assert_eq!(
                StorageError::is_transaction_too_large_message(&rendered),
                too_large,
                "wrong transaction-too-large verdict for {err:?} rendered as {rendered:?}"
            );
        }
    }

    #[test]
    fn messages_survive_the_io_error_the_engine_raises_them_through() {
        // Both refusals cross the crate boundary inside an io::Error, which is
        // the form whose rendering the server actually classifies.
        let ddl = std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            StorageError::DdlInTransaction { verb: "drop" },
        );
        assert!(StorageError::is_ddl_in_transaction_message(
            &ddl.to_string()
        ));

        let too_large = std::io::Error::new(
            std::io::ErrorKind::OutOfMemory,
            StorageError::TransactionTooLarge {
                pages: 65_536,
                limit_bytes: 268_435_456,
            },
        );
        assert!(StorageError::is_transaction_too_large_message(
            &too_large.to_string()
        ));
    }
}
