//! PowQL entity-link traversal over the persistent catalog (catalog v7).
//!
//! A link is declared as a bare statement:
//!
//! ```text
//! link Order.user -> User on user_id = id
//! ```
//!
//! or as an alter action (`alter Order add link user -> User on user_id = id`).
//! The `on <local> = <target>` clause reads "the owner's local key equals the
//! target's target key". Cardinality is derived by the catalog from whether the
//! target key is unique: a unique target key is a to-one (scalar) link, else a
//! to-many (block) link.
//!
//! A projection can then traverse a link:
//!
//! ```text
//! Order as o { o.total, o.user.name }                 // scalar hop (to-one)
//! User  as u { u.name, u.orders { total, status } }   // block (to-many)
//! ```

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::pj1::parse_json_text;
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_entity_links_{name}_{}_{}",
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

/// Canonical PJ1 value for a JSON text literal, for order-insensitive
/// object-key comparison against a returned `Value::Json`.
fn json(text: &str) -> Value {
    Value::Json(parse_json_text(text).unwrap().into())
}

fn rows_of(result: QueryResult) -> (Vec<String>, Vec<Vec<Value>>) {
    match result {
        QueryResult::Rows { columns, rows } => (columns, rows),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Fixture: `Company <- User <- Order`, with all three links declared as bare
/// statements after the types exist. alice (company acme) has orders 1 and 2,
/// bob (no company) has order 3, order 4 has a NULL user_id, order 5 dangles
/// (user_id 99 has no user). cara (user 3) has no orders.
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
        "type Order { required id: int, user_id: int, required total: float, product_id: int }",
    );
    // to-one links (target key is unique)
    exec(&mut engine, "link Order.user -> User on user_id = id");
    exec(
        &mut engine,
        "link User.company -> Company on company_id = id",
    );
    // to-many link (target key user_id is not unique)
    exec(&mut engine, "link User.orders -> Order on id = user_id");

    exec(
        &mut engine,
        r#"insert Company { id := 10, name := "acme" }"#,
    );
    exec(
        &mut engine,
        r#"insert User { id := 1, name := "alice", company_id := 10 }"#,
    );
    exec(&mut engine, r#"insert User { id := 2, name := "bob" }"#);
    exec(&mut engine, r#"insert User { id := 3, name := "cara" }"#);
    exec(
        &mut engine,
        "insert Order { id := 1, user_id := 1, total := 9.5, product_id := 101 }",
    );
    exec(
        &mut engine,
        "insert Order { id := 2, user_id := 1, total := 20.25, product_id := 102 }",
    );
    exec(
        &mut engine,
        "insert Order { id := 3, user_id := 2, total := 5.5 }",
    );
    exec(&mut engine, "insert Order { id := 4, total := 1.0 }");
    exec(
        &mut engine,
        "insert Order { id := 5, user_id := 99, total := 2.0 }",
    );
    engine
}

#[test]
fn ddl_bare_and_alter_forms_both_declare_a_usable_link() {
    let mut engine = Engine::new(&temp_dir("ddl")).unwrap();
    exec(
        &mut engine,
        "type User { required unique id: int, required name: str }",
    );
    exec(
        &mut engine,
        "type Order { required id: int, user_id: int, required total: float }",
    );
    // Bare statement form.
    exec(&mut engine, "link Order.user -> User on user_id = id");
    // Alter-action form (a second link name on the same owner).
    exec(
        &mut engine,
        "alter Order add link buyer -> User on user_id = id",
    );
    exec(&mut engine, r#"insert User { id := 1, name := "alice" }"#);
    exec(
        &mut engine,
        "insert Order { id := 1, user_id := 1, total := 9.5 }",
    );
    let (_, rows) = rows_of(exec(
        &mut engine,
        "Order as o { o.user.name, o.buyer.name }",
    ));
    assert_eq!(rows[0][0], Value::Str("alice".into()));
    assert_eq!(rows[0][1], Value::Str("alice".into()));
}

#[test]
fn ddl_explain_shows_create_link_without_executing() {
    let mut engine = Engine::new(&temp_dir("ddl_explain")).unwrap();
    exec(
        &mut engine,
        "type User { required unique id: int, required name: str }",
    );
    exec(
        &mut engine,
        "type Order { required id: int, user_id: int, required total: float }",
    );
    let (_, rows) = rows_of(exec(
        &mut engine,
        "explain link Order.user -> User on user_id = id",
    ));
    let text: String = rows.iter().map(|r| format!("{r:?}")).collect();
    assert!(
        text.contains("CreateLink") && text.contains("Order.user"),
        "explain should show the CreateLink node, got: {text}"
    );
    // EXPLAIN did not execute the DDL, so a real declaration still succeeds.
    exec(&mut engine, "link Order.user -> User on user_id = id");
}

#[test]
fn scalar_hop_reads_target_column_and_never_drops_rows() {
    let mut engine = fixture("scalar");
    let (cols, rows) = rows_of(exec(&mut engine, "Order as o { o.total, o.user.name }"));
    assert_eq!(cols, vec!["o.total".to_string(), "o.user.name".to_string()]);
    // Five orders in, five rows out: NULL and dangling FKs do not drop rows.
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[0], vec![Value::Float(9.5), Value::Str("alice".into())]);
    assert_eq!(
        rows[1],
        vec![Value::Float(20.25), Value::Str("alice".into())]
    );
    assert_eq!(rows[2], vec![Value::Float(5.5), Value::Str("bob".into())]);
    // NULL FK (order 4) and dangling FK (order 5) both traverse to Empty.
    assert_eq!(rows[3], vec![Value::Float(1.0), Value::Empty]);
    assert_eq!(rows[4], vec![Value::Float(2.0), Value::Empty]);
}

#[test]
fn scalar_hop_supports_a_field_alias() {
    let mut engine = fixture("scalar_alias");
    let (cols, rows) = rows_of(exec(
        &mut engine,
        "Order as o { o.total, buyer: o.user.name }",
    ));
    assert_eq!(cols, vec!["o.total".to_string(), "buyer".to_string()]);
    assert_eq!(rows[0][1], Value::Str("alice".into()));
}

#[test]
fn multi_hop_chains_hops_and_propagates_missing() {
    let mut engine = fixture("multihop");
    let (cols, rows) = rows_of(exec(
        &mut engine,
        "Order as o { o.total, o.user.company.name }",
    ));
    assert_eq!(
        cols,
        vec!["o.total".to_string(), "o.user.company.name".to_string()]
    );
    assert_eq!(rows.len(), 5);
    // alice -> acme on both her orders.
    assert_eq!(rows[0][1], Value::Str("acme".into()));
    assert_eq!(rows[1][1], Value::Str("acme".into()));
    // bob has no company: missing propagates through the second hop.
    assert_eq!(rows[2][1], Value::Empty);
    // NULL and dangling user_id: missing at the first hop stays missing.
    assert_eq!(rows[3][1], Value::Empty);
    assert_eq!(rows[4][1], Value::Empty);
}

#[test]
fn to_many_block_matches_explicit_nested_query() {
    let mut engine = fixture("block");
    let (via_cols, via_rows) = rows_of(exec(
        &mut engine,
        "User as u { u.name, orders: u.orders { total, product_id } }",
    ));
    let (exp_cols, exp_rows) = rows_of(exec(
        &mut engine,
        "User as u { u.name, orders: Order as o filter o.user_id = u.id { o.total, o.product_id } }",
    ));
    // Byte-identical to the explicit correlated spelling.
    assert_eq!(via_cols, exp_cols);
    assert_eq!(via_rows, exp_rows);

    assert_eq!(via_cols, vec!["u.name".to_string(), "orders".to_string()]);
    assert_eq!(via_rows.len(), 3);
    assert_eq!(via_rows[0][0], Value::Str("alice".into()));
    assert_eq!(
        via_rows[0][1],
        json(r#"[{"total":9.5,"product_id":101},{"total":20.25,"product_id":102}]"#)
    );
}

#[test]
fn to_many_block_composes_with_residual_order_limit() {
    let mut engine = fixture("block_compose");
    let via = "User as u { u.name, orders: u.orders filter total > 10.0 order total desc limit 1 { total } }";
    let explicit = "User as u { u.name, orders: Order as o filter o.user_id = u.id and o.total > 10.0 order o.total desc limit 1 { o.total } }";
    let (_, via_rows) = rows_of(exec(&mut engine, via));
    let (_, exp_rows) = rows_of(exec(&mut engine, explicit));
    assert_eq!(via_rows, exp_rows);
    // alice keeps only 20.25; bob's single 5.5 is filtered out.
    assert_eq!(via_rows[0][1], json(r#"[{"total":20.25}]"#));
}

#[test]
fn childless_to_many_parent_gets_empty_array() {
    let mut engine = fixture("childless");
    let (_, rows) = rows_of(exec(
        &mut engine,
        "User as u { u.name, orders: u.orders { total } }",
    ));
    // cara (user 3) has no orders.
    let cara = rows
        .iter()
        .find(|r| r[0] == Value::Str("cara".into()))
        .unwrap();
    assert_eq!(cara[1], json("[]"));
}

#[test]
fn block_through_to_one_link_is_a_kind_mismatch_error() {
    let mut engine = fixture("kind_block");
    // `o.user` is a to-one link; a block traversal must not pretend it is 1:N.
    let err = engine
        .execute_powql("Order as o { o.total, u: o.user { name } }")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("to-one"),
        "expected a to-one kind-mismatch error, got: {err}"
    );
}

#[test]
fn scalar_path_through_to_many_link_is_a_kind_mismatch_error() {
    let mut engine = fixture("kind_scalar");
    // `u.orders` is a to-many link; a scalar path must not silently pick one.
    let err = engine
        .execute_powql("User as u { u.name, u.orders.total }")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("to-many"),
        "expected a to-many kind-mismatch error, got: {err}"
    );
}

#[test]
fn scalar_hop_through_a_non_unique_key_is_a_hard_error() {
    // A link declared on a NON-unique target key derives to-many, so traversing
    // it as a scalar path is a hard error, never a silent fan-out. This is the
    // "correct by default" wedge SQL's JOIN cannot make.
    let mut engine = Engine::new(&temp_dir("nonunique")).unwrap();
    exec(
        &mut engine,
        "type U { required id: int, required name: str }",
    );
    exec(&mut engine, "type O { required id: int, u_id: int }");
    // U.id is NOT unique, so this is a to-many link.
    exec(&mut engine, "link O.u -> U on u_id = id");
    exec(&mut engine, r#"insert U { id := 1, name := "first" }"#);
    exec(&mut engine, r#"insert U { id := 1, name := "second" }"#);
    exec(&mut engine, "insert O { id := 1, u_id := 1 }");
    let err = engine
        .execute_powql("O as o { o.id, o.u.name }")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("to-many") && err.contains("unique"),
        "expected a non-unique / to-many error, got: {err}"
    );
}

#[test]
fn unknown_scalar_link_is_a_clean_error() {
    let mut engine = fixture("unknown_scalar");
    let err = engine
        .execute_powql("Order as o { o.total, o.nosuch.name }")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("nosuch") && err.contains("Order"),
        "expected an unknown-link error naming link and type, got: {err}"
    );
}

#[test]
fn unknown_block_link_is_a_clean_error() {
    let mut engine = fixture("unknown_block");
    let err = engine
        .execute_powql("User as u { u.name, friends: u.friends { name } }")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("friends") && err.contains("User"),
        "expected an unknown-link error naming link and type, got: {err}"
    );
}

#[test]
fn filter_on_link_path_is_a_clean_error() {
    let mut engine = fixture("filterpath");
    // A link path is only valid as a projection field, never in a filter.
    let err = engine
        .execute_powql(r#"Order as o filter o.user.name = "alice" { o.total }"#)
        .unwrap_err();
    let _ = err.to_string(); // any clean error is acceptable; no panic, no rows
}

#[test]
fn plain_queries_and_two_part_qualifiers_are_unchanged() {
    let mut engine = fixture("plain");
    // Two-part `o.total` keeps its meaning; only three or more parts traverse.
    let (cols, rows) = rows_of(exec(&mut engine, "Order as o { o.total, o.user.name }"));
    assert_eq!(cols.len(), 2);
    assert_eq!(rows.len(), 5);
    // An unaliased scan is untouched.
    let (cols, rows) = rows_of(exec(&mut engine, "Order { .id, .total }"));
    assert_eq!(cols, vec!["id".to_string(), "total".to_string()]);
    assert_eq!(rows.len(), 5);
    // The explicit nested-projection spelling still works untouched.
    let (cols, _) = rows_of(exec(
        &mut engine,
        "User as u { renamed: u.name, orders: Order as o filter o.user_id = u.id { o.total } }",
    ));
    assert_eq!(cols, vec!["renamed".to_string(), "orders".to_string()]);
}

#[test]
fn scalar_and_block_links_compose_in_one_projection() {
    let mut engine = fixture("compose");
    let (cols, rows) = rows_of(exec(
        &mut engine,
        "User as u { u.name, u.company.name, orders: u.orders { total } }",
    ));
    assert_eq!(
        cols,
        vec![
            "u.name".to_string(),
            "u.company.name".to_string(),
            "orders".to_string()
        ]
    );
    let alice = rows
        .iter()
        .find(|r| r[0] == Value::Str("alice".into()))
        .unwrap();
    assert_eq!(alice[1], Value::Str("acme".into()));
    assert_eq!(alice[2], json(r#"[{"total":9.5},{"total":20.25}]"#));
}

#[test]
fn link_traversal_is_never_served_stale_from_the_plan_cache() {
    let mut engine = fixture("plancache");

    // Run a scalar-hop query; order 5 dangles (user 99 absent).
    let (_, rows) = rows_of(exec(&mut engine, "Order as o { o.id, o.user.name }"));
    assert_eq!(rows[4][1], Value::Empty);

    // Insert the missing target row, then re-run the SAME query shape. A cached
    // (unresolved) plan must never be served: the second run must re-resolve
    // against the live catalog and observe the new target row.
    exec(&mut engine, r#"insert User { id := 99, name := "zed" }"#);
    let (hits_before, _, _) = engine.plan_cache_stats();
    let (_, rows) = rows_of(exec(&mut engine, "Order as o { o.id, o.user.name }"));
    let (hits_after, _, _) = engine.plan_cache_stats();
    assert_eq!(rows[4][1], Value::Str("zed".into()));
    assert_eq!(
        hits_after, hits_before,
        "a link-traversal statement must not be served from the plan cache"
    );

    // A block traversal likewise re-resolves: a newly inserted child appears.
    let (_, rows) = rows_of(exec(
        &mut engine,
        "User as u { u.id, orders: u.orders { total } }",
    ));
    let cara = rows.iter().find(|r| r[0] == Value::Int(3)).unwrap();
    assert_eq!(cara[1], json("[]"));
    exec(
        &mut engine,
        "insert Order { id := 6, user_id := 3, total := 7.0 }",
    );
    let (_, rows) = rows_of(exec(
        &mut engine,
        "User as u { u.id, orders: u.orders { total } }",
    ));
    let cara = rows.iter().find(|r| r[0] == Value::Int(3)).unwrap();
    assert_eq!(cara[1], json(r#"[{"total":7.0}]"#));
}
