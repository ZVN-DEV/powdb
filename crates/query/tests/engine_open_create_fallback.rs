//! Opening a damaged database silently re-initialized it.
//!
//! `Engine::new` treats a `NotFound` from `Catalog::open` as "there is no
//! database here" and calls `Catalog::create`, which writes a fresh empty
//! catalog over the existing one and truncates the WAL. Only one `NotFound`
//! actually means that: the one raised because `catalog.bin` is absent. Every
//! other file the open path touches can raise the same kind — a table's `.heap`
//! most obviously — and those all landed in the same arm.
//!
//! The observed failure: a two-table database, one `.heap` deleted, reopened.
//! `catalog.bin` was rewritten from 92 bytes to 14, `.tables` reported no
//! tables, and the *other* table became unreachable even though its heap was
//! still intact on disk. No error, and the log line said the engine had
//! initialized a fresh database.
//!
//! A missing heap for a table the catalog still lists is damage. It must be
//! loud, and it must leave the catalog alone so the heap can be restored.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQUE_DIR: AtomicU64 = AtomicU64::new(0);

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "powdb_openfallback_{tag}_{}_{}",
        std::process::id(),
        UNIQUE_DIR.fetch_add(1, Ordering::Relaxed),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("failed to execute `{query}`: {e}"))
}

fn count(engine: &mut Engine, query: &str) -> i64 {
    match exec(engine, query) {
        QueryResult::Scalar(Value::Int(n)) => n,
        other => panic!("`{query}`: expected a count, got {other:?}"),
    }
}

/// Two populated tables, then closed cleanly so the directory on disk is a
/// complete database.
fn two_table_database(dir: &std::path::Path) {
    let mut engine = Engine::new(dir).unwrap();
    exec(&mut engine, "type Keep { id: int }");
    exec(&mut engine, "type Gone { id: int }");
    for id in 1..=3i64 {
        exec(&mut engine, &format!("insert Keep {{ id := {id} }}"));
        exec(&mut engine, &format!("insert Gone {{ id := {id} }}"));
    }
    assert_eq!(count(&mut engine, "count(Keep)"), 3);
    assert_eq!(count(&mut engine, "count(Gone)"), 3);
    drop(engine);
}

#[test]
fn reopening_with_a_missing_heap_errors_instead_of_reinitializing() {
    let dir = fresh_dir("missing_heap");
    two_table_database(&dir);

    let catalog_path = dir.join("catalog.bin");
    let catalog_before =
        std::fs::read(&catalog_path).expect("catalog.bin exists after a clean run");
    assert!(
        !catalog_before.is_empty(),
        "fixture must leave a populated catalog"
    );

    let heap_path = dir.join("Gone.heap");
    let heap_before = std::fs::read(&heap_path).expect("the second table has a heap");
    std::fs::remove_file(&heap_path).expect("remove one table's heap");

    let error = Engine::new(&dir)
        .err()
        .expect("opening a database whose catalog lists a table with no heap must fail");
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::NotFound,
        "the missing heap must surface as the error it is, got: {error}"
    );

    assert_eq!(
        std::fs::read(&catalog_path).expect("catalog.bin must survive the failed open"),
        catalog_before,
        "a failed open must not rewrite the catalog"
    );

    // The damage is recoverable precisely because nothing was overwritten:
    // restore the heap from a backup and both tables come back with their rows.
    // Had the open re-initialized the directory, this would be unrecoverable.
    std::fs::write(&heap_path, &heap_before).expect("restore the heap");
    let mut engine = Engine::new(&dir).expect("reopen after restoring the heap");
    assert_eq!(count(&mut engine, "count(Keep)"), 3);
    assert_eq!(count(&mut engine, "count(Gone)"), 3);
}

/// The other half of the same arm: an empty directory really is a fresh
/// database and must still be created without complaint.
#[test]
fn a_directory_with_no_catalog_still_initializes_fresh() {
    let dir = fresh_dir("empty");
    let mut engine = Engine::new(&dir).expect("an empty directory is a fresh database");
    assert!(dir.join("catalog.bin").exists());
    exec(&mut engine, "type T { id: int }");
    exec(&mut engine, "insert T { id := 1 }");
    assert_eq!(count(&mut engine, "count(T)"), 1);
}

/// An intact database still reopens and keeps its rows.
#[test]
fn reopening_an_intact_database_still_works() {
    let dir = fresh_dir("intact");
    two_table_database(&dir);
    let mut engine = Engine::new(&dir).expect("reopen an undamaged database");
    assert_eq!(count(&mut engine, "count(Keep)"), 3);
    assert_eq!(count(&mut engine, "count(Gone)"), 3);
}
