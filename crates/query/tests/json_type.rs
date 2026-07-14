//! End-to-end tests for the v0.12 JSON type: the `json` column type, PJ1
//! coercion on insert/update, `->` path extraction in filters/projections,
//! `json_type`, null-vs-missing semantics, spilled (>4070B) documents, and
//! typed error cases. Exercises the whole query stack through `Engine`.

use powdb_query::executor::Engine;
use powdb_query::result::{QueryError, QueryResult};
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_jsontype_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn exec(engine: &mut Engine, q: &str) -> QueryResult {
    engine
        .execute_powql(q)
        .unwrap_or_else(|e| panic!("failed `{q}`: {e}"))
}

fn rows(engine: &mut Engine, q: &str) -> Vec<Vec<Value>> {
    match exec(engine, q) {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows for `{q}`, got {other:?}"),
    }
}

/// Fresh engine with a `Post { id: int, data: json }` table.
fn engine_with_posts(name: &str) -> Engine {
    let mut engine = Engine::new(&temp_dir(name)).unwrap();
    exec(&mut engine, "type Post { required id: int, data: json }");
    engine
}

#[test]
fn json_column_ddl_and_describe() {
    let mut engine = engine_with_posts("ddl");
    // `describe` renders the column type as `json` end to end.
    let r = rows(&mut engine, "describe Post");
    let data_row = r
        .iter()
        .find(|row| row[0] == Value::Str("data".into()))
        .expect("data column present");
    assert_eq!(data_row[1], Value::Str("json".into()));
}

