//! Language-lab slice: nested/shaped results in a PowQL projection.
//!
//! `User as u { u.name, orders: Order as o filter o.user_id = u.id { ... } }`
//! emits one row per parent with the matching child rows assembled into a
//! JSON array of objects (the engine's native PJ1 type). No row explosion,
//! no NULL sentinel rows: a parent with zero children gets `[]`.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::pj1::parse_json_text;
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_nested_{name}_{}_{}",
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

/// Fresh engine with User/Order and the fixture rows: alice has two orders,
/// bob has one (with no product_id), cara has none.
fn engine_with_users_and_orders(name: &str) -> Engine {
    let mut engine = Engine::new(&temp_dir(name)).unwrap();
    exec(
        &mut engine,
        "type User { required id: int, required name: str }",
    );
    exec(
        &mut engine,
        "type Order { required id: int, required user_id: int, required total: float, product_id: int }",
    );
    exec(&mut engine, r#"insert User { id := 1, name := "alice" }"#);
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
    engine
}

fn assert_nested_shape(result: QueryResult) {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("expected rows, got {result:?}");
    };
    assert_eq!(columns, vec!["u.name".to_string(), "orders".to_string()]);
    // Exactly one output row per parent: no row explosion, no NULL rows.
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], Value::Str("alice".into()));
    assert_eq!(
        rows[0][1],
        json(r#"[{"total":9.5,"product_id":101},{"total":20.25,"product_id":102}]"#)
    );
    assert_eq!(rows[1][0], Value::Str("bob".into()));
    // Missing optional child column maps to JSON null.
    assert_eq!(rows[1][1], json(r#"[{"total":5.5,"product_id":null}]"#));
    assert_eq!(rows[2][0], Value::Str("cara".into()));
    // Zero matching children: empty array, parent row still present.
    assert_eq!(rows[2][1], json("[]"));
}

#[test]
fn nested_projection_emits_json_arrays_without_row_explosion() {
    let mut engine = engine_with_users_and_orders("basic");
    let result = exec(
        &mut engine,
        "User as u { u.name, orders: Order as o filter o.user_id = u.id { o.total, o.product_id } }",
    );
    assert_nested_shape(result);
}

#[test]
fn nested_projection_correlation_sides_are_symmetric() {
    let mut engine = engine_with_users_and_orders("reversed");
    // Same correlation with the outer column on the left.
    let result = exec(
        &mut engine,
        "User as u { u.name, orders: Order as o filter u.id = o.user_id { o.total, o.product_id } }",
    );
    assert_nested_shape(result);
}

#[test]
fn nested_projection_residual_conditions_are_rejected() {
    let mut engine = engine_with_users_and_orders("residual");
    // Slice scope: exactly one equi-correlation predicate. AND-ed residuals
    // must fail with a clear error, not silently misfilter.
    let err = engine
        .execute_powql(
            "User as u { u.name, orders: Order as o filter o.user_id = u.id and o.total > 10.0 { o.total } }",
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("correlation"),
        "expected a clear correlation-predicate error, got: {err}"
    );
}

#[test]
fn plain_queries_are_unchanged() {
    let mut engine = engine_with_users_and_orders("smoke");
    let QueryResult::Rows { columns, rows } =
        exec(&mut engine, r#"User filter .id = 1 { .name }"#)
    else {
        panic!("expected rows");
    };
    assert_eq!(columns, vec!["name".to_string()]);
    assert_eq!(rows, vec![vec![Value::Str("alice".into())]]);
}
