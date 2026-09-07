//! Q20: a materialized view created while its source returns no rows is not
//! frozen as a table of strings.
//!
//! Column types were inferred only from the rows the source query returned at
//! create time, so an empty materialization typed every column `str` and
//! persisted that. From the first insert onwards every read and every refresh
//! failed with "produced a Int in column 'id' but the view stores Str; drop and
//! recreate the view", and the state survived restart: a permanently broken
//! view produced by two ordinary statements in the ordinary order.
//!
//! Two rules close it. A pass-through projection column takes its type
//! statically from the source schema, so the common shape is right at create
//! time. And a refresh whose fresh rows disagree with a backing table that
//! holds no rows retypes it instead of refusing: there is nothing there for the
//! stored types to describe.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use std::path::PathBuf;

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "powdb_matview_empty_typing_{name}_{}_{:?}",
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

fn cells(result: &QueryResult) -> Vec<Vec<String>> {
    match result {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|row| row.iter().map(Value::to_wire_string).collect())
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The declared type of every column of `table`, in schema order.
fn column_types(engine: &mut Engine, table: &str) -> Vec<String> {
    match exec(engine, &format!("describe {table}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|row| match row.get(1) {
                Some(Value::Str(name)) => name.clone(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn seed_empty_view(engine: &mut Engine) {
    exec(engine, "type S { required unique id: int, n: int }");
    exec(engine, "insert S { id := 1 }");
    // `.n` is null on the only row, so the view materializes over no rows.
    exec(engine, "materialize EV as S filter .n > 0");
}

#[test]
fn a_view_materialized_over_zero_rows_inherits_the_source_column_types() {
    let mut engine = Engine::new(&fresh_dir("types")).unwrap();
    seed_empty_view(&mut engine);
    assert_eq!(column_types(&mut engine, "EV"), vec!["int", "int"]);
}

#[test]
fn a_view_materialized_over_zero_rows_still_answers_after_the_source_fills() {
    let mut engine = Engine::new(&fresh_dir("fills")).unwrap();
    seed_empty_view(&mut engine);
    exec(&mut engine, "insert S { id := 2, n := 5 }");
    for read in ["EV", "EV { .id }", "count(EV)", "refresh EV"] {
        engine
            .execute_powql(read)
            .unwrap_or_else(|e| panic!("`{read}` on a view materialized over zero rows: {e}"));
    }
    assert_eq!(
        cells(&exec(&mut engine, "EV")),
        cells(&exec(&mut engine, "S filter .n > 0"))
    );
}

#[test]
fn an_int_comparison_on_such_a_view_is_not_a_type_error() {
    let mut engine = Engine::new(&fresh_dir("compare")).unwrap();
    seed_empty_view(&mut engine);
    exec(&mut engine, "insert S { id := 2, n := 5 }");
    assert_eq!(
        cells(&exec(&mut engine, "EV filter .n = 5 { .id }")),
        vec![vec!["2".to_string()]]
    );
}

#[test]
fn the_inherited_typing_survives_a_restart() {
    let dir = fresh_dir("restart");
    {
        let mut engine = Engine::new(&dir).unwrap();
        seed_empty_view(&mut engine);
    }
    let mut engine = Engine::new(&dir).unwrap();
    assert_eq!(column_types(&mut engine, "EV"), vec!["int", "int"]);
    exec(&mut engine, "insert S { id := 2, n := 5 }");
    assert_eq!(
        cells(&exec(&mut engine, "EV")),
        cells(&exec(&mut engine, "S filter .n > 0"))
    );
}

/// A computed column has no source column to inherit from, so it is still
/// typed from the rows. When there are none, the backing table is empty, and
/// the first refresh that produces a row retypes it rather than refusing
/// forever.
#[test]
fn a_computed_column_typed_over_zero_rows_retypes_on_the_first_real_refresh() {
    let mut engine = Engine::new(&fresh_dir("computed")).unwrap();
    exec(&mut engine, "type S { required unique id: int, n: int }");
    exec(&mut engine, "insert S { id := 1 }");
    exec(
        &mut engine,
        "materialize EV as S filter .n > 0 { doubled: .n * 2 }",
    );
    exec(&mut engine, "insert S { id := 2, n := 5 }");
    assert_eq!(
        cells(&exec(&mut engine, "EV")),
        vec![vec!["10".to_string()]]
    );
    assert_eq!(column_types(&mut engine, "EV"), vec!["int"]);
}

// The refusal itself still stands where it protects real rows: a NON-empty
// view whose refresh produces a different type stays a typed error, held by
// `matview_backing_types::a_refresh_that_changes_a_column_type_is_a_clean_error`.