#[test]
fn insert_coerces_text_to_json_and_round_trips() {
    let mut engine = engine_with_posts("insert");
    exec(
        &mut engine,
        r#"insert Post { id := 1, data := "{\"author\":\"alice\",\"age\":30}" }"#,
    );
    // The stored value is canonical PJ1 and renders as canonical JSON text
    // (keys sorted bytewise).
    let r = rows(&mut engine, "Post filter .id = 1 { .data }");
    assert_eq!(r.len(), 1);
    assert!(matches!(r[0][0], Value::Json(_)), "stored as Json");
    assert_eq!(r[0][0].to_wire_string(), r#"{"age":30,"author":"alice"}"#);
}

#[test]
fn invalid_json_insert_is_a_typed_error() {
    let mut engine = engine_with_posts("invalid");
    let err = engine
        .execute_powql(r#"insert Post { id := 1, data := "{not valid json" }"#)
        .unwrap_err();
    // Coercion errors flow through the executor's standard error channel; the
    // message carries the wire-safe `invalid JSON` prefix (SAFE_ERROR_PREFIXES
    // matches "invalid") so it reaches the client verbatim.
    let msg = match err {
        QueryError::TypeError(m) | QueryError::Execution(m) => m,
        other => panic!("expected a coercion error, got {other:?}"),
    };
    assert!(
        msg.starts_with("invalid JSON"),
        "message must start with the safe `invalid JSON` prefix, got: {msg}"
    );
}

#[test]
fn path_extraction_scalarizes_by_type() {
    let mut engine = engine_with_posts("scalarize");
    exec(
        &mut engine,
        r#"insert Post { id := 1, data := "{\"author\":\"alice\",\"age\":30,\"score\":9.5,\"active\":true,\"tags\":[\"rust\",\"db\"],\"nested\":{\"x\":1},\"maybe\":null}" }"#,
    );
    let r = rows(
        &mut engine,
        "Post filter .id = 1 { \
         s: .data->author, i: .data->age, f: .data->score, \
         b: .data->active, arr0: .data->tags->0, nx: .data->nested->x }",
    );
    assert_eq!(r[0][0], Value::Str("alice".into()), "string -> Str");
    assert_eq!(r[0][1], Value::Int(30), "integral number -> Int");
    assert_eq!(r[0][2], Value::Float(9.5), "fractional number -> Float");
    assert_eq!(r[0][3], Value::Bool(true), "bool -> Bool");
    assert_eq!(r[0][4], Value::Str("rust".into()), "array index -> element");
    assert_eq!(r[0][5], Value::Int(1), "nested object key -> Int");
}

#[test]
fn object_and_array_nodes_extract_as_subdocuments() {
    let mut engine = engine_with_posts("subdoc");
    exec(
        &mut engine,
        r#"insert Post { id := 1, data := "{\"tags\":[\"a\",\"b\"],\"nested\":{\"x\":1}}" }"#,
    );
    let r = rows(
        &mut engine,
        "Post filter .id = 1 { tags: .data->tags, nested: .data->nested }",
    );
    // Composite nodes come back as owned JSON sub-documents.
    assert!(matches!(r[0][0], Value::Json(_)), "array node -> Json");
    assert!(matches!(r[0][1], Value::Json(_)), "object node -> Json");
    assert_eq!(r[0][0].to_wire_string(), r#"["a","b"]"#);
    assert_eq!(r[0][1].to_wire_string(), r#"{"x":1}"#);
}

#[test]
fn filter_over_path() {
    let mut engine = engine_with_posts("filter");
    exec(
        &mut engine,
        r#"insert Post { id := 1, data := "{\"author\":\"alice\",\"age\":30}" }"#,
    );
    exec(
        &mut engine,
        r#"insert Post { id := 2, data := "{\"author\":\"bob\",\"age\":18}" }"#,
    );
    // Equality on a string node.
    let r = rows(
        &mut engine,
        r#"Post filter .data->author = "alice" { .id }"#,
    );
    assert_eq!(r, vec![vec![Value::Int(1)]]);
    // Range on a numeric node (no implicit coercion — Int node vs Int literal).
    let r = rows(&mut engine, "Post filter .data->age > 21 { .id }");
    assert_eq!(r, vec![vec![Value::Int(1)]]);
}

#[test]
fn null_versus_missing_via_json_type() {
    let mut engine = engine_with_posts("nullmiss");
    exec(
        &mut engine,
        r#"insert Post { id := 1, data := "{\"maybe\":null,\"tags\":[1,2],\"who\":\"x\",\"n\":5,\"flag\":false,\"obj\":{}}" }"#,
    );
    let r = rows(
        &mut engine,
        "Post filter .id = 1 { \
         present_null: json_type(.data->maybe), \
         missing: json_type(.data->nope), \
         arr: json_type(.data->tags), \
         obj: json_type(.data->obj), \
         num: json_type(.data->n), \
         str: json_type(.data->who), \
         boo: json_type(.data->flag) }",
    );
    // A present JSON null classifies as 'null'; a missing path is the empty
    // set (the whole point of json_type as the escape hatch).
    assert_eq!(r[0][0], Value::Str("null".into()), "present null -> 'null'");
    assert_eq!(r[0][1], Value::Empty, "missing path -> Empty");
    assert_eq!(r[0][2], Value::Str("array".into()));
    assert_eq!(r[0][3], Value::Str("object".into()));
    assert_eq!(r[0][4], Value::Str("number".into()));
    assert_eq!(r[0][5], Value::Str("string".into()));
    assert_eq!(r[0][6], Value::Str("bool".into()));
}

#[test]
fn missing_path_and_json_null_both_extract_empty() {
    let mut engine = engine_with_posts("emptyextract");
    exec(
        &mut engine,
        r#"insert Post { id := 1, data := "{\"maybe\":null}" }"#,
    );
    let r = rows(
        &mut engine,
        "Post filter .id = 1 { got_null: .data->maybe, got_missing: .data->nope }",
    );
    assert_eq!(r[0][0], Value::Empty, "JSON null scalarizes to Empty");
    assert_eq!(r[0][1], Value::Empty, "missing path scalarizes to Empty");
}

#[test]
fn project_path_alongside_ordering_by_a_real_column() {
    // Ordering/grouping BY a JSON path expression is a documented non-goal
    // (needs Expr-valued order/group keys); PowQL does not resolve order keys
    // against projection aliases either. The supported pattern is to ORDER by
    // a real column while PROJECTING path values — verify that composes.
    let mut engine = engine_with_posts("projorder");
    for (id, author, age) in [(3, "c", 30), (1, "a", 20), (2, "b", 40)] {
        exec(
            &mut engine,
            &format!(
                r#"insert Post {{ id := {id}, data := "{{\"author\":\"{author}\",\"age\":{age}}}" }}"#
            ),
        );
    }
    let r = rows(
        &mut engine,
        "Post order .id { .id, who: .data->author, yrs: .data->age }",
    );
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Str("a".into()), Value::Int(20)],
            vec![Value::Int(2), Value::Str("b".into()), Value::Int(40)],
            vec![Value::Int(3), Value::Str("c".into()), Value::Int(30)],
        ],
        "path projections evaluated per row, ordered by the real .id column"
    );
}

#[test]
fn spilled_document_over_4070_bytes_round_trips_and_queries() {
    let mut engine = engine_with_posts("spill");
    // A JSON doc whose string value alone exceeds the 4070B inline row cap, so
    // it spills into overflow pages (Lane A) and must reassemble for `->`.
    let big = "x".repeat(6000);
    exec(
        &mut engine,
        &format!(
            r#"insert Post {{ id := 1, data := "{{\"author\":\"zoe\",\"blob\":\"{big}\",\"n\":7}}" }}"#
        ),
    );
    // Filter and project through the reassembled (out-of-line) document.
    let r = rows(
        &mut engine,
        r#"Post filter .data->author = "zoe" { .id, who: .data->author, n: .data->n }"#,
    );
    assert_eq!(r.len(), 1, "spilled doc found via path filter");
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[0][1], Value::Str("zoe".into()));
    assert_eq!(r[0][2], Value::Int(7));
    // The big string node scalarizes back to its full length.
    let r = rows(&mut engine, "Post filter .id = 1 { blob: .data->blob }");
    match &r[0][0] {
        Value::Str(s) => assert_eq!(s.len(), 6000, "full spilled string reassembled"),
        other => panic!("expected Str, got {other:?}"),
    }
}

