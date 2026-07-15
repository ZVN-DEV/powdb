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

fn first_values(result: QueryResult) -> Vec<Value> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("expected rows, got {result:?}");
    };
    rows.into_iter()
        .map(|row| row.into_iter().next().expect("projected value"))
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
    assert_eq!(
        sorted_row_ids(
            engine
                .execute_powql_readonly("Doc filter .data->score = 20 { .id }")
                .unwrap()
        ),
        fallback,
        "the shared-reader execution path must use the same expression index"
    );
    assert_eq!(
        sorted_row_ids(exec(&mut engine, "Doc filter .data->score = null { .id }",)),
        vec![4, 5],
        "JSON null and missing paths retain existing Empty equality semantics"
    );

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
fn sql_create_index_lowers_only_direct_json_arrow_paths() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    seed_ranked_docs(&mut engine);

    engine
        .execute_sql("CREATE INDEX doc_score_idx ON Doc ((data -> 'score'))")
        .unwrap();
    assert_eq!(
        sorted_row_ids(
            engine
                .execute_sql("SELECT id FROM Doc WHERE data -> 'score' = 20")
                .unwrap()
        ),
        vec![1, 6]
    );
    let plan = explain_text(&mut engine, "explain Doc filter .data->score = 20 { .id }");
    assert!(plan.contains("ExprIndexScan"), "{plan}");
    assert!(plan.contains("index_id="), "{plan}");

    engine
        .execute_sql("CREATE UNIQUE INDEX doc_code_idx ON Doc ((data -> 'code'))")
        .unwrap();
    insert_doc(&mut engine, 7, r#"{"code":"only"}"#);
    let duplicate = engine
        .execute_powql(r#"insert Doc { id := 8, data := "{\"code\":\"only\"}" }"#)
        .expect_err("SQL-lowered unique path index must enforce uniqueness");
    let message = duplicate.to_string().to_ascii_lowercase();
    assert!(
        message.contains("unique") || message.contains("duplicate"),
        "{duplicate}"
    );

    let text_error = engine
        .execute_sql("CREATE INDEX doc_text_idx ON Doc ((data ->> 'score'))")
        .expect_err("text extraction is not a stored JSON-path identity");
    assert!(text_error.to_string().contains("->>"), "{text_error}");
    let arbitrary_error = engine
        .execute_sql("CREATE INDEX doc_math_idx ON Doc ((data + 1))")
        .expect_err("arbitrary SQL expressions remain unsupported");
    assert!(
        arbitrary_error
            .to_string()
            .contains("only direct JSON -> paths"),
        "{arbitrary_error}"
    );
}

#[test]
fn path_index_handles_exclusive_ranges_and_bounded_order_with_nulls_last() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    seed_ranked_docs(&mut engine);

    let asc_all = "Doc order .data->score asc limit 6 { score: .data->score }";
    let desc_all = "Doc order .data->score desc limit 6 { score: .data->score }";
    let asc_window = "Doc order .data->score asc limit 3 offset 1 { score: .data->score }";
    let desc_window = "Doc order .data->score desc limit 3 offset 1 { score: .data->score }";
    let asc_tie_cut = "Doc order .data->score asc limit 1 offset 1 { .id }";
    let desc_tie_cut = "Doc order .data->score desc limit 1 offset 1 { .id }";
    let fallback_asc_all = first_values(exec(&mut engine, asc_all));
    let fallback_desc_all = first_values(exec(&mut engine, desc_all));
    let fallback_asc_window = first_values(exec(&mut engine, asc_window));
    let fallback_desc_window = first_values(exec(&mut engine, desc_window));
    let fallback_asc_tie_cut = row_ids(exec(&mut engine, asc_tie_cut));
    let fallback_desc_tie_cut = row_ids(exec(&mut engine, desc_tie_cut));
    assert_eq!(
        fallback_asc_all,
        vec![
            Value::Int(10),
            Value::Int(20),
            Value::Int(20),
            Value::Int(30),
            Value::Empty,
            Value::Empty,
        ]
    );
    assert_eq!(
        fallback_desc_all,
        vec![
            Value::Int(30),
            Value::Int(20),
            Value::Int(20),
            Value::Int(10),
            Value::Empty,
            Value::Empty,
        ]
    );
    assert_eq!(
        fallback_asc_window,
        vec![Value::Int(20), Value::Int(20), Value::Int(30)]
    );
    assert_eq!(
        fallback_desc_window,
        vec![Value::Int(20), Value::Int(20), Value::Int(10)]
    );
    assert_eq!(fallback_asc_tie_cut, vec![1]);
    assert_eq!(fallback_desc_tie_cut, vec![1]);
    let fallback_plan = explain_text(&mut engine, &format!("explain {desc_window}"));
    assert!(fallback_plan.contains("Sort"), "{fallback_plan}");
    assert!(
        !fallback_plan.contains("OrderedExprIndexScan"),
        "{fallback_plan}"
    );

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
        first_values(exec(&mut engine, asc_window)),
        fallback_asc_window
    );
    assert_eq!(
        first_values(exec(&mut engine, desc_window)),
        fallback_desc_window
    );
    assert_eq!(
        first_values(engine.execute_powql_readonly(desc_window).unwrap()),
        fallback_desc_window,
        "bounded order must also run natively through the shared-reader path"
    );
    assert_eq!(
        row_ids(exec(&mut engine, asc_tie_cut)),
        fallback_asc_tie_cut,
        "ascending LIMIT/OFFSET must preserve scan order inside an equal-key group"
    );
    assert_eq!(
        row_ids(exec(&mut engine, desc_tie_cut)),
        fallback_desc_tie_cut,
        "descending LIMIT/OFFSET must preserve scan order inside an equal-key group"
    );
    assert_eq!(
        row_ids(engine.execute_powql_readonly(desc_tie_cut).unwrap()),
        fallback_desc_tie_cut,
        "readonly indexed ordering must preserve the same descending tie row"
    );
    assert_eq!(
        first_values(exec(&mut engine, asc_all)),
        fallback_asc_all,
        "missing and JSON null sort after indexed scalar values"
    );
    assert_eq!(
        first_values(exec(&mut engine, desc_all)),
        fallback_desc_all,
        "missing and JSON null remain last in descending order"
    );

    let range_plan = explain_text(
        &mut engine,
        "explain Doc filter .data->score > 10 and .data->score <= 20 { .id }",
    );
    assert!(range_plan.contains("ExprRangeScan"), "{range_plan}");
    assert!(range_plan.contains("index_id="), "{range_plan}");

    let order_plan = explain_text(&mut engine, &format!("explain {desc_window}"));
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

