//! End-to-end tests for the embedded `powdb::Database` facade — open, query,
//! close, reopen (durability), readonly reads, and the SQL frontend, all
//! in-process with no server.

use std::sync::atomic::{AtomicU64, Ordering};

use powdb::{Database, QueryResult, Value};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn fresh_dir(label: &str) -> std::path::PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "powdb_facade_{}_{}_{}",
        std::process::id(),
        label,
        id
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn open_write_read() {
    let dir = fresh_dir("owr");
    let mut db = Database::open(&dir).unwrap();
    db.query("type User { required name: str, age: int }")
        .unwrap();
    db.query(r#"insert User { name := "Ada", age := 36 }"#)
        .unwrap();
    match db.query("count(User)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
        other => panic!("expected scalar 1, got {other:?}"),
    }
}

#[test]
fn reopen_after_close_recovers() {
    let dir = fresh_dir("reopen");
    {
        let mut db = Database::open(&dir).unwrap();
        db.query("type T { required id: int, required v: int }")
            .unwrap();
        db.query("insert T { id := 1, v := 100 }").unwrap();
        db.close(); // explicit clean shutdown
    }
    let mut db = Database::open(&dir).unwrap();
    match db.query("count(T)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
        other => panic!("expected 1 row after reopen, got {other:?}"),
    }
}

#[test]
fn readonly_query_takes_shared_ref() {
    let dir = fresh_dir("ro");
    let mut db = Database::open(&dir).unwrap();
    db.query("type T { required id: int }").unwrap();
    db.query("insert T { id := 1 }").unwrap();
    // `&self` — concurrent readers don't need exclusive access.
    let db_ref: &Database = &db;
    match db_ref.query_readonly("count(T)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
        other => panic!("expected scalar 1, got {other:?}"),
    }
}

#[test]
fn query_sql_frontend_works() {
    let dir = fresh_dir("sql");
    let mut db = Database::open(&dir).unwrap();
    db.query("type User { required name: str, age: int }")
        .unwrap();
    db.query(r#"insert User { name := "Ada", age := 36 }"#)
        .unwrap();
    match db.query_sql("SELECT name FROM User").unwrap() {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("expected rows, got {other:?}"),
    }
}
