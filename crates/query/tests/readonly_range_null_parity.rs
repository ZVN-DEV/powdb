//! Read-only mirror parity for range scans over nullable columns.
//!
//! `null` is excluded from all six comparison forms (docs/POWQL.md), and the
//! answer must not depend on whether an index exists or on which executor the
//! read came through. The read-only mirror walked its own `RangeScan` arm and
//! rechecked candidate rows with `range_matches`, which ordered `Value::Empty`
//! below every value, so an upper-bound range over a nullable `str` or `bool`
//! column returned the null rows once a stored-column index existed.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQUE_DIR: AtomicU64 = AtomicU64::new(0);

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "powdb_ro_null_{tag}_{}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the unix epoch")
            .as_nanos(),
        UNIQUE_DIR.fetch_add(1, Ordering::Relaxed),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn exec(engine: &mut Engine, query: &str) {
    engine
        .execute_powql(query)
        .unwrap_or_else(|err| panic!("fixture statement `{query}` failed: {err}"));
}

/// Row ids of whatever the query returned, so read-write and read-only answers
/// compare as sets regardless of column layout.
fn ids(result: &QueryResult) -> Vec<i64> {
    match result {
        QueryResult::Rows { columns, rows } => {
            let idx = columns
                .iter()
                .position(|c| c == "id")
                .expect("query projects id");
            let mut out: Vec<i64> = rows
                .iter()
                .map(|row| match &row[idx] {
                    powdb_storage::types::Value::Int(i) => *i,
                    other => panic!("id column holds {other:?}"),
                })
                .collect();
            out.sort_unstable();
            out
        }
        QueryResult::Scalar(value) => match value {
            powdb_storage::types::Value::Int(i) => vec![*i],
            other => panic!("scalar is {other:?}"),
        },
        other => panic!("unexpected result {other:?}"),
    }
}

fn engine_with_nulls(tag: &str, indexed: bool) -> Engine {
    let dir = fresh_dir(tag);
    let mut engine = Engine::new(&dir).expect("open engine");
    exec(
        &mut engine,
        "type T { required unique id: int, s: str, b: bool }",
    );
    exec(&mut engine, "insert T { id := 1, s := \"a\", b := false }");
    exec(&mut engine, "insert T { id := 2, s := \"m\", b := true }");
    exec(&mut engine, "insert T { id := 3 }");
    exec(&mut engine, "insert T { id := 4 }");
    if indexed {
        exec(&mut engine, "alter T add index .s");
        exec(&mut engine, "alter T add index .b");
    }
    engine
}

/// Every upper-bound form over a nullable column, in both languages. Each must
/// return only the non-null rows, whatever the access path.
const UPPER_BOUND_CASES: &[(&str, &[i64])] = &[
    ("T filter .s <= \"m\" { .id }", &[1, 2]),
    ("T filter .s < \"z\" { .id }", &[1, 2]),
    ("T filter .s <= \"m\" order .id limit 10 { .id }", &[1, 2]),
    ("T filter .s <= \"m\" and .id > 0 { .id }", &[1, 2]),
    ("T filter .b < true { .id }", &[1]),
    ("T filter .b <= false { .id }", &[1]),
    ("T filter .s >= \"a\" { .id }", &[1, 2]),
    ("T filter .s > \"\" { .id }", &[1, 2]),
];

#[test]
fn readonly_range_over_nullable_column_excludes_nulls_when_indexed() {
    let mut engine = engine_with_nulls("indexed", true);
    for (query, expected) in UPPER_BOUND_CASES {
        let rw = engine.execute_powql(query).expect("read-write query");
        let ro = engine.execute_powql_readonly(query).expect("read-only query");
        assert_eq!(
            ids(&rw),
            expected.to_vec(),
            "read-write path returned the wrong rows for `{query}`"
        );
        assert_eq!(
            ids(&ro),
            expected.to_vec(),
            "read-only path returned the wrong rows for `{query}`"
        );
    }
}

#[test]
fn readonly_range_answers_match_the_unindexed_answers() {
    let indexed = engine_with_nulls("parity_idx", true);
    let mut plain = engine_with_nulls("parity_plain", false);
    for (query, _) in UPPER_BOUND_CASES {
        let control = plain.execute_powql(query).expect("control query");
        let ro = indexed
            .execute_powql_readonly(query)
            .expect("read-only query");
        assert_eq!(
            ids(&ro),
            ids(&control),
            "indexed read-only answer differs from the unindexed answer for `{query}`"
        );
    }
}

#[test]
fn readonly_count_over_nullable_range_excludes_nulls() {
    let engine = engine_with_nulls("count", true);
    for (query, expected) in [
        ("count(T filter .s <= \"m\")", 2),
        ("count(T filter .s < \"z\")", 2),
        ("count(T filter .b < true)", 1),
    ] {
        let ro = engine
            .execute_powql_readonly(query)
            .expect("read-only count");
        match ro {
            QueryResult::Scalar(powdb_storage::types::Value::Int(n)) => {
                assert_eq!(n, expected, "read-only `{query}` counted the null rows")
            }
            other => panic!("`{query}` returned {other:?}"),
        }
    }
}

#[test]
fn readonly_sql_range_over_nullable_column_excludes_nulls() {
    let engine = engine_with_nulls("sql", true);
    let ro = engine
        .execute_sql_readonly("SELECT id FROM T WHERE s <= 'm'")
        .expect("read-only sql query");
    assert_eq!(ids(&ro), vec![1, 2], "read-only SQL counted the null rows");
}