#[test]
fn update_recoerces_json() {
    let mut engine = engine_with_posts("update");
    exec(
        &mut engine,
        r#"insert Post { id := 1, data := "{\"author\":\"alice\"}" }"#,
    );
    exec(
        &mut engine,
        r#"Post filter .id = 1 update { data := "{\"author\":\"bob\",\"age\":40}" }"#,
    );
    let r = rows(
        &mut engine,
        "Post filter .id = 1 { who: .data->author, age: .data->age }",
    );
    assert_eq!(r[0][0], Value::Str("bob".into()));
    assert_eq!(r[0][1], Value::Int(40));

    // An invalid update value is a typed error and leaves the row unchanged.
    let err = engine
        .execute_powql(r#"Post filter .id = 1 update { data := "nope{" }"#)
        .unwrap_err();
    assert!(matches!(err, QueryError::TypeError(m) if m.starts_with("invalid JSON")));
    let r = rows(&mut engine, "Post filter .id = 1 { who: .data->author }");
    assert_eq!(
        r[0][0],
        Value::Str("bob".into()),
        "row unchanged after failed update"
    );
}

#[test]
fn path_on_non_json_column_is_a_typed_error() {
    let mut engine = engine_with_posts("nonjson");
    exec(
        &mut engine,
        r#"insert Post { id := 1, data := "{\"a\":1}" }"#,
    );
    // `.id` is an int column; a `->` on it is a type error, not a silent Empty.
    let err = engine.execute_powql("Post filter .id->x = 1").unwrap_err();
    match err {
        QueryError::TypeError(msg) => {
            assert!(msg.starts_with("type mismatch"), "safe prefix, got: {msg}");
            assert!(msg.contains("not json"), "names the problem, got: {msg}");
        }
        other => panic!("expected a TypeError, got {other:?}"),
    }
}

#[test]
fn deeply_nested_path() {
    let mut engine = engine_with_posts("deep");
    exec(
        &mut engine,
        r#"insert Post { id := 1, data := "{\"a\":{\"b\":{\"c\":{\"d\":42}}}}" }"#,
    );
    let r = rows(
        &mut engine,
        "Post filter .id = 1 { deep: .data->a->b->c->d }",
    );
    assert_eq!(r[0][0], Value::Int(42));
}

#[test]
fn prepared_insert_coerces_json_not_raw_str() {
    // The prepared InsertFast path writes literals verbatim; without the json
    // guard it would store a raw Str (invalid PJ1) in a json column. Confirm
    // the value round-trips as canonical JSON (i.e. it was parsed to PJ1).
    use powdb_query::ast::Literal;
    let mut engine = engine_with_posts("prepared");
    let prep = engine
        .prepare(r#"insert Post { id := 1, data := "{\"b\":2,\"a\":1}" }"#)
        .expect("prepare");
    engine
        .execute_prepared(
            &prep,
            &[Literal::Int(1), Literal::String(r#"{"b":2,"a":1}"#.into())],
        )
        .expect("execute_prepared");
    let r = rows(&mut engine, "Post filter .id = 1 { .data, a: .data->a }");
    // Canonical (keys sorted) output proves the string was parsed to PJ1.
    assert_eq!(r[0][0].to_wire_string(), r#"{"a":1,"b":2}"#);
    assert_eq!(r[0][1], Value::Int(1), "path walks the coerced PJ1 doc");
}
