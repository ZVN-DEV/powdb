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
/// The `From<String>` impl enables gradual migration: existing
/// `Err(format!(...))` sites continue to compile via `?` propagation.
///
/// Display strings are wire-visible behavior: the server's egress
/// sanitization prefix-matches them and clients assert on them. Every
/// message is pinned byte-exact by `tests/error_display.rs`; do not
/// reword one without updating that suite deliberately.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum QueryError {
    /// Table does not exist.
    #[error("table '{0}' not found")]
    TableNotFound(String),
    /// Column does not exist on table.
    #[error("{}", column_not_found_message(table, column))]
    ColumnNotFound { table: String, column: String },
    /// Type mismatch in expression.
    #[error("type mismatch: {0}")]
    TypeError(String),
    /// Join result exceeded MAX_JOIN_ROWS.
    #[error("join result exceeds row limit")]
    JoinLimitExceeded,
    /// A fallback nested-loop join would evaluate more candidate pairs than
    /// the measured safety cap.
    #[error("{}", nested_loop_pair_limit_message(*left_rows, *right_rows, *limit))]
    NestedLoopPairLimitExceeded {
        left_rows: usize,
        right_rows: usize,
        limit: usize,
    },
    /// Sort exceeded MAX_SORT_ROWS.
    #[error("sort input exceeds row limit \u{2014} add a LIMIT clause")]
    SortLimitExceeded,
    /// Per-query memory budget exceeded during materialization (sort buffer,
    /// join build side, GROUP BY hash table, or IN-list). Returned cleanly so
    /// the server process is never OOM-killed by a crafted query.
    #[error(
        "query exceeded memory budget: requested {requested_bytes} bytes, limit {limit_bytes} bytes"
    )]
    MemoryLimitExceeded {
        limit_bytes: usize,
        requested_bytes: usize,
    },
    /// Parse error (wraps parser error).
    #[error("{0}")]
    Parse(String),
    /// Index-related error.
    #[error("{0}")]
    IndexError(String),
    /// View-related error.
    #[error("{0}")]
    ViewError(String),
    /// WAL or I/O error.
    #[error("{0}")]
    StorageError(String),
    /// Readonly path needs write lock (internal sentinel). The server
    /// intercepts this variant before Display, so the sentinel string must
    /// never cross the wire.
    #[error("__POWDB_READONLY_NEEDS_WRITE__")]
    ReadonlyNeedsWrite,
    /// The engine was opened read-only (snapshot serving) and the statement
    /// requires a writer. Unlike [`QueryError::ReadonlyNeedsWrite`], this is a
    /// terminal error with an operator-facing message: there is no writer to
    /// escalate to in this mode.
    #[error(
        "readonly mode: statement requires a writer (this database was opened read-only for snapshot serving; refresh materialized views before snapshotting a read-only directory)"
    )]
    ReadonlyMode,
    /// The per-query deadline elapsed before execution finished. Returned as a
    /// clean early-return from an unbounded executor loop so the query releases
    /// its locks instead of running to completion. `timeout_ms` is the
    /// configured per-query timeout.
    #[error("query timeout after {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },
    /// Execution was cancelled cooperatively (e.g. the issuing client
    /// disconnected). Like [`QueryError::Timeout`], a clean early-return.
    #[error("query cancelled by client disconnect")]
    Cancelled,
    /// Generic execution error (catch-all for migration).
    #[error("{0}")]
    Execution(String),
}

/// Display body for [`QueryError::ColumnNotFound`]: the table name is
/// optional, and the historical message omits the trailing clause when it
/// is empty.
fn column_not_found_message(table: &str, column: &str) -> String {
    if table.is_empty() {
        format!("column '{column}' not found")
    } else {
        format!("column '{column}' not found in table '{table}'")
    }
}

/// Display body for [`QueryError::NestedLoopPairLimitExceeded`]: the pair
/// count is reported exactly when it fits in usize and as an overflow note
/// otherwise.
fn nested_loop_pair_limit_message(left_rows: usize, right_rows: usize, limit: usize) -> String {
    match left_rows.checked_mul(right_rows) {
        Some(pairs) => format!(
            "nested-loop join would evaluate {pairs} candidate pairs, above the {limit} pair limit; add an equi-key to ON, index/filter an input, reduce the joined row counts, or raise the cap via POWDB_MAX_NESTED_LOOP_PAIRS"
        ),
        None => format!(
            "nested-loop join candidate count overflows usize ({left_rows} x {right_rows}), above the {limit} pair limit; add an equi-key to ON, index/filter an input, reduce the joined row counts, or raise the cap via POWDB_MAX_NESTED_LOOP_PAIRS"
        ),
    }
}

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
