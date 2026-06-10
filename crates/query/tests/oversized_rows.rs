//! Oversized-row regression tests (remote-DoS fix).
//!
//! With `panic = "abort"` in the release profile, a panicking insert path
//! kills the whole server process. An encoded row larger than one 4KB
//! slotted page used to hit `expect("row too large for empty page")` in
//! `HeapFile::insert`. These tests pin the fixed behavior: oversized rows
//! are a clean query error and the engine stays fully usable.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;

fn temp_engine(name: &str) -> (Engine, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "powdb_oversized_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(&dir).unwrap();
    (engine, dir)
}

fn big_string(n: usize) -> String {
    "x".repeat(n)
}

#[test]
fn oversized_insert_is_clean_error_and_engine_survives() {
    let (mut engine, dir) = temp_engine("insert");
    engine
        .execute_powql("type T { required id: int, b: str }")
        .unwrap();

    // 5000-byte string > 4KB page capacity → must be a clean error.
    let q = format!(r#"insert T {{ id := 1, b := "{}" }}"#, big_string(5000));
    let err = engine
        .execute_powql(&q)
        .expect_err("oversized insert must fail");
    assert!(
        err.to_string().contains("row too large"),
        "unexpected error: {err}"
    );

    // Engine keeps working: normal insert + query succeed.
    engine
        .execute_powql(r#"insert T { id := 2, b := "small" }"#)
        .unwrap();
    let res = engine.execute_powql("count(T)").unwrap();
    match res {
        QueryResult::Scalar(v) => assert_eq!(format!("{v:?}"), "Int(1)"),
        other => panic!("expected scalar count, got {other:?}"),
    }
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn oversized_update_is_clean_error_and_row_intact() {
    let (mut engine, dir) = temp_engine("update");
    engine
        .execute_powql("type T { required id: int, b: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert T { id := 1, b := "original" }"#)
        .unwrap();

    // Oversized new value on update must fail cleanly...
    let q = format!(
        r#"T filter .id = 1 update {{ b := "{}" }}"#,
        big_string(5000)
    );
    let err = engine
        .execute_powql(&q)
        .expect_err("oversized update must fail");
    assert!(
        err.to_string().contains("row too large"),
        "unexpected error: {err}"
    );

    // ...and the original row must be untouched.
    let res = engine.execute_powql("T filter .id = 1").unwrap();
    match res {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "row must still exist");
            let joined = format!("{:?}", rows[0]);
            assert!(
                joined.contains("original"),
                "row value must be unchanged: {joined}"
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn oversized_update_does_not_poison_wal_replay() {
    // A failed oversized update must not leave a WAL record that breaks (or
    // mutates state during) recovery on the next open.
    let (mut engine, dir) = temp_engine("replay");
    engine
        .execute_powql("type T { required id: int, b: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert T { id := 1, b := "original" }"#)
        .unwrap();
    let q = format!(
        r#"T filter .id = 1 update {{ b := "{}" }}"#,
        big_string(5000)
    );
    engine
        .execute_powql(&q)
        .expect_err("oversized update must fail");
    // Run one more statement so any buffered WAL bytes get group-committed.
    engine.execute_powql("count(T)").unwrap();
    drop(engine);

    // Reopen: replay must succeed and show the original value.
    let mut engine = Engine::new(&dir).expect("reopen after failed oversized update");
    let res = engine.execute_powql("T filter .id = 1").unwrap();
    match res {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            let joined = format!("{:?}", rows[0]);
            assert!(joined.contains("original"), "row corrupted: {joined}");
        }
        other => panic!("expected rows, got {other:?}"),
    }
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}
