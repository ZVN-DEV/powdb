//! S3 and S10: a query that reads a table which does not exist says so.
//!
//! `count(Missing)` reported the storage layer's generic error (wire class 0,
//! "an internal error") while `count(Missing filter .x = 1)` reported the typed
//! `TableNotFound` (class 2), so the same mistake was retryable or not
//! depending on whether a filter happened to be attached. A join to a missing
//! table was worse: it reported `column 'id' not found in table 'm'`, sending
//! the reader to look for a column in a table that was never there.

use powdb_query::executor::Engine;
use powdb_query::result::QueryError;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type Post { required unique id: int, uid: int }")
        .unwrap();
    engine
        .execute_powql("insert Post { id := 1, uid := 1 }")
        .unwrap();
    (dir, engine)
}

fn error(engine: &mut Engine, query: &str) -> QueryError {
    engine
        .execute_powql(query)
        .map(|ok| panic!("`{query}` should have failed, got {ok:?}"))
        .unwrap_err()
}

fn assert_missing_table(engine: &mut Engine, query: &str, table: &str) {
    match error(engine, query) {
        QueryError::TableNotFound(name) => assert_eq!(name, table, "for `{query}`"),
        other => panic!("`{query}` reported {other:?}, not a missing table"),
    }
}

#[test]
fn a_count_over_a_missing_table_names_the_table() {
    let (_dir, mut engine) = engine();
    assert_missing_table(&mut engine, "count(Missing)", "Missing");
}

#[test]
fn every_spelling_of_the_same_mistake_reports_the_same_class() {
    let (_dir, mut engine) = engine();
    for query in [
        "count(Missing)",
        "count(Missing filter .x = 1)",
        "Missing { .id }",
        "Missing filter .id = 1 { .id }",
        "sum(Missing { .id })",
        "Missing order .id limit 1 { .id }",
    ] {
        assert_missing_table(&mut engine, query, "Missing");
    }
}

#[test]
fn a_join_to_a_missing_table_names_the_table() {
    let (_dir, mut engine) = engine();
    assert_missing_table(
        &mut engine,
        "Post as p join Missing as m on p.uid = m.id { p.id }",
        "Missing",
    );
    assert_missing_table(
        &mut engine,
        "Missing as m join Post as p on m.id = p.uid { p.id }",
        "Missing",
    );
}

#[test]
fn a_mutation_of_a_missing_table_names_the_table() {
    let (_dir, mut engine) = engine();
    assert_missing_table(&mut engine, "Missing filter .id = 1 delete", "Missing");
    assert_missing_table(
        &mut engine,
        "Missing filter .id = 1 update { uid := 2 }",
        "Missing",
    );
}

#[test]
fn the_read_only_path_reports_it_the_same_way() {
    let (_dir, engine) = engine();
    match engine.execute_powql_readonly("count(Missing)").unwrap_err() {
        QueryError::TableNotFound(name) => assert_eq!(name, "Missing"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn a_table_that_exists_is_untouched() {
    let (_dir, mut engine) = engine();
    engine.execute_powql("count(Post)").unwrap();
    engine.execute_powql("Post { .id }").unwrap();
}
