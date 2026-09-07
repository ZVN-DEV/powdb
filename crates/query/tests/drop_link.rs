//! Q8 (second half): a declared link can be dropped.
//!
//! `link Post.author -> User on uid = id` could be declared and never removed:
//! `drop link Post.author` reported "expected table name after drop" and
//! `alter Post drop link author` reported that `link` is a reserved word. The
//! only way out was to drop the owning type. The catalog has had
//! `Catalog::drop_link` all along; nothing reached it.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type User { required unique id: int, name: str }")
        .unwrap();
    engine
        .execute_powql("type Post { required unique id: int, uid: int }")
        .unwrap();
    engine
        .execute_powql("link Post.author -> User on uid = id")
        .unwrap();
    engine
        .execute_powql("insert User { id := 1, name := \"ann\" }")
        .unwrap();
    engine
        .execute_powql("insert Post { id := 10, uid := 1 }")
        .unwrap();
    (dir, engine)
}

fn link_names(engine: &mut Engine) -> Vec<String> {
    match engine.execute_powql("schema links").unwrap() {
        QueryResult::Rows { rows, .. } => rows.iter().map(|row| format!("{:?}", row[1])).collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn a_link_can_be_dropped_by_its_qualified_name() {
    let (_dir, mut engine) = engine();
    engine
        .execute_powql("Post as p { p.id, p.author.name }")
        .unwrap();
    engine.execute_powql("drop link Post.author").unwrap();
    assert!(link_names(&mut engine).is_empty(), "the link is gone");
    assert!(
        engine
            .execute_powql("Post as p { p.id, p.author.name }")
            .is_err(),
        "traversal must stop working once the link is gone"
    );
    // The owner keeps its rows and its columns.
    engine.execute_powql("Post { .id, .uid }").unwrap();
}

#[test]
fn the_alter_spelling_drops_it_too() {
    let (_dir, mut engine) = engine();
    engine.execute_powql("alter Post drop link author").unwrap();
    assert!(link_names(&mut engine).is_empty());
}

#[test]
fn dropping_a_link_that_is_not_there_is_an_error_unless_if_exists() {
    let (_dir, mut engine) = engine();
    assert!(engine.execute_powql("drop link Post.nope").is_err());
    engine
        .execute_powql("drop link if exists Post.nope")
        .unwrap();
    engine
        .execute_powql("alter Post drop link if exists nope")
        .unwrap();
}

#[test]
fn the_link_stays_dropped_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type User { required unique id: int, name: str }")
            .unwrap();
        engine
            .execute_powql("type Post { required unique id: int, uid: int }")
            .unwrap();
        engine
            .execute_powql("link Post.author -> User on uid = id")
            .unwrap();
        engine.execute_powql("drop link Post.author").unwrap();
    }
    let mut reopened = Engine::new(dir.path()).unwrap();
    assert!(link_names(&mut reopened).is_empty());
    // And the name is free to declare again.
    reopened
        .execute_powql("link Post.author -> User on uid = id")
        .unwrap();
}

#[test]
fn a_repeated_traversal_does_not_survive_in_the_plan_cache() {
    let (_dir, mut engine) = engine();
    for _ in 0..3 {
        engine
            .execute_powql("Post as p { p.id, p.author.name }")
            .unwrap();
    }
    engine.execute_powql("drop link Post.author").unwrap();
    assert!(
        engine
            .execute_powql("Post as p { p.id, p.author.name }")
            .is_err(),
        "a cached plan must not keep traversing a link that is gone"
    );
}
