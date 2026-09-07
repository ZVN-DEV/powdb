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

/// The unaliased spelling of the same mistake. `Post filter .author.name = ...`
/// reached a different parser production than `Post as p filter p.author.name`
/// and kept the old opaque message ("unexpected trailing token near token 3:
/// field '.name'"), so the fix covered one of the two ways to write it.
#[test]
fn the_unaliased_link_path_says_the_same_thing() {
    let (_dir, mut engine) = engine();
    for query in [
        "Post filter .author.name = \"ann\" { .id }",
        "Post order .author.name { .id }",
        "Post group .author.name { c: count(.id) }",
        "count(Post filter .author.name = \"ann\")",
        "Post filter .author.name = \"ann\" delete",
    ] {
        let message = message(&mut engine, query);
        assert!(
            message.contains("link traversal") && message.contains("projection"),
            "`{query}` was refused with {message}"
        );
        assert!(
            !message.contains("trailing token"),
            "`{query}` still gets the opaque message: {message}"
        );
    }
}

/// The two spellings are the same mistake and must read the same, apart from
/// the path each one quotes.
#[test]
fn both_spellings_carry_the_same_guidance() {
    let (_dir, mut engine) = engine();
    let aliased = message(
        &mut engine,
        "Post as p filter p.author.name = \"ann\" { p.id }",
    );
    let unaliased = message(&mut engine, "Post filter .author.name = \"ann\" { .id }");
    let guidance = "is only supported in a projection";
    let (_, aliased_tail) = aliased.split_once(guidance).unwrap_or_else(|| {
        panic!("the aliased message no longer carries the shared guidance: {aliased}")
    });
    let (_, unaliased_tail) = unaliased.split_once(guidance).unwrap_or_else(|| {
        panic!("the unaliased message does not carry the shared guidance: {unaliased}")
    });
    assert_eq!(
        aliased_tail, unaliased_tail,
        "the two spellings must give the same advice"
    );
    assert!(
        aliased.contains("'p.author.name'"),
        "the aliased message must quote what was written: {aliased}"
    );
    assert!(
        unaliased.contains("'.author.name'"),
        "the unaliased message must quote what was written: {unaliased}"
    );
}

/// A bare `.column` is untouched: only two adjacent dotted parts are a path.
#[test]
fn an_unqualified_column_is_untouched() {
    let (_dir, mut engine) = engine();
    engine
        .execute_powql("Post filter .uid = 1 { .id }")
        .unwrap();
    engine.execute_powql("Post order .uid { .id }").unwrap();
    engine
        .execute_powql("Post group .uid { c: count(.id) }")
        .unwrap();
}
