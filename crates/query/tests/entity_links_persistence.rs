//! Entity links survive a restart: the whole point of catalog v7 over the
//! in-memory prototype. A link declared through the query `Engine` is sourced
//! from the persistent catalog, so dropping and reopening the `Engine` on the
//! same data directory keeps the link usable in a traversal query.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::catalog::{read_active_catalog_version, CATALOG_VERSION};
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_entity_links_persist_{name}_{}_{}",
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

#[test]
fn link_survives_engine_reopen_and_stays_usable() {
    let dir = temp_dir("reopen");

    // First session: declare types + a link + rows, then close the engine.
    {
        let mut engine = Engine::new(&dir).unwrap();
        exec(
            &mut engine,
            "type User { required unique id: int, required name: str }",
        );
        exec(
            &mut engine,
            "type Order { required id: int, user_id: int, required total: float }",
        );
        exec(&mut engine, "link Order.user -> User on user_id = id");
        exec(&mut engine, r#"insert User { id := 1, name := "alice" }"#);
        exec(
            &mut engine,
            "insert Order { id := 1, user_id := 1, total := 9.5 }",
        );

        // The link resolves in this session.
        let (_, rows) = rows_of(exec(&mut engine, "Order as o { o.total, o.user.name }"));
        assert_eq!(rows[0][1], Value::Str("alice".into()));
    }

    // Declaring a link activated catalog v7 on disk.
    assert_eq!(
        read_active_catalog_version(&dir).unwrap(),
        CATALOG_VERSION,
        "declaring a link must persist catalog v7"
    );

    // Second session on the SAME data dir: the link is still there, sourced
    // from the persistent catalog (no in-memory registry to lose it).
    {
        let mut engine = Engine::new(&dir).unwrap();
        let (cols, rows) = rows_of(exec(&mut engine, "Order as o { o.total, o.user.name }"));
        assert_eq!(cols, vec!["o.total".to_string(), "o.user.name".to_string()]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], vec![Value::Float(9.5), Value::Str("alice".into())]);

        // And a link declared in the SECOND session also persists and traverses
        // (the write path routes to the same persistent catalog).
        exec(&mut engine, "link Order.buyer -> User on user_id = id");
        let (_, rows) = rows_of(exec(&mut engine, "Order as o { o.buyer.name }"));
        assert_eq!(rows[0][0], Value::Str("alice".into()));
    }

    // The second-session link is durable too.
    {
        let mut engine = Engine::new(&dir).unwrap();
        let (_, rows) = rows_of(exec(&mut engine, "Order as o { o.buyer.name }"));
        assert_eq!(rows[0][0], Value::Str("alice".into()));
    }
}

#[test]
fn fresh_db_stays_pre_v7_until_the_first_link_activates_v7() {
    let dir = temp_dir("activation");
    let mut engine = Engine::new(&dir).unwrap();
    exec(
        &mut engine,
        "type User { required unique id: int, required name: str }",
    );
    exec(
        &mut engine,
        "type Order { required id: int, user_id: int, required total: float }",
    );

    // A DB with tables but no links has not activated catalog v7.
    let before = read_active_catalog_version(&dir).unwrap();
    assert!(
        before < CATALOG_VERSION,
        "a link-free database must stay below catalog v7, got {before}"
    );

    // Declaring the first link lazily activates v7.
    exec(&mut engine, "link Order.user -> User on user_id = id");
    let after = read_active_catalog_version(&dir).unwrap();
    assert_eq!(
        after, CATALOG_VERSION,
        "the first link must activate catalog v7"
    );
}

#[test]
fn block_link_also_survives_reopen() {
    let dir = temp_dir("block_reopen");
    {
        let mut engine = Engine::new(&dir).unwrap();
        exec(
            &mut engine,
            "type User { required unique id: int, required name: str }",
        );
        exec(
            &mut engine,
            "type Order { required id: int, user_id: int, required total: float }",
        );
        // to-many link (user_id is not unique on Order).
        exec(&mut engine, "link User.orders -> Order on id = user_id");
        exec(&mut engine, r#"insert User { id := 1, name := "alice" }"#);
        exec(
            &mut engine,
            "insert Order { id := 1, user_id := 1, total := 9.5 }",
        );
        exec(
            &mut engine,
            "insert Order { id := 2, user_id := 1, total := 20.25 }",
        );
    }
    {
        let mut engine = Engine::new(&dir).unwrap();
        let (_, rows) = rows_of(exec(
            &mut engine,
            "User as u { u.name, orders: u.orders { total } }",
        ));
        assert_eq!(rows.len(), 1);
        // The block traversal still resolves against the reopened catalog.
        match &rows[0][1] {
            Value::Json(_) => {}
            other => panic!("expected a JSON array of child orders, got {other:?}"),
        }
    }
}
