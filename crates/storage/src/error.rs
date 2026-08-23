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

    /// A write would put a duplicate key into a unique column index. Raised by
    /// the insert/update preflight before anything is written, so the heap and
    /// every index are left untouched.
    #[error("unique constraint violation on {table}.{column}")]
    UniqueConstraintViolation { table: String, column: String },

    /// The same refusal for a unique *expression* index (for example a unique
    /// JSON path). Separate from [`Self::UniqueConstraintViolation`] because
    /// the offending key is an expression rather than a column, and the
    /// message names it.
    #[error("unique expression index violation on {table} ({expression})")]
    UniqueExpressionIndexViolation { table: String, expression: String },
}

/// The kind of a [`StorageError`], stripped of its payload.
///
/// [`StorageError`] can be neither `Clone` nor `PartialEq` because
/// [`StorageError::Io`] wraps [`std::io::Error`], which is neither. Callers
/// that must keep a refusal around and branch on it later carry this instead
/// of the error itself: the query layer stores it in `QueryError::Storage`
/// beside the rendered message, and the server maps it to a wire error class.
///
/// [`StorageError::kind`] matches exhaustively, so a new `StorageError`
/// variant does not compile until it has been given a kind here, and the
/// server's mapping from kind to wire class is exhaustive for the same
/// reason. That is the whole point of the type: the class of an error a
/// client acts on is decided by the compiler, not by a substring search over
/// a message that anyone may reword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageErrorKind {
    Io,
    CorruptData,
    CorruptCrc,
    WalReplay,
    CatalogCorrupt,
    PageCorrupt,
    InvalidIdentifier,
    RowTooLarge,
    ValueTooLarge,
    OverflowCorrupt,
    DdlInTransaction,
    TransactionTooLarge,
    UniqueConstraintViolation,
    UniqueExpressionIndexViolation,
}

impl StorageError {
    /// This error's [`StorageErrorKind`].
    ///
    /// The match is exhaustive on purpose: a new variant fails to compile
    /// until it is classified, which is what stops a refusal a client must act
    /// on from silently defaulting to "internal server error".
    pub fn kind(&self) -> StorageErrorKind {
        match self {
            Self::Io(_) => StorageErrorKind::Io,
            Self::CorruptData(_) => StorageErrorKind::CorruptData,
            Self::CorruptCrc { .. } => StorageErrorKind::CorruptCrc,
            Self::WalReplay(_) => StorageErrorKind::WalReplay,
            Self::CatalogCorrupt(_) => StorageErrorKind::CatalogCorrupt,
            Self::PageCorrupt(_) => StorageErrorKind::PageCorrupt,
            Self::InvalidIdentifier(_) => StorageErrorKind::InvalidIdentifier,
            Self::RowTooLarge { .. } => StorageErrorKind::RowTooLarge,
            Self::ValueTooLarge { .. } => StorageErrorKind::ValueTooLarge,
            Self::OverflowCorrupt(_) => StorageErrorKind::OverflowCorrupt,
            Self::DdlInTransaction { .. } => StorageErrorKind::DdlInTransaction,
            Self::TransactionTooLarge { .. } => StorageErrorKind::TransactionTooLarge,
            Self::UniqueConstraintViolation { .. } => StorageErrorKind::UniqueConstraintViolation,
            Self::UniqueExpressionIndexViolation { .. } => {
                StorageErrorKind::UniqueExpressionIndexViolation
            }
        }
    }

    /// Recover the kind of a refusal that crossed an `io::Result` boundary.
    ///
    /// Most of the storage engine's internals still speak `io::Result`, and a
    /// typed refusal travels through them as the `io::Error`'s inner source
    /// (`io::Error::new(kind, StorageError::...)`). Downcasting back to the
    /// real error is what lets the query layer keep the variant instead of
    /// flattening it to text. Returns `None` for a plain I/O failure or for a
    /// refusal that was raised as a bare string.
    pub fn kind_of_io_error(error: &std::io::Error) -> Option<StorageErrorKind> {
        error
            .get_ref()?
            .downcast_ref::<StorageError>()
            .map(StorageError::kind)
    }

