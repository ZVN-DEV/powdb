//! Equality between a json column and a string literal.
//!
//! Until v0.27 `Value` equality's strict typing made `filter .j = "{}"`
//! false for every row: no error, no match, recorded in the oracle ledger as
//! `json-column-never-equals-a-string-literal`. A string literal compared
//! against a json column is now parsed and canonicalized exactly as it would
//! be on insert (`eval::coerce_value`), so the comparison asks "is this the
//! same document", and a literal that is not JSON at all is a typed error
//! instead of a silent empty result. Ordered comparisons (`<`, `>`, ...)
//! stay strictly typed; documents have no meaningful order against text.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_json_str_eq_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn engine(name: &str) -> Engine {
    let mut engine = Engine::new(&temp_dir(name)).unwrap();
    engine
        .execute_powql("type Post { id: int, data: json }")
        .unwrap();
    for insert in [
        r#"insert Post { id := 1, data := "{\"a\":1,\"b\":2}" }"#,
        r#"insert Post { id := 2, data := "{}" }"#,
        r#"insert Post { id := 3, data := "[1,2,3]" }"#,
    ] {
        engine.execute_powql(insert).unwrap();
    }
    engine
}

fn ids(result: QueryResult) -> Vec<i64> {
    match result {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|row| match &row[0] {
                Value::Int(id) => *id,
                other => panic!("expected int id, got {other:?}"),
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn a_canonical_text_literal_matches_the_document() {
    let mut engine = engine("canonical");
    let got = ids(engine
        .execute_powql(r#"Post filter .data = "{\"a\":1,\"b\":2}" { .id }"#)
        .unwrap());
    assert_eq!(got, vec![1]);
}

#[test]
fn equality_is_document_level_not_text_level() {
    let mut engine = engine("doc_level");
    // Different key order and whitespace, same document.
    let got = ids(engine
        .execute_powql(r#"Post filter .data = "{ \"b\": 2, \"a\": 1 }" { .id }"#)
        .unwrap());
    assert_eq!(got, vec![1], "the literal is parsed, not text-compared");
}

#[test]
fn empty_object_and_array_literals_match() {
    let mut engine = engine("containers");
    assert_eq!(
        ids(engine
            .execute_powql(r#"Post filter .data = "{}" { .id }"#)
            .unwrap()),
        vec![2]
    );
    assert_eq!(
        ids(engine
            .execute_powql(r#"Post filter .data = "[1,2,3]" { .id }"#)
            .unwrap()),
        vec![3]
    );
}

#[test]
fn not_equal_is_the_two_valued_complement() {
    let mut engine = engine("neq");
    let mut got = ids(engine
        .execute_powql(r#"Post filter .data != "{}" { .id }"#)
        .unwrap());
    got.sort_unstable();
    assert_eq!(got, vec![1, 3]);
}

#[test]
fn a_literal_that_is_not_json_is_a_typed_error_not_an_empty_result() {
    let mut engine = engine("invalid");
    let err = engine
        .execute_powql(r#"Post filter .data = "not json" { .id }"#)
        .expect_err("comparing a json column with a non-JSON literal must error");
    let msg = err.to_string();
    assert!(
        msg.contains("json"),
        "the error must name the json comparison, got: {msg}"
    );
}

#[test]
fn ordered_comparisons_against_text_stay_refused_or_strict() {
    let mut engine = engine("ordered");
    // `<` between a document and text is not given a meaning: it must not
    // silently match anything.
    if let Ok(result) = engine.execute_powql(r#"Post filter .data < "{}" { .id }"#) {
        assert_eq!(ids(result), Vec::<i64>::new());
    }
}

#[test]
fn sql_frontend_agrees() {
    let mut engine = engine("sql");
    let got = ids(engine
        .execute_sql(r#"SELECT id FROM Post WHERE data = '{ "b": 2, "a": 1 }'"#)
        .unwrap());
    assert_eq!(got, vec![1]);
}
