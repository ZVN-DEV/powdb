//! Link introspection: `schema links` lists every declared entity link as
//! ordinary result rows, and `describe <Type>` appends the type's links after
//! its column rows (outgoing first, then links targeting the type).
//!
//! ```text
//! schema links
//! ```
//!
//! | owner | name | target | local_key | target_key | cardinality |
//!
//! Rows are ordered by owner, then link name, so drivers can diff output
//! across runs. `describe` keeps its existing four columns and rows
//! byte-for-byte; link rows are appended with `type = "link"` so existing
//! consumers that only read column rows are unaffected.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_link_introspection_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn exec(engine: &mut Engine, q: &str) -> QueryResult {
    engine
        .execute_powql(q)
        .unwrap_or_else(|e| panic!("failed `{q}`: {e}"))
}

fn rows_of(result: QueryResult) -> (Vec<String>, Vec<Vec<Value>>) {
    match result {
        QueryResult::Rows { columns, rows } => (columns, rows),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn s(v: &str) -> Value {
    Value::Str(v.to_string())
}

/// Fixture: `Company <- User <- Order` with a to-one chain plus a to-many
/// reverse link, declared out of alphabetical order to prove the listing
/// sorts by (owner, name) instead of declaration order.
fn fixture(name: &str) -> Engine {
    let mut engine = Engine::new(&temp_dir(name)).unwrap();
    exec(
        &mut engine,
        "type Company { required unique id: int, required name: str }",
    );
    exec(
        &mut engine,
        "type User { required unique id: int, required name: str, company_id: int }",
    );
    exec(
        &mut engine,
        "type Order { required id: int, user_id: int, required total: float }",
    );
    // Declared out of (owner, name) order on purpose.
    exec(
        &mut engine,
        "link User.company -> Company on company_id = id",
    );
    exec(&mut engine, "link Order.user -> User on user_id = id");
    // to-many: Order.id is not unique.
    exec(&mut engine, "link User.orders -> Order on id = user_id");
    engine
}

const LINK_COLUMNS: [&str; 6] = [
    "owner",
    "name",
    "target",
    "local_key",
    "target_key",
    "cardinality",
];

#[test]
fn schema_links_on_empty_db_returns_zero_rows() {
    let mut engine = Engine::new(&temp_dir("empty")).unwrap();
    let (columns, rows) = rows_of(exec(&mut engine, "schema links"));
    assert_eq!(columns, LINK_COLUMNS);
    assert!(rows.is_empty(), "expected no rows, got {rows:?}");
}

#[test]
fn schema_links_lists_links_sorted_by_owner_then_name() {
    let mut engine = fixture("list");
    let (columns, rows) = rows_of(exec(&mut engine, "schema links"));
    assert_eq!(columns, LINK_COLUMNS);
    assert_eq!(
        rows,
        vec![
            vec![
                s("Order"),
                s("user"),
                s("User"),
                s("user_id"),
                s("id"),
                s("to-one"),
            ],
            vec![
                s("User"),
                s("company"),
                s("Company"),
                s("company_id"),
                s("id"),
                s("to-one"),
            ],
            vec![
                s("User"),
                s("orders"),
                s("Order"),
                s("id"),
                s("user_id"),
                s("to-many"),
            ],
        ]
    );
}

#[test]
fn describe_appends_outgoing_and_incoming_link_rows() {
    let mut engine = fixture("describe");
    let (columns, rows) = rows_of(exec(&mut engine, "describe User"));
    // Existing shape is untouched: same four columns, column rows first.
    assert_eq!(columns, ["column", "type", "nullable", "index"]);
    assert_eq!(
        rows[..3],
        vec![
            vec![s("id"), s("int"), Value::Bool(false), s("unique")],
            vec![s("name"), s("str"), Value::Bool(false), s("")],
            vec![s("company_id"), s("int"), Value::Bool(true), s("")],
        ]
    );
    // Appended: outgoing links (by name), then links targeting the type
    // (by owner, name), each qualified as `Owner.name`.
    assert_eq!(
        rows[3..],
        vec![
            vec![
                s("company"),
                s("link"),
                Value::Empty,
                s("-> Company (to-one, company_id -> id)"),
            ],
            vec![
                s("orders"),
                s("link"),
                Value::Empty,
                s("-> Order (to-many, id -> user_id)"),
            ],
            vec![
                s("Order.user"),
                s("link"),
                Value::Empty,
                s("<- Order (to-one, user_id -> id)"),
            ],
        ]
    );
}

#[test]
fn describe_without_links_is_unchanged() {
    let mut engine = Engine::new(&temp_dir("no_links")).unwrap();
    exec(
        &mut engine,
        "type Tag { required unique id: int, label: str }",
    );
    let (columns, rows) = rows_of(exec(&mut engine, "describe Tag"));
    assert_eq!(columns, ["column", "type", "nullable", "index"]);
    assert_eq!(
        rows,
        vec![
            vec![s("id"), s("int"), Value::Bool(false), s("unique")],
            vec![s("label"), s("str"), Value::Bool(true), s("")],
        ]
    );
}

#[test]
fn schema_type_alias_matches_describe_link_rows() {
    let mut engine = fixture("alias");
    let via_describe = rows_of(exec(&mut engine, "describe Order"));
    let via_schema = rows_of(exec(&mut engine, "schema Order"));
    assert_eq!(via_describe, via_schema);
}

#[test]
fn link_introspection_survives_restart() {
    let dir = temp_dir("restart");
    let (links_before, describe_before) = {
        let mut engine = Engine::new(&dir).unwrap();
        exec(
            &mut engine,
            "type Author { required unique id: int, name: str }",
        );
        exec(
            &mut engine,
            "type Book { required id: int, author_id: int, title: str }",
        );
        exec(&mut engine, "link Book.author -> Author on author_id = id");
        (
            rows_of(exec(&mut engine, "schema links")),
            rows_of(exec(&mut engine, "describe Book")),
        )
    };
    let mut engine = Engine::new(&dir).unwrap();
    assert_eq!(rows_of(exec(&mut engine, "schema links")), links_before);
    assert_eq!(rows_of(exec(&mut engine, "describe Book")), describe_before);
    assert_eq!(
        links_before.1,
        vec![vec![
            s("Book"),
            s("author"),
            s("Author"),
            s("author_id"),
            s("id"),
            s("to-one"),
        ]]
    );
}
