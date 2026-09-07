//! An index over a `bytes` column may answer equality, not an ordered
//! comparison.
//!
//! `Btree::encode_composite_value` writes a four-byte big-endian length before
//! the bytes, so index order is length first and value order is bytewise: the
//! one-byte key `\xff` sorts before the two-byte key `\x0102` in the index and
//! after it in every scan. A range scan over that index therefore silently
//! dropped every row whose value had a different length from the bound, while
//! the same query without an index answered correctly. Until the key encoding
//! is order-preserving, the index is withdrawn from bounds on a bytes column
//! and the query falls back to the scan, which is always right.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

/// One value of every length in the fixture, so a length-ordered key and a
/// value-ordered key disagree in both directions.
const ROWS: [(i64, &str); 8] = [
    (1, r"\\x"),
    (2, r"\\x00"),
    (3, r"\\xff"),
    (4, r"\\x0000"),
    (5, r"\\x00ff10"),
    (6, r"\\x0102"),
    (7, r"\\x010203"),
    (8, r"\\x0102030405"),
];

fn engine(indexed: bool) -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required id: int, y: bytes }")
        .unwrap();
    if indexed {
        engine.execute_powql("alter T add index .y").unwrap();
    }
    for (id, value) in ROWS {
        engine
            .execute_powql(&format!("insert T {{ id := {id}, y := \"{value}\" }}"))
            .unwrap();
    }
    (dir, engine)
}

fn ids(engine: &mut Engine, query: &str) -> Vec<i64> {
    match engine.execute_powql(query) {
        Ok(QueryResult::Rows { rows, .. }) => {
            let mut out: Vec<i64> = rows
                .iter()
                .map(|row| match row[0] {
                    Value::Int(n) => n,
                    ref other => panic!("`{query}` produced {other:?}, wanted an id"),
                })
                .collect();
            out.sort_unstable();
            out
        }
        Ok(other) => panic!("`{query}` answered {other:?}"),
        Err(error) => panic!("`{query}` failed: {error}"),
    }
}

fn plan(engine: &mut Engine, query: &str) -> String {
    match engine.execute_powql(&format!("explain {query}")) {
        Ok(QueryResult::Rows { rows, .. }) => rows
            .iter()
            .map(|row| match row[0] {
                Value::Str(ref s) => s.clone(),
                ref other => panic!("explain produced {other:?}"),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Ok(other) => panic!("explain answered {other:?}"),
        Err(error) => panic!("explain failed: {error}"),
    }
}

#[test]
fn an_ordered_comparison_on_a_bytes_column_ignores_the_index() {
    let cases: [(&str, &[i64]); 4] = [
        (r#"T filter .y >= "\\x0102" { .id }"#, &[3, 6, 7, 8]),
        (r#"T filter .y > "\\x0102" { .id }"#, &[3, 7, 8]),
        (r#"T filter .y < "\\x0102" { .id }"#, &[1, 2, 4, 5]),
        (r#"T filter .y <= "\\x0102" { .id }"#, &[1, 2, 4, 5, 6]),
    ];
    let (_plain_dir, mut plain) = engine(false);
    let (_indexed_dir, mut indexed) = engine(true);
    for (query, expected) in cases {
        let without = ids(&mut plain, query);
        let with = ids(&mut indexed, query);
        assert_eq!(
            without, expected,
            "the scan itself must order bytes bytewise for `{query}`"
        );
        assert_eq!(
            with, expected,
            "`{query}` answered differently with an index on the column"
        );
        let shape = plan(&mut indexed, query);
        assert!(
            shape.contains("SeqScan"),
            "`{query}` must fall back to a scan while bytes index keys are \
             length-ordered, plan was:\n{shape}"
        );
    }
}

/// The withdrawal is about ordered comparisons only: an exact key still probes
/// the index, because the encoding is injective even though it is not
/// order-preserving.
#[test]
fn an_equality_probe_on_a_bytes_column_still_uses_the_index() {
    let (_dir, mut indexed) = engine(true);
    let query = r#"T filter .y = "\\xff" { .id }"#;
    assert_eq!(ids(&mut indexed, query), vec![3]);
    let shape = plan(&mut indexed, query);
    assert!(
        shape.contains("IndexScan"),
        "an exact bytes key must still probe the index, plan was:\n{shape}"
    );
}

/// A `uuid` column keeps its index for bounds: uuid keys are a fixed sixteen
/// bytes with no length prefix, so their key order is their value order.
#[test]
fn an_ordered_comparison_on_a_uuid_column_keeps_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type U { required id: int, u: uuid }")
        .unwrap();
    engine.execute_powql("alter U add index .u").unwrap();
    for (id, u) in [
        (1, "00000000-0000-0000-0000-000000000001"),
        (2, "550e8400-e29b-41d4-a716-446655440000"),
        (3, "ffffffff-ffff-ffff-ffff-ffffffffffff"),
    ] {
        engine
            .execute_powql(&format!("insert U {{ id := {id}, u := \"{u}\" }}"))
            .unwrap();
    }
    let query = r#"U filter .u > "550e8400-e29b-41d4-a716-446655440000" { .id }"#;
    assert_eq!(ids(&mut engine, query), vec![3]);
    let shape = plan(&mut engine, query);
    assert!(
        shape.contains("RangeScan"),
        "a uuid bound must keep the index, plan was:\n{shape}"
    );
}
