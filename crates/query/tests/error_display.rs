//! Byte-exact Display regression suite for `QueryError` and `ParseError`.
//!
//! The server's egress sanitization (`SAFE_ERROR_PREFIXES` in
//! crates/server/src/handler.rs) string-matches these messages, and several
//! integration tests assert on them verbatim. Any change to a Display string
//! is therefore a wire-visible behavior change. This suite pins every variant
//! to its exact current output so refactors of the error types (e.g. the
//! thiserror migration) cannot silently reword a message.

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