#[test]
fn update_changing_indexed_path_from_scalar_to_missing_moves_row_to_nulls_last() {
    // Maintaining an expression index across an update that drops the indexed
    // path must relocate the row from its scalar key into the empty set, where
    // NULLS-LAST ordering still finds it — the row is never lost.
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    exec(&mut engine, "type Doc { required id: int, data: json }");
    insert_doc(&mut engine, 1, r#"{"score":20}"#);
    insert_doc(&mut engine, 2, r#"{"score":10}"#);
    insert_doc(&mut engine, 3, r#"{"score":30}"#);
    exec(&mut engine, "alter Doc add index (.data->score)");

    // Rewrite row 2 so its indexed path is now absent.
    exec(
        &mut engine,
        r#"Doc filter .id = 2 update { data := "{\"label\":\"gone\"}" }"#,
    );

    // The row survives and sorts last under NULLS-LAST in both directions,
    // proving it moved into the index's empty set rather than being dropped.
    assert_eq!(
        sorted_row_ids(exec(&mut engine, "Doc { .id }")),
        vec![1, 2, 3],
        "the updated row must still exist"
    );
    assert_eq!(
        row_ids(exec(&mut engine, "Doc order .data->score asc { .id }",)),
        vec![1, 3, 2],
        "ascending: scalar keys first, the now-missing row last"
    );
    assert_eq!(
        row_ids(exec(&mut engine, "Doc order .data->score desc { .id }",)),
        vec![3, 1, 2],
        "descending reverses the scalar keys but keeps the missing row last"
    );
    // Equality against null/missing finds exactly the relocated row.
    assert_eq!(
        row_ids(exec(&mut engine, "Doc filter .data->score = null { .id }",)),
        vec![2],
        "the relocated row is findable via the empty-set equality path"
    );
}

#[test]
fn update_violating_unique_path_index_fails_atomically() {
    // A PowQL update that would collide on a unique expression index must fail
    // and leave both the heap row and the index untouched.
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    exec(&mut engine, "type Doc { required id: int, data: json }");
    insert_doc(&mut engine, 1, r#"{"code":"a"}"#);
    insert_doc(&mut engine, 2, r#"{"code":"b"}"#);
    exec(&mut engine, "alter Doc add unique (.data->code)");

    let error = engine
        .execute_powql(r#"Doc filter .id = 2 update { data := "{\"code\":\"a\"}" }"#)
        .expect_err("updating row 2 to a duplicate indexed key must fail");
    let message = error.to_string().to_ascii_lowercase();
    assert!(
        message.contains("unique") || message.contains("duplicate"),
        "{error}"
    );

    // The heap row is unchanged: row 2 still carries code "b".
    assert_eq!(
        row_ids(exec(&mut engine, r#"Doc filter .data->code = "b" { .id }"#,)),
        vec![2],
        "the failed update must not rewrite the row"
    );
    // The index is unchanged: "a" still maps to exactly row 1.
    assert_eq!(
        sorted_row_ids(exec(&mut engine, r#"Doc filter .data->code = "a" { .id }"#,)),
        vec![1],
        "the failed update must not leave a duplicate in the unique index"
    );
}

#[test]
fn update_writing_non_scalar_into_indexed_path_reports_scalar_key_error() {
    // Runtime index maintenance (not the build-time scan) must reject a value
    // whose indexed path resolves to an object/array, with the exact
    // storage-layer "expression index key must be scalar" message.
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    exec(&mut engine, "type Doc { required id: int, data: json }");
    insert_doc(&mut engine, 1, r#"{"code":"a"}"#);
    exec(&mut engine, "alter Doc add index (.data->code)");

    let error = engine
        .execute_powql(r#"Doc filter .id = 1 update { data := "{\"code\":{\"nested\":1}}" }"#)
        .expect_err("a non-scalar indexed key must be rejected at maintenance time");
    assert!(
        error
            .to_string()
            .contains("expression index key must be scalar"),
        "expected the storage-layer scalar-key message, got: {error}"
    );
    // The rejected update left the original scalar key intact.
    assert_eq!(
        row_ids(exec(&mut engine, r#"Doc filter .data->code = "a" { .id }"#,)),
        vec![1],
        "the failed update must not corrupt the existing indexed row"
    );
}
