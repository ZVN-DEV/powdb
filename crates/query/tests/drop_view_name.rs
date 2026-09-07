//! Q5: `drop <name>` refuses a materialized view instead of half-dropping it.
//!
//! `drop V` removed the backing table but left the registry entry, so the view
//! was still registered, still listed, still marked dirty by writes to its
//! source, and every read of it reported a missing table. The definition
//! survived in `views.bin`, so the state outlived the process. There is one DDL
//! that drops a view completely, and this is the only way to reach it.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int, n: int }")
        .unwrap();
    engine
        .execute_powql("insert T { id := 1, n := 5 }")
        .unwrap();
    engine.execute_powql("materialize V as T { .n }").unwrap();
    (dir, engine)
}

fn message(engine: &mut Engine, query: &str) -> String {
    engine
        .execute_powql(query)
        .map(|ok| panic!("`{query}` should have been refused, got {ok:?}"))
        .unwrap_err()
        .to_string()
}

#[test]
fn dropping_a_view_as_a_table_is_refused_and_names_the_right_ddl() {
    let (_dir, mut engine) = engine();
    let refusal = message(&mut engine, "drop V");
    assert!(
        refusal.contains("materialized view") && refusal.contains("drop view V"),
        "the refusal must name the DDL that works, got {refusal}"
    );
    assert!(
        engine.execute_powql("count(V)").is_ok(),
        "the view must still be readable after the refusal"
    );
}

#[test]
fn if_exists_does_not_turn_the_refusal_into_a_half_drop() {
    let (_dir, mut engine) = engine();
    message(&mut engine, "drop if exists V");
    assert!(engine.execute_powql("count(V)").is_ok());
}

#[test]
fn drop_view_still_drops_it_completely() {
    let (_dir, mut engine) = engine();
    engine.execute_powql("drop view V").unwrap();
    assert!(
        engine.execute_powql("count(V)").is_err(),
        "the view is gone"
    );
    // The registry entry is gone too, so the name is free again.
    engine.execute_powql("materialize V as T { .n }").unwrap();
    match engine.execute_powql("count(V)").unwrap() {
        QueryResult::Scalar(value) => assert_eq!(value, Value::Int(1)),
        other => panic!("expected a count, got {other:?}"),
    }
}

#[test]
fn a_plain_table_still_drops() {
    let (_dir, mut engine) = engine();
    engine.execute_powql("drop view V").unwrap();
    engine.execute_powql("drop T").unwrap();
    assert!(engine.execute_powql("count(T)").is_err());
}

#[test]
fn the_refusal_survives_a_restart_because_nothing_changed() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type T { required unique id: int, n: int }")
            .unwrap();
        engine
            .execute_powql("insert T { id := 1, n := 5 }")
            .unwrap();
        engine.execute_powql("materialize V as T { .n }").unwrap();
        assert!(engine.execute_powql("drop V").is_err());
    }
    let mut reopened = Engine::new(dir.path()).unwrap();
    assert!(
        reopened.execute_powql("count(V)").is_ok(),
        "the view must come back whole"
    );
}
