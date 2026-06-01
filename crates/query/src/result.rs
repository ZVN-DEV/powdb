use std::fmt;

use powdb_storage::types::Value;

/// The result of executing a query.
#[derive(Debug)]
pub enum QueryResult {
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    Scalar(Value),   // count, avg, etc.
    Modified(u64),   // insert/update/delete — number of rows affected
    Created(String), // DDL — type name created
    Executed {
        message: String,
    }, // DDL — alter/drop feedback
}

impl QueryResult {
    pub fn row_count(&self) -> usize {
        match self {
            QueryResult::Rows { rows, .. } => rows.len(),
            QueryResult::Scalar(_) => 1,
            QueryResult::Modified(n) => *n as usize,
            QueryResult::Created(_) => 0,
            QueryResult::Executed { .. } => 0,
        }
    }
}

/// Typed error enum for query execution failures.
///
/// Replaces the previous `Result<QueryResult, String>` pattern with
/// structured variants that callers can programmatically match on.
/// The `From<String>` impl enables gradual migration — existing
/// `Err(format!(...))` sites continue to compile via `?` propagation.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryError {
    /// Table does not exist.
    TableNotFound(String),
    /// Column does not exist on table.
    ColumnNotFound { table: String, column: String },
    /// Type mismatch in expression.
    TypeError(String),
    /// Join result exceeded MAX_JOIN_ROWS.
    JoinLimitExceeded,
    /// Sort exceeded MAX_SORT_ROWS.
    SortLimitExceeded,
    /// Per-query memory budget exceeded during materialization (sort buffer,
    /// join build side, GROUP BY hash table, or IN-list). Returned cleanly so
    /// the server process is never OOM-killed by a crafted query.
    MemoryLimitExceeded {
        limit_bytes: usize,
        requested_bytes: usize,
    },
    /// Parse error (wraps parser error).
    Parse(String),
    /// Index-related error.
    IndexError(String),
    /// View-related error.
    ViewError(String),
    /// WAL or I/O error.
    StorageError(String),
    /// Readonly path needs write lock (internal sentinel).
    ReadonlyNeedsWrite,
    /// Generic execution error (catch-all for migration).
    Execution(String),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::TableNotFound(t) => write!(f, "table '{t}' not found"),
            QueryError::ColumnNotFound { table, column } => {
                if table.is_empty() {
                    write!(f, "column '{column}' not found")
                } else {
                    write!(f, "column '{column}' not found in table '{table}'")
                }
            }
            QueryError::TypeError(msg) => write!(f, "type mismatch: {msg}"),
            QueryError::JoinLimitExceeded => write!(f, "join result exceeds row limit"),
            QueryError::SortLimitExceeded => {
                write!(f, "sort input exceeds row limit — add a LIMIT clause")
            }
            QueryError::MemoryLimitExceeded {
                limit_bytes,
                requested_bytes,
            } => write!(
                f,
                "query exceeded memory budget: requested {requested_bytes} bytes, limit {limit_bytes} bytes"
            ),
            QueryError::Parse(msg) => write!(f, "{msg}"),
            QueryError::IndexError(msg) => write!(f, "{msg}"),
            QueryError::ViewError(msg) => write!(f, "{msg}"),
            QueryError::StorageError(msg) => write!(f, "{msg}"),
            QueryError::ReadonlyNeedsWrite => {
                write!(f, "__POWDB_READONLY_NEEDS_WRITE__")
            }
            QueryError::Execution(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for QueryError {}

impl From<String> for QueryError {
    fn from(s: String) -> Self {
        QueryError::Execution(s)
    }
}

impl From<&str> for QueryError {
    fn from(s: &str) -> Self {
        QueryError::Execution(s.to_string())
    }
}
