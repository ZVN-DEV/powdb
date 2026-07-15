//! End-to-end contract tests for Lane A conjunction index selection.
//!
//! These tests use only the public PowQL execution surface. The core property
//! is that a conjunction filter returns exactly the same rows whether or not
//! the driving index exists: adding an index is a pure performance change, so
//! the index-driven path (with a compiled residual recheck) and the plain
//! sequential-scan path must agree for every query shape.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|error| panic!("failed `{query}`: {error}"))
}

fn sorted_ids(result: QueryResult) -> Vec<i64> {
    let QueryResult::Rows { rows, columns } = result else {
        panic!("expected rows");
    };
    let id_col = columns
        .iter()
        .position(|c| c == "id")
        .expect("id column present");
    let mut ids: Vec<i64> = rows
        .into_iter()
        .map(|row| match &row[id_col] {
            Value::Int(id) => *id,
            other => panic!("expected int id, got {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

fn explain_text(engine: &mut Engine, query: &str) -> String {
    let QueryResult::Rows { rows, .. } = exec(engine, query) else {
        panic!("expected EXPLAIN rows");
    };
    rows.into_iter()
        .filter_map(|row| match row.into_iter().next() {
            Some(Value::Str(line)) => Some(line),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn insert_doc(engine: &mut Engine, id: i64, model_id: i64, published: bool, data: &str) {
    let escaped = data.replace('\\', "\\\\").replace('"', "\\\"");
    exec(
        engine,
        &format!(
            r#"insert Doc {{ id := {id}, model_id := {model_id}, is_published := {published}, data := "{escaped}" }}"#
        ),
    );
}

/// A CMS-shaped table: scalar columns plus a nested JSON document. Seeded with
/// present, missing, and JSON-null values on the indexed path.
fn seed_docs(engine: &mut Engine) {
    exec(
        engine,
        "type Doc { required id: int, model_id: int, is_published: bool, data: json }",
    );
    insert_doc(engine, 1, 1, true, r#"{"ns":{"value":"x"}}"#);
    insert_doc(engine, 2, 1, false, r#"{"ns":{"value":"y"}}"#);
    insert_doc(engine, 3, 2, true, r#"{"ns":{"value":"x"}}"#);
    insert_doc(engine, 4, 1, true, r#"{"ns":{"value":"x"}}"#);
    insert_doc(engine, 5, 1, true, r#"{"ns":{}}"#); // missing value
    insert_doc(engine, 6, 1, true, r#"{"ns":{"value":null}}"#); // JSON null
    insert_doc(engine, 7, 2, false, r#"{"ns":{"value":"z"}}"#);
    insert_doc(engine, 8, 1, true, r#"{"ns":{"value":"x"}}"#);
}

/// Every conjunction shape returns the identical row set before and after the
/// indexes exist. This is the property-style check that the index-driven path
/// (and its residual recheck) matches the sequential-scan path exactly.
#[test]
fn conjunction_results_match_with_and_without_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    seed_docs(&mut engine);

    let queries = [
        // eq + eq (column + column)
        r#"Doc filter .model_id = 1 and .is_published = true { .id }"#,
        // eq + range (column + column)
        r#"Doc filter .model_id = 1 and .id > 3 { .id }"#,
        // path + column mix
        r#"Doc filter .data->ns->value = "x" and .model_id = 1 { .id }"#,
        // three conjuncts across path and columns
        r#"Doc filter .model_id = 1 and .is_published = true and .data->ns->value = "x" { .id }"#,
        // JSON null / missing key on the driving path
        r#"Doc filter .data->ns->value = null and .model_id = 1 { .id }"#,
        // path-driven with a range residual
        r#"Doc filter .data->ns->value = "x" and .id > 2 { .id }"#,
    ];

    // Capture the sequential-scan answers first (no indexes exist yet).
    let unindexed: Vec<Vec<i64>> = queries
        .iter()
        .map(|q| sorted_ids(exec(&mut engine, q)))
        .collect();

    exec(&mut engine, "alter Doc add index .model_id");
    exec(&mut engine, "alter Doc add index (.data->ns->value)");

    for (query, expected) in queries.iter().zip(unindexed) {
        assert_eq!(
            sorted_ids(exec(&mut engine, query)),
            expected,
            "index-driven result diverged from the sequential scan for `{query}`"
        );
    }
}

/// EXPLAIN on the driving three-conjunct shape must show the index scan under a
/// residual Filter, not a SeqScan, once the expression index exists.
#[test]
fn explain_shows_index_scan_with_residual_filter_for_conjunction() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    seed_docs(&mut engine);

    let query =
        r#"explain Doc filter .model_id = 1 and .is_published = true and .data->ns->value = "x""#;

    // Before any index: a plain sequential scan.
    let before = explain_text(&mut engine, query);
    assert!(before.contains("SeqScan"), "{before}");
    assert!(!before.contains("ExprIndexScan"), "{before}");

    exec(&mut engine, "alter Doc add index (.data->ns->value)");

    // After: the expression index drives the scan, the rest is a residual Filter.
    let after = explain_text(&mut engine, query);
    assert!(after.contains("ExprIndexScan"), "{after}");
    assert!(after.contains("Filter"), "{after}");
    assert!(!after.contains("SeqScan"), "{after}");
    assert!(
        after.contains("path=v1:.data->\"ns\"->\"value\""),
        "residual explain should name the driving path: {after}"
    );
}

/// When a column and an expression index both apply, the eq column index and
/// the eq path index are the same tier; the first conjunct wins. Adding the
/// path index must still never change results.
#[test]
fn column_index_and_path_index_are_both_honored() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    seed_docs(&mut engine);
    exec(&mut engine, "alter Doc add index .model_id");
    exec(&mut engine, "alter Doc add index (.data->ns->value)");

    // model_id is the first conjunct, so it drives; the path is the residual.
    let model_first = explain_text(
        &mut engine,
        r#"explain Doc filter .model_id = 1 and .data->ns->value = "x""#,
    );
    assert!(
        model_first.contains("IndexScan table=Doc column=model_id"),
        "{model_first}"
    );

    // Path is the first conjunct here, so it drives instead.
    let path_first = explain_text(
        &mut engine,
        r#"explain Doc filter .data->ns->value = "x" and .model_id = 1"#,
    );
    assert!(path_first.contains("ExprIndexScan"), "{path_first}");

    assert_eq!(
        sorted_ids(exec(
            &mut engine,
            r#"Doc filter .model_id = 1 and .data->ns->value = "x" { .id }"#,
        )),
        vec![1, 4, 8],
    );
}