    /// Whether `message` is the rendered form of [`Self::DdlInTransaction`].
    ///
    /// LEGACY FALLBACK. Classification is normally recovered from the variant
    /// (see [`Self::kind_of_io_error`]), but not every path that turns a
    /// storage failure into a query error carries the typed error along: some
    /// still render it with `to_string()` first, and the variant is gone by
    /// the time a wire error class is picked. For those, the text produced
    /// here is the only evidence left. Keeping the predicate in this file,
    /// beside the `#[error(...)]` string it reads, is what keeps the two in
    /// step: `rendered_messages_identify_exactly_their_own_variant` renders a
    /// real instance of every variant and fails if a reworded message stops
    /// matching, or starts matching a sibling.
    pub fn is_ddl_in_transaction_message(message: &str) -> bool {
        message.contains("inside an explicit transaction: DDL is not transactional")
    }

    /// Whether `message` is the rendered form of [`Self::TransactionTooLarge`].
    /// LEGACY FALLBACK; see [`Self::is_ddl_in_transaction_message`].
    pub fn is_transaction_too_large_message(message: &str) -> bool {
        message.contains("cannot buffer more of this transaction:")
    }

    /// Whether `message` is the rendered form of one of the unique-index
    /// refusals. LEGACY FALLBACK; see [`Self::is_ddl_in_transaction_message`].
    pub fn is_unique_violation_message(message: &str) -> bool {
        message.contains("unique constraint violation on")
            || message.contains("unique expression index violation on")
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
            StorageError::UniqueConstraintViolation {
                table: "User".into(),
                column: "email".into(),
            },
            StorageError::UniqueExpressionIndexViolation {
                table: "Doc".into(),
                expression: ".data->code".into(),
            },
        ]
    }

    /// `(is_ddl_in_transaction, is_transaction_too_large, is_unique_violation)`
    /// for a variant.
    fn expected_predicates(err: &StorageError) -> (bool, bool, bool) {
        match err {
            StorageError::DdlInTransaction { .. } => (true, false, false),
            StorageError::TransactionTooLarge { .. } => (false, true, false),
            StorageError::UniqueConstraintViolation { .. }
            | StorageError::UniqueExpressionIndexViolation { .. } => (false, false, true),
            StorageError::Io(_)
            | StorageError::CorruptData(_)
            | StorageError::CorruptCrc { .. }
            | StorageError::WalReplay(_)
            | StorageError::CatalogCorrupt(_)
            | StorageError::PageCorrupt(_)
            | StorageError::InvalidIdentifier(_)
            | StorageError::RowTooLarge { .. }
            | StorageError::ValueTooLarge { .. }
            | StorageError::OverflowCorrupt(_) => (false, false, false),
        }
    }

    #[test]
    fn rendered_messages_identify_exactly_their_own_variant() {
        for err in one_of_every_variant() {
            let rendered = err.to_string();
            let (ddl, too_large, unique) = expected_predicates(&err);
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
            assert_eq!(
                StorageError::is_unique_violation_message(&rendered),
                unique,
                "wrong unique-violation verdict for {err:?} rendered as {rendered:?}"
            );
        }
    }

    /// Every variant must map to its own kind. A `kind()` arm that returns a
    /// sibling's kind (the copy-paste failure this table exists to catch)
    /// makes two variants share one kind and fails here.
    #[test]
    fn every_variant_has_a_distinct_kind() {
        let mut seen: Vec<StorageErrorKind> = Vec::new();
        for err in one_of_every_variant() {
            let kind = err.kind();
            assert!(
                !seen.contains(&kind),
                "{err:?} reuses the kind {kind:?} of an earlier variant"
            );
            seen.push(kind);
        }
    }

    /// The variant must survive the `io::Error` the engine raises it through.
    /// This is the round trip the query layer depends on to keep a refusal's
    /// kind instead of flattening it to text.
    #[test]
    fn kinds_survive_the_io_error_round_trip() {
        for err in one_of_every_variant() {
            let expected = err.kind();
            let wrapped = std::io::Error::new(std::io::ErrorKind::InvalidInput, err);
            assert_eq!(
                StorageError::kind_of_io_error(&wrapped),
                Some(expected),
                "the kind was lost crossing an io::Error boundary"
            );
        }
    }

    /// A plain I/O failure carries no storage kind, so callers fall back to
    /// the legacy text path rather than mis-reporting one.
    #[test]
    fn a_plain_io_error_has_no_storage_kind() {
        let bare = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        assert_eq!(StorageError::kind_of_io_error(&bare), None);
        let no_source = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(StorageError::kind_of_io_error(&no_source), None);
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
