//! E14: a `views.bin` that cannot be decoded refuses the open.
//!
//! The registry open was wrapped in `unwrap_or_else(|_| ViewRegistry::new(..))`,
//! so a truncated or corrupt view file was read as "this database has no
//! materialized views". Every `materialize` statement ever run then vanished:
//! reads of a view name reported an unknown table, the backing tables stayed on
//! disk with nobody to refresh them, and the next `materialize` of the same name
//! overwrote the file that still held the definitions. A damaged heap and a
//! missing catalog both refuse the open, so this one does too.

use powdb_query::executor::Engine;

/// A data directory with one materialized view, closed cleanly.
fn seeded() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int }")
        .unwrap();
    engine.execute_powql("insert T { id := 1 }").unwrap();
    engine.execute_powql("materialize V as T { .id }").unwrap();
    drop(engine);
    dir
}

fn view_file(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("views.bin")
}

#[test]
fn a_truncated_view_file_refuses_the_open() {
    let dir = seeded();
    let path = view_file(&dir);
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..bytes.len() - 4]).unwrap();
    let message = Engine::new(dir.path())
        .map(|_| panic!("a truncated view file must not open as a database with no views"))
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("views.bin"),
        "the error must name the file to restore, got {message}"
    );
}

#[test]
fn a_view_file_with_a_bad_header_refuses_the_open() {
    let dir = seeded();
    let path = view_file(&dir);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] = b'X';
    std::fs::write(&path, &bytes).unwrap();
    let message = Engine::new(dir.path())
        .map(|_| panic!("a corrupt view file must not open as a database with no views"))
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("views.bin"),
        "the error must name the file to restore, got {message}"
    );
}

#[test]
fn a_database_with_no_view_file_still_opens() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int }")
        .unwrap();
    drop(engine);
    assert!(!view_file(&dir).exists(), "no view was ever materialized");
    Engine::new(dir.path()).unwrap();
}

#[test]
fn an_intact_view_file_still_opens_and_serves_the_view() {
    let dir = seeded();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine.execute_powql("count(V)").unwrap();
}
