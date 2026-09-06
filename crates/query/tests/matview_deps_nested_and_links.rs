//! E7: a materialized view over a nested projection block, a block link
//! traversal or a scalar link path registers the child and target tables as
//! dependencies.
//!
//! `extract_view_deps` used to register only the query's own source and its
//! joins, so a view over the two flagship PowQL shapes never recorded the
//! tables its nested fields actually read. Nothing then marked the view dirty
//! when those tables changed and every later read served the copy taken at
//! create time, permanently and without an error.
//!
//! Each test states the same property: reading the view answers exactly what
//! re-running its source query answers, after a write to a table only the
//! nested or link half of the query reaches.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use std::path::PathBuf;

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "powdb_matview_nested_deps_{name}_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
}

/// Every cell of a result, rendered with the canonical text the wire uses so a
/// nested JSON column and a scalar column compare the same way.
fn cells(result: &QueryResult) -> Vec<Vec<String>> {
    match result {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|row| row.iter().map(Value::to_wire_string).collect())
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The property a materialized view exists to keep: reading it answers exactly
/// what re-running its source query answers.
fn assert_view_tracks_source(engine: &mut Engine, view: &str, source: &str) {
    let expected = cells(&exec(engine, source));
    let actual = cells(&exec(engine, view));
    assert_eq!(
        actual, expected,
        "materialized view '{view}' is stale against its source `{source}`"
    );
}

fn seed(engine: &mut Engine) {
    exec(engine, "type User { required unique id: int, name: str }");
    exec(
        engine,
        "type Order { required unique id: int, user_id: int, total: float }",
    );
    exec(engine, "insert User { id := 1, name := \"ada\" }");
    exec(
        engine,
        "insert Order { id := 10, user_id := 1, total := 5.0 }",
    );
}

#[test]
fn a_view_over_a_nested_block_is_dirtied_by_the_child_table() {
    let dir = fresh_dir("nested_block");
    let mut engine = Engine::new(&dir).unwrap();
    seed(&mut engine);
    let source =
        "User as u { name: u.name, orders: Order as o filter o.user_id = u.id { o.total } }";
    exec(&mut engine, &format!("materialize V as {source}"));
    assert_view_tracks_source(&mut engine, "V", source);

    exec(
        &mut engine,
        "insert Order { id := 11, user_id := 1, total := 7.0 }",
    );
    assert_view_tracks_source(&mut engine, "V", source);
}

#[test]
fn a_view_over_a_block_link_is_dirtied_by_the_link_target() {
    let dir = fresh_dir("block_link");
    let mut engine = Engine::new(&dir).unwrap();
    seed(&mut engine);
    exec(&mut engine, "link User.orders -> Order on id = user_id");
    let source = "User as u { name: u.name, orders: u.orders { total } }";
    exec(&mut engine, &format!("materialize V as {source}"));
    assert_view_tracks_source(&mut engine, "V", source);

    exec(
        &mut engine,
        "insert Order { id := 12, user_id := 1, total := 9.0 }",
    );
    assert_view_tracks_source(&mut engine, "V", source);
}

#[test]
fn a_view_over_a_scalar_link_hop_is_dirtied_by_the_target() {
    let dir = fresh_dir("scalar_link");
    let mut engine = Engine::new(&dir).unwrap();
    seed(&mut engine);
    exec(&mut engine, "link Order.user -> User on user_id = id");
    let source = "Order as o { id: o.id, user_name: o.user.name }";
    exec(&mut engine, &format!("materialize V as {source}"));
    assert_view_tracks_source(&mut engine, "V", source);

    exec(
        &mut engine,
        "User filter .id = 1 update { name := \"grace\" }",
    );
    assert_view_tracks_source(&mut engine, "V", source);
}

#[test]
fn a_view_over_a_two_hop_scalar_link_is_dirtied_by_the_far_target() {
    let dir = fresh_dir("two_hop");
    let mut engine = Engine::new(&dir).unwrap();
    seed(&mut engine);
    exec(
        &mut engine,
        "type Company { required unique id: int, label: str }",
    );
    exec(
        &mut engine,
        "insert Company { id := 100, label := \"zvn\" }",
    );
    exec(&mut engine, "alter User add column company_id: int");
    exec(
        &mut engine,
        "User filter .id = 1 update { company_id := 100 }",
    );
    exec(&mut engine, "link Order.user -> User on user_id = id");
    exec(
        &mut engine,
        "link User.company -> Company on company_id = id",
    );
    let source = "Order as o { id: o.id, label: o.user.company.label }";
    exec(&mut engine, &format!("materialize V as {source}"));
    assert_view_tracks_source(&mut engine, "V", source);

    exec(
        &mut engine,
        "Company filter .id = 100 update { label := \"powdb\" }",
    );
    assert_view_tracks_source(&mut engine, "V", source);
}

#[test]
fn a_view_whose_filter_reads_a_subquery_is_dirtied_by_that_table() {
    let dir = fresh_dir("subquery");
    let mut engine = Engine::new(&dir).unwrap();
    seed(&mut engine);
    let source = "User filter .id in (Order { .user_id }) { .id, .name }";
    exec(&mut engine, &format!("materialize V as {source}"));
    assert_view_tracks_source(&mut engine, "V", source);

    exec(&mut engine, "Order filter .id = 10 delete");
    assert_view_tracks_source(&mut engine, "V", source);
}
