//! End-to-end contract tests for JSON-path expression indexes.
//!
//! These tests intentionally use only the public PowQL execution surface. They
//! cover both the indexed runtime and its required sequential-scan fallback.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|error| panic!("failed `{query}`: {error}"))
}

fn insert_doc(engine: &mut Engine, id: i64, json: &str) {
    let escaped = json.replace('\\', "\\\\").replace('"', "\\\"");
    exec(
        engine,
        &format!(r#"insert Doc {{ id := {id}, data := "{escaped}" }}"#),
    );
}

fn row_ids(result: QueryResult) -> Vec<i64> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("expected rows, got {result:?}");
    };
    rows.into_iter()
        .map(|row| match row.first() {
            Some(Value::Int(id)) => *id,
            value => panic!("expected integer id, got {value:?}"),
        })
        .collect()
}

fn sorted_row_ids(result: QueryResult) -> Vec<i64> {
    let mut ids = row_ids(result);
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

fn seed_ranked_docs(engine: &mut Engine) {
    exec(engine, "type Doc { required id: int, data: json }");
    insert_doc(engine, 1, r#"{"score":20,"label":"one"}"#);
    insert_doc(engine, 2, r#"{"score":10,"label":"two"}"#);
    insert_doc(engine, 3, r#"{"score":30,"label":"three"}"#);
    insert_doc(engine, 4, r#"{"label":"missing"}"#);
    insert_doc(engine, 5, r#"{"score":null,"label":"null"}"#);
    insert_doc(engine, 6, r#"{"score":20,"label":"six"}"#);
}

#[test]
fn add_and_drop_path_index_preserve_fallback_parity_and_explain_actual_strategy() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    seed_ranked_docs(&mut engine);

    let query = "Doc filter .data->score = 20 { .id }";
    let fallback = sorted_row_ids(exec(&mut engine, query));
    assert_eq!(fallback, vec![1, 6]);

    let before = explain_text(&mut engine, &format!("explain {query}"));
    assert!(before.contains("Filter"), "{before}");
    assert!(before.contains("SeqScan"), "{before}");
    assert!(!before.contains("ExprIndexScan"), "{before}");

    exec(&mut engine, "alter Doc add index (.data->score)");
    assert_eq!(sorted_row_ids(exec(&mut engine, query)), fallback);

    let indexed = explain_text(&mut engine, &format!("explain {query}"));
    assert!(indexed.contains("ExprIndexScan"), "{indexed}");
    assert!(indexed.contains("path=v1:.data->\"score\""), "{indexed}");
    assert!(indexed.contains("index_id="), "{indexed}");
    assert!(!indexed.contains("SeqScan"), "{indexed}");

    exec(&mut engine, "alter Doc drop index (.data->score)");
    assert_eq!(sorted_row_ids(exec(&mut engine, query)), fallback);
    let after = explain_text(&mut engine, &format!("explain {query}"));
    assert!(after.contains("Filter"), "{after}");
    assert!(after.contains("SeqScan"), "{after}");
    assert!(!after.contains("ExprIndexScan"), "{after}");
}

#[test]
fn path_index_handles_exclusive_ranges_and_bounded_order_with_nulls_last() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    seed_ranked_docs(&mut engine);
    exec(&mut engine, "alter Doc add index (.data->score)");

    assert_eq!(
        sorted_row_ids(exec(
            &mut engine,
            "Doc filter .data->score > 10 and .data->score <= 20 { .id }",
        )),
        vec![1, 6]
    );
    assert_eq!(
        sorted_row_ids(exec(
            &mut engine,
            "Doc filter 10 < .data->score and 20 >= .data->score { .id }",
        )),
        vec![1, 6],
        "reversed exclusive bounds must use the same indexed semantics"
    );

    assert_eq!(
        row_ids(exec(
            &mut engine,
            "Doc order .data->score asc limit 3 offset 1 { .id }",
        )),
        vec![1, 6, 3]
    );
    assert_eq!(
        row_ids(exec(
            &mut engine,
            "Doc order .data->score desc limit 3 offset 1 { .id }",
        )),
        vec![1, 6, 2]
    );
    assert_eq!(
        row_ids(exec(
            &mut engine,
            "Doc order .data->score asc limit 6 { .id }",
        )),
        vec![2, 1, 6, 3, 4, 5],
        "missing and JSON null sort after indexed scalar values"
    );
    assert_eq!(
        row_ids(exec(
            &mut engine,
            "Doc order .data->score desc limit 6 { .id }",
        )),
        vec![3, 1, 6, 2, 4, 5],
        "missing and JSON null remain last in descending order"
    );

    let range_plan = explain_text(
        &mut engine,
        "explain Doc filter .data->score > 10 and .data->score <= 20 { .id }",
    );
    assert!(range_plan.contains("ExprRangeScan"), "{range_plan}");
    assert!(range_plan.contains("index_id="), "{range_plan}");

    let order_plan = explain_text(
        &mut engine,
        "explain Doc order .data->score desc limit 3 offset 1 { .id }",
    );
    assert!(order_plan.contains("OrderedExprIndexScan"), "{order_plan}");
    assert!(order_plan.contains("index_id="), "{order_plan}");
    assert!(order_plan.contains("descending=true"), "{order_plan}");
}

#[test]
fn path_index_reopens_and_reads_a_spilled_json_document() {
    let dir = tempfile::tempdir().unwrap();
    let body = "x".repeat(20_000);
    {
        let mut engine = Engine::new(dir.path()).unwrap();
        exec(&mut engine, "type Doc { required id: int, data: json }");
        insert_doc(
            &mut engine,
            1,
            &format!(r#"{{"author":"Aster","body":"{body}"}}"#),
        );
        insert_doc(&mut engine, 2, r#"{"author":"Birch"}"#);
        exec(&mut engine, "alter Doc add index (.data->author)");
        assert_eq!(
            row_ids(exec(
                &mut engine,
                r#"Doc filter .data->author = "Aster" { .id }"#,
            )),
            vec![1]
        );
        exec(&mut engine, "count(Doc)");
    }

    let mut reopened = Engine::new(dir.path()).unwrap();
    assert_eq!(
        row_ids(exec(
            &mut reopened,
            r#"Doc filter .data->author = "Aster" { .id }"#,
        )),
        vec![1]
    );
    let plan = explain_text(
        &mut reopened,
        r#"explain Doc filter .data->author = "Aster" { .id }"#,
    );
    assert!(plan.contains("ExprIndexScan"), "{plan}");
    assert!(plan.contains("index_id="), "{plan}");
}

#[test]
fn unique_path_index_rejects_duplicates_without_leaving_partial_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    exec(&mut engine, "type Doc { required id: int, data: json }");
    insert_doc(&mut engine, 1, r#"{"code":"a"}"#);
    insert_doc(&mut engine, 2, r#"{"code":"b"}"#);
    exec(&mut engine, "alter Doc add unique (.data->code)");

    let error = engine
        .execute_powql(r#"insert Doc { id := 3, data := "{\"code\":\"a\"}" }"#)
        .expect_err("duplicate indexed path value must fail");
    let message = error.to_string().to_ascii_lowercase();
    assert!(
        message.contains("unique") || message.contains("duplicate"),
        "{error}"
    );
    assert_eq!(
        sorted_row_ids(exec(&mut engine, "Doc { .id }")),
        vec![1, 2],
        "failed unique insertion must not write a partial heap row"
    );
}

#[test]
fn path_index_creation_rejects_existing_duplicates_and_non_scalar_nodes() {
    let duplicate_dir = tempfile::tempdir().unwrap();
    let mut duplicates = Engine::new(duplicate_dir.path()).unwrap();
    exec(&mut duplicates, "type Doc { required id: int, data: json }");
    insert_doc(&mut duplicates, 1, r#"{"code":"same"}"#);
    insert_doc(&mut duplicates, 2, r#"{"code":"same"}"#);
    let duplicate_error = duplicates
        .execute_powql("alter Doc add unique (.data->code)")
        .expect_err("building a unique path index over duplicates must fail");
    let duplicate_message = duplicate_error.to_string().to_ascii_lowercase();
    assert!(
        duplicate_message.contains("unique") || duplicate_message.contains("duplicate"),
        "{duplicate_error}"
    );
    assert_eq!(
        sorted_row_ids(exec(
            &mut duplicates,
            r#"Doc filter .data->code = "same" { .id }"#,
        )),
        vec![1, 2],
        "a rejected unique-index build must leave fallback reads correct"
    );

    let nonscalar_dir = tempfile::tempdir().unwrap();
    let mut nonscalar = Engine::new(nonscalar_dir.path()).unwrap();
    exec(&mut nonscalar, "type Doc { required id: int, data: json }");
    insert_doc(&mut nonscalar, 1, r#"{"meta":{"kind":"nested"}}"#);
    let nonscalar_error = nonscalar
        .execute_powql("alter Doc add index (.data->meta)")
        .expect_err("objects and arrays are not valid expression-index keys");
    let nonscalar_message = nonscalar_error.to_string().to_ascii_lowercase();
    assert!(
        nonscalar_message.contains("scalar")
            || nonscalar_message.contains("object")
            || nonscalar_message.contains("array"),
        "{nonscalar_error}"
    );
    assert_eq!(
        row_ids(exec(
            &mut nonscalar,
            r#"Doc filter .data->meta = "not-an-object" { .id }"#,
        )),
        Vec::<i64>::new(),
        "a rejected non-scalar index build must leave fallback reads usable"
    );
}
