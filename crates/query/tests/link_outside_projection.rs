//! Q8: traversing a link outside a projection says what the limitation is.
//!
//! `Post as p { p.author.name }` reads through the link, but the same path in
//! `filter`, `order` or `group` reached the generic expression parser, which
//! read `p.author` as a qualified column and then had a `.name` it could not
//! place: "unexpected trailing token near token 6: field '.name'". Nothing in
//! that message says the word link, so the reader cannot tell a typo from a
//! feature that is not there.

use powdb_query::executor::Engine;

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

fn message(engine: &mut Engine, query: &str) -> String {
    engine
        .execute_powql(query)
        .map(|ok| panic!("`{query}` should have been refused, got {ok:?}"))
        .unwrap_err()
        .to_string()
}

#[test]
fn a_link_path_outside_a_projection_names_the_limitation() {
    let (_dir, mut engine) = engine();
    for query in [
        "Post as p filter p.author.name = \"ann\" { p.id }",
        "Post as p order p.author.name { p.id }",
        "Post as p group p.author.name { c: count(p.id) }",
        "count(Post as p filter p.author.name = \"ann\")",
        "Post as p filter p.author.name = \"ann\" delete",
    ] {
        let message = message(&mut engine, query);
        assert!(
            message.contains("link traversal") && message.contains("projection"),
            "`{query}` was refused with {message}"
        );
    }
}

#[test]
fn the_projection_forms_still_work() {
    let (_dir, mut engine) = engine();
    engine
        .execute_powql("Post as p { p.id, p.author.name }")
        .unwrap();
    engine
        .execute_powql("Post as p filter p.uid = 1 { p.id, p.author.name }")
        .unwrap();
}

#[test]
fn a_plain_qualified_column_is_untouched() {
    let (_dir, mut engine) = engine();
    engine
        .execute_powql("Post as p filter p.uid = 1 { p.id }")
        .unwrap();
    engine
        .execute_powql("Post as p join User as u on p.uid = u.id filter u.name = \"ann\" { p.id }")
        .unwrap();
}
