//! Byte-exact Display regression suite for `QueryError` and `ParseError`.
//!
//! The server's egress sanitization (`SAFE_ERROR_PREFIXES` in
//! crates/server/src/handler/classify.rs) string-matches these messages, and several
//! integration tests assert on them verbatim. Any change to a Display string
//! is therefore a wire-visible behavior change. This suite pins every variant
//! to its exact current output so refactors of the error types (e.g. the
//! thiserror migration) cannot silently reword a message.

use powdb_query::executor::Engine;
use powdb_query::parser::ParseError;
use powdb_query::result::QueryError;

#[test]
fn query_error_display_is_byte_exact_for_every_variant() {
    let cases: Vec<(QueryError, String)> = vec![
        (
            QueryError::TableNotFound("users".into()),
            "table 'users' not found".into(),
        ),
        (
            QueryError::ColumnNotFound {
                table: String::new(),
                column: "age".into(),
            },
            "column 'age' not found".into(),
        ),
        (
            QueryError::ColumnNotFound {
                table: "users".into(),
                column: "age".into(),
            },
            "column 'age' not found in table 'users'".into(),
        ),
        (
            QueryError::TypeError("expected int, got str".into()),
            "type mismatch: expected int, got str".into(),
        ),
        (
            QueryError::JoinLimitExceeded,
            "join result exceeds row limit".into(),
        ),
        (
            QueryError::NestedLoopPairLimitExceeded {
                left_rows: 3,
                right_rows: 4,
                limit: 10,
            },
            "nested-loop join would evaluate 12 candidate pairs, above the 10 pair limit; \
             add an equi-key to ON, index/filter an input, reduce the joined row counts, \
             or raise the cap via POWDB_MAX_NESTED_LOOP_PAIRS"
                .into(),
        ),
        (
            QueryError::NestedLoopPairLimitExceeded {
                left_rows: usize::MAX,
                right_rows: 2,
                limit: 10,
            },
            format!(
                "nested-loop join candidate count overflows usize ({} x 2), above the 10 pair \
                 limit; add an equi-key to ON, index/filter an input, reduce the joined row \
                 counts, or raise the cap via POWDB_MAX_NESTED_LOOP_PAIRS",
                usize::MAX
            ),
        ),
        (
            QueryError::SortLimitExceeded,
            "sort input exceeds row limit \u{2014} add a LIMIT clause".into(),
        ),
        (
            QueryError::MemoryLimitExceeded {
                limit_bytes: 1024,
                requested_bytes: 2048,
            },
            "query exceeded memory budget: requested 2048 bytes, limit 1024 bytes".into(),
        ),
        (
            QueryError::Parse("expected identifier".into()),
            "expected identifier".into(),
        ),
        (
            QueryError::IndexError("index 'idx' already exists".into()),
            "index 'idx' already exists".into(),
        ),
        (
            QueryError::ViewError("view 'v' not found".into()),
            "view 'v' not found".into(),
        ),
        (
            QueryError::StorageError("wal append failed".into()),
            "wal append failed".into(),
        ),
        (
            QueryError::ReadonlyNeedsWrite,
            "__POWDB_READONLY_NEEDS_WRITE__".into(),
        ),
        (
            QueryError::ReadonlyMode,
            "readonly mode: statement requires a writer (this database was opened read-only \
             for snapshot serving; refresh materialized views before snapshotting a read-only \
             directory)"
                .into(),
        ),
        (
            QueryError::Timeout { timeout_ms: 2000 },
            "query timeout after 2000ms".into(),
        ),
        (
            QueryError::Cancelled,
            "query cancelled by client disconnect".into(),
        ),
        (
            QueryError::Execution("unique constraint violation on User.email".into()),
            "unique constraint violation on User.email".into(),
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(error.to_string(), expected, "variant: {error:?}");
    }
}

/// Keeps the name of the test above honest.
///
/// That test walks a hand-written `Vec`, so adding a `QueryError` variant does
/// not make it fail to compile: `QueryError::Storage` was added to the enum
/// while a test claiming to cover "every variant" stayed green and silently
/// stopped covering everything. This match has no wildcard arm, so a new
/// variant breaks the build here and whoever adds it is told to pin its
/// rendered text above.
#[test]
fn every_query_error_variant_is_named_here() {
    fn discriminant_name(error: &QueryError) -> &'static str {
        match error {
            QueryError::TableNotFound(_) => "TableNotFound",
            QueryError::ColumnNotFound { .. } => "ColumnNotFound",
            QueryError::TypeError(_) => "TypeError",
            QueryError::JoinLimitExceeded => "JoinLimitExceeded",
            QueryError::NestedLoopPairLimitExceeded { .. } => "NestedLoopPairLimitExceeded",
            QueryError::SortLimitExceeded => "SortLimitExceeded",
            QueryError::MemoryLimitExceeded { .. } => "MemoryLimitExceeded",
            QueryError::Parse(_) => "Parse",
            QueryError::IndexError(_) => "IndexError",
            QueryError::ViewError(_) => "ViewError",
            QueryError::StorageError(_) => "StorageError",
            QueryError::Storage { .. } => "Storage",
            QueryError::ReadonlyNeedsWrite => "ReadonlyNeedsWrite",
            QueryError::ReadonlyMode => "ReadonlyMode",
            QueryError::Timeout { .. } => "Timeout",
            QueryError::Cancelled => "Cancelled",
            QueryError::Execution(_) => "Execution",
        }
    }

    assert_eq!(
        discriminant_name(&QueryError::Cancelled),
        "Cancelled",
        "the guard must actually run, not merely compile"
    );
}

