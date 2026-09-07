//! Q24: `union` without `all` is a set on both sides.
//!
//! The executor seeded its seen-set with the left branch's rows and then only
//! tested the right branch against it, so every duplicate the left branch
//! carried survived: `T { .k } union T { .k }` answered x, x, y over rows
//! x, x, y where x, y is correct. Both dispatch arms (read-write and read-only)
//! carried their own copy of the same loop.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn seeded() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int, k: str }")
        .unwrap();
    for (id, k) in [(1, "x"), (2, "x"), (3, "y")] {
        engine
            .execute_powql(&format!("insert T {{ id := {id}, k := \"{k}\" }}"))
            .unwrap();
    }
    (dir, engine)
}

fn keys(result: QueryResult) -> Vec<String> {
    match result {
        QueryResult::Rows { rows, .. } => {
            let mut out: Vec<String> = rows
                .iter()
                .map(|row| match &row[0] {
                    Value::Str(s) => s.clone(),
                    other => format!("{other:?}"),
                })
                .collect();
            out.sort();
            out
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn union_deduplicates_the_left_branch_too() {
    let (_dir, mut engine) = seeded();
    assert_eq!(
        keys(engine.execute_powql("T { .k } union T { .k }").unwrap()),
        vec!["x".to_string(), "y".to_string()]
    );
}

#[test]
fn union_deduplicates_a_left_branch_with_no_right_matches() {
    let (_dir, mut engine) = seeded();
    assert_eq!(
        keys(
            engine
                .execute_powql("T { .k } union T filter .id = 99 { .k }")
                .unwrap()
        ),
        vec!["x".to_string(), "y".to_string()]
    );
}

#[test]
fn union_all_still_keeps_every_row() {
    let (_dir, mut engine) = seeded();
    assert_eq!(
        keys(engine.execute_powql("T { .k } union all T { .k }").unwrap()),
        vec![
            "x".to_string(),
            "x".to_string(),
            "x".to_string(),
            "x".to_string(),
            "y".to_string(),
            "y".to_string()
        ]
    );
}

#[test]
fn the_read_only_path_deduplicates_the_same_way() {
    let (_dir, engine) = seeded();
    assert_eq!(
        keys(
            engine
                .execute_powql_readonly("T { .k } union T { .k }")
                .unwrap()
        ),
        vec!["x".to_string(), "y".to_string()]
    );
}

// The SQL frontend has no `UNION`: `select k from T union select k from T`
// is refused at parse time ("unexpected trailing SQL token: select"), so there
// is no second spelling of this rule to hold.
