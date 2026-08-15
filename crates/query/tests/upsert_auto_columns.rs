//! `upsert` used to skip `auto` assignment on its insert branch, writing NULL
//! into the very column the user declared `unique auto`.
//!
//! ```text
//! type T { unique auto id: int, unique k: str }
//! insert T { k := "a" } returning     -> [[1,"a"]]        auto honoured
//! upsert T on .k { k := "b" }         -> 1 row affected
//! upsert T on .k { k := "c" }         -> 1 row affected
//! T { .id, .k }                       -> [[1,"a"],[null,"b"],[null,"c"]]
//! ```
//!
//! Nothing rejected it: several NULLs coexist in a unique index by design, so
//! the second NULL id was as acceptable to the index as the first. `default`
//! values were already applied on this branch; only `auto` was skipped, which
//! is what made it look deliberate rather than missed. Writing NULL into a
//! declared key column is corruption of the caller's key, so the insert branch
//! of an upsert now assigns `auto` exactly as `insert` does.
//!
//! The update branch deliberately does not: a row that already exists keeps
//! the key it already has.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_upsert_auto_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("failed to execute `{query}`: {e}"))
}

/// `(id, k)` pairs, with the id as `Option<i64>` so a NULL is a value the
/// assertion can name rather than a panic.
fn id_key_rows(engine: &mut Engine, query: &str) -> Vec<(Option<i64>, String)> {
    match exec(engine, query) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                let id = match &r[0] {
                    Value::Int(n) => Some(*n),
                    Value::Empty => None,
                    other => panic!("expected int or null id, got {other:?}"),
                };
                let k = match &r[1] {
                    Value::Str(s) => s.clone(),
                    other => panic!("expected str key, got {other:?}"),
                };
                (id, k)
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The reported sequence, verbatim. Against the unfixed engine this returns
/// `[(Some(1), "a"), (None, "b"), (None, "c")]` — two NULL primary keys and
/// no error anywhere.
#[test]
fn upsert_inserts_assign_auto_ids() {
    let dir = temp_dir("basic");
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type T { unique auto id: int, unique k: str }");

    exec(&mut engine, r#"insert T { k := "a" }"#);
    assert!(matches!(
        exec(&mut engine, r#"upsert T on .k { k := "b" }"#),
        QueryResult::Modified(1)
    ));
    assert!(matches!(
        exec(&mut engine, r#"upsert T on .k { k := "c" }"#),
        QueryResult::Modified(1)
    ));

    let mut rows = id_key_rows(&mut engine, "T { .id, .k }");
    rows.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(rows.len(), 3);

    assert!(
        rows.iter().all(|(id, k)| {
            assert!(id.is_some(), "row {k:?} was inserted with a NULL auto id");
            true
        }),
        "no upsert-inserted row may carry a NULL auto id"
    );

    let ids: Vec<i64> = rows.iter().map(|(id, _)| id.unwrap()).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 3, "auto ids must be distinct: {ids:?}");
    // Insertion order here is a, b, c, and the sequence hands out 1, 2, 3.
    assert_eq!(ids, vec![1, 2, 3], "auto ids must be monotonic: {ids:?}");
}

/// The conflicting branch must leave the existing key alone: an upsert that
/// updates a row is not a new row and must not consume or change an id.
#[test]
fn upsert_conflict_keeps_the_existing_auto_id() {
    let dir = temp_dir("conflict");
    let mut engine = Engine::new(&dir).unwrap();
    exec(
        &mut engine,
        "type T { unique auto id: int, unique k: str, v: int }",
    );
    exec(&mut engine, r#"insert T { k := "a", v := 1 }"#);

    exec(&mut engine, r#"upsert T on .k { k := "a", v := 2 }"#);

    let rows = id_key_rows(&mut engine, "T { .id, .k }");
    assert_eq!(rows, vec![(Some(1), "a".to_string())]);

    // The id the update did not consume is still the next one out.
    exec(&mut engine, r#"upsert T on .k { k := "b" }"#);
    let mut rows = id_key_rows(&mut engine, "T { .id, .k }");
    rows.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        rows,
        vec![(Some(1), "a".to_string()), (Some(2), "b".to_string())]
    );
}

/// An id the caller supplies explicitly is honoured, and pushes the sequence
/// past itself so a later upsert-insert cannot collide with it. This is the
/// same contract `insert` has; the point is that the upsert path shares the
/// assignment logic rather than reimplementing part of it.
#[test]
fn upsert_honours_an_explicit_id_and_advances_the_sequence() {
    let dir = temp_dir("explicit");
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type T { unique auto id: int, unique k: str }");

    exec(&mut engine, r#"upsert T on .k { id := 50, k := "a" }"#);
    exec(&mut engine, r#"upsert T on .k { k := "b" }"#);
    exec(&mut engine, r#"insert T { k := "c" }"#);

    let mut rows = id_key_rows(&mut engine, "T { .id, .k }");
    rows.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        rows,
        vec![
            (Some(50), "a".to_string()),
            (Some(51), "b".to_string()),
            (Some(52), "c".to_string()),
        ]
    );
}

/// `insert` exempts `auto` columns from the required check; `upsert` did not,
/// so the same schema accepted one statement and refused the other.
#[test]
fn upsert_does_not_demand_a_value_for_a_required_auto_column() {
    let dir = temp_dir("required");
    let mut engine = Engine::new(&dir).unwrap();
    exec(
        &mut engine,
        "type T { required unique auto id: int, unique k: str }",
    );

    exec(&mut engine, r#"insert T { k := "a" }"#);
    exec(&mut engine, r#"upsert T on .k { k := "b" }"#);

    let mut rows = id_key_rows(&mut engine, "T { .id, .k }");
    rows.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        rows,
        vec![(Some(1), "a".to_string()), (Some(2), "b".to_string())]
    );
}

/// Ids handed out by an upsert have to survive a restart the same way an
/// insert's do — the sequence is seeded from the highest stored value, so a
/// NULL written by the old behavior would also have reset it.
#[test]
fn upsert_assigned_ids_survive_a_restart_and_the_sequence_resumes() {
    let dir = temp_dir("restart");
    {
        let mut engine = Engine::new(&dir).unwrap();
        exec(&mut engine, "type T { unique auto id: int, unique k: str }");
        exec(&mut engine, r#"upsert T on .k { k := "a" }"#);
        exec(&mut engine, r#"upsert T on .k { k := "b" }"#);
    }

    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, r#"upsert T on .k { k := "c" }"#);
    let mut rows = id_key_rows(&mut engine, "T { .id, .k }");
    rows.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        rows,
        vec![
            (Some(1), "a".to_string()),
            (Some(2), "b".to_string()),
            (Some(3), "c".to_string()),
        ]
    );
}