/// The two messages the `sum` aggregate paths compose are wire-visible for the
/// same reason the variants above are: the server's egress allowlist forwards
/// anything starting with `cannot` or `type mismatch` verbatim, so clients see
/// these strings.
///
/// This runs real queries through the engine rather than constructing the
/// variants by hand. A hand-built `QueryError::TypeError("<literal>")` asserted
/// against `"type mismatch: " + <the same literal>` only re-states the Display
/// impl: it keeps passing when the producing function rewords its message, so
/// it pins nothing about what a client actually receives. Going through
/// `execute_powql` makes the assertion fail when `agg_overflow_error` or
/// `non_numeric_agg_error` (crates/query/src/executor/plan_exec/aggregate.rs)
/// drifts, which is the whole point of a byte-exact suite.
#[test]
fn aggregate_error_display_is_byte_exact() {
    // A recycled pid would otherwise reopen a previous run's directory, where
    // the `type Agg` below already exists.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("powdb_errdisplay_{}_{unique}", std::process::id()));
    let mut engine = Engine::new(&dir).expect("engine");
    engine
        .execute_powql("type Agg { required id: int, required n: int, required label: str }")
        .expect("ddl");
    for (id, n) in [(1, i64::MAX), (2, i64::MAX)] {
        engine
            .execute_powql(&format!(
                "insert Agg {{ id := {id}, n := {n}, label := \"x\" }}"
            ))
            .expect("insert");
    }
    // An i64 total that leaves the range: every value was a well typed int, so
    // the message must report the overflow without claiming a type mismatch.
    assert_eq!(
        engine
            .execute_powql("sum(Agg { .n })")
            .expect_err("an overflowing total must be refused")
            .to_string(),
        "cannot compute sum: the integer total overflows int64"
    );
    // A str argument really is a type mismatch and keeps that prefix.
    assert_eq!(
        engine
            .execute_powql("sum(Agg { .label })")
            .expect_err("a str argument must be refused")
            .to_string(),
        "type mismatch: sum requires a numeric argument, but a str value was aggregated"
    );
}

#[test]
fn parse_error_display_is_byte_exact_for_every_variant() {
    let cases: Vec<(ParseError, String)> = vec![
        (
            ParseError::Lex {
                message: "unterminated quoted identifier".into(),
                position: 7,
            },
            "at position 7: unterminated quoted identifier".into(),
        ),
        (
            ParseError::UnexpectedToken {
                expected: "identifier".into(),
                got: "'}'".into(),
            },
            "expected identifier, got '}'".into(),
        ),
        (
            ParseError::NestingDepthExceeded { max: 64 },
            "query nesting depth exceeds maximum of 64".into(),
        ),
        (
            ParseError::Unsupported {
                feature: "window functions are not supported".into(),
            },
            "window functions are not supported".into(),
        ),
        (
            ParseError::Syntax {
                message: "trailing comma in projection".into(),
            },
            "trailing comma in projection".into(),
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(error.to_string(), expected, "variant: {error:?}");
    }
}

#[test]
fn parse_error_message_matches_display() {
    let error = ParseError::Syntax {
        message: "bad".into(),
    };
    assert_eq!(error.message(), error.to_string());
}
