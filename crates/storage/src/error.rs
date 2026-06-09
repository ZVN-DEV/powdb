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
