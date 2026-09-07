//! Q7 and Q16: a projection column is named after what it computes.
//!
//! Every projection path inlined its own `Expr::Field(name) => name, _ => "?"`,
//! so `D { .j->a, .j->b }` came back as two columns both called `?` and a
//! caller reading by name could not tell them apart. A grouped aggregate was
//! worse: the planner rewrites the call into a reference to its internal
//! `__agg_N` slot, and the unaliased field took that synthetic name as its
//! header, in both languages.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type D { required unique id: int, n: int, s: str, j: json }")
        .unwrap();
    engine
        .execute_powql(
            "insert D { id := 1, n := 3, s := \"a\", j := \"{ \\\"a\\\": 1, \\\"b\\\": 2 }\" }",
        )
        .unwrap();
    (dir, engine)
}

fn columns(engine: &mut Engine, query: &str) -> Vec<String> {
    match engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
    {
        QueryResult::Rows { columns, .. } => columns,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn sql_columns(engine: &mut Engine, query: &str) -> Vec<String> {
    match engine
        .execute_sql(query)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
    {
        QueryResult::Rows { columns, .. } => columns,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn two_json_paths_get_two_different_names() {
    let (_dir, mut engine) = engine();
    assert_eq!(
        columns(&mut engine, "D { .j->a, .j->b }"),
        [".j->a", ".j->b"]
    );
}

#[test]
fn every_access_path_names_it_the_same() {
    let (_dir, mut engine) = engine();
    for query in [
        "D { .j->a, .n + 1 }",
        "D { .j->a, .n + 1 } limit 5",
        "D filter .n = 3 { .j->a, .n + 1 }",
        "D order .id { .j->a, .n + 1 }",
    ] {
        let fast = columns(&mut engine, query);
        engine.set_force_generic_path(true);
        let generic = columns(&mut engine, query);
        engine.set_force_generic_path(false);
        assert_eq!(fast, generic, "`{query}` names differ by access path");
        assert_eq!(fast, [".j->a", ".n + 1"], "{query}");
    }
}

#[test]
fn a_bare_column_keeps_its_own_name() {
    let (_dir, mut engine) = engine();
    assert_eq!(columns(&mut engine, "D { .id, .n }"), ["id", "n"]);
    assert_eq!(columns(&mut engine, "D { x: .n }"), ["x"]);
}

#[test]
fn an_unaliased_aggregate_is_named_after_the_call() {
    let (_dir, mut engine) = engine();
    assert_eq!(
        columns(&mut engine, "D group .s { .s, count(.n) }"),
        ["s", "count(.n)"]
    );
    assert_eq!(
        columns(&mut engine, "D group .s { .s, count(.n), sum(.n) }"),
        ["s", "count(.n)", "sum(.n)"]
    );
    assert_eq!(
        columns(&mut engine, "D group .s { .s, c: count(.n) }"),
        ["s", "c"]
    );
}

#[test]
fn the_sql_frontend_uses_the_sql_spelling() {
    let (_dir, mut engine) = engine();
    assert_eq!(
        sql_columns(&mut engine, "select s, count(n) from D group by s"),
        ["s", "count(n)"]
    );
    assert_eq!(
        sql_columns(&mut engine, "select s, count(*) from D group by s"),
        ["s", "count(*)"]
    );
    assert_eq!(
        sql_columns(&mut engine, "select s, count(n) as c from D group by s"),
        ["s", "c"]
    );
}

#[test]
fn a_primary_key_column_is_required_and_unique() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_sql("create table t (id int primary key, n int)")
        .unwrap();
    engine
        .execute_sql("insert into t (id, n) values (1, 1)")
        .unwrap();
    assert!(
        engine
            .execute_sql("insert into t (id, n) values (1, 2)")
            .is_err(),
        "the key must be unique"
    );
    assert!(
        engine.execute_sql("insert into t (n) values (3)").is_err(),
        "the key must be required"
    );
}

#[test]
fn a_table_level_primary_key_names_the_spelling_that_works() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    let message = engine
        .execute_sql("create table t (id int, primary key (id))")
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("PRIMARY KEY") && message.contains("int PRIMARY KEY"),
        "got {message}"
    );
}

#[test]
fn a_json_column_can_be_declared_in_sql() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_sql("create table t (id int primary key, doc json)")
        .unwrap();
    engine
        .execute_sql("insert into t (id, doc) values (1, '{\"a\": 1}')")
        .unwrap();
    assert_eq!(
        sql_columns(&mut engine, "select json_type(doc) as k from t"),
        ["k"]
    );
}
