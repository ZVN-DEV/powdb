//! Guards around link-path projections and aggregates (2026-07-23 plan-quality
//! audit P3s).
//!
//! 1. A bare dotted path (`.user.name`) in a projection slot is token-identical
//!    to two comma-less fields (`.user .name`), so it used to silently parse as
//!    TWO fields and project Empty columns. It is now a hard parse error that
//!    tells the user to alias the table (`Order as o { o.user.name }`).
//! 2. An aggregate over a table expression that carries a nested or link
//!    projection used to either silently count parent rows or silently
//!    aggregate to 0. It is now a clear error; plain aggregates are unchanged.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_linkguard_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn fixture(name: &str) -> Engine {
    let mut e = Engine::new(&temp_dir(name)).unwrap();
    for q in [
        "type User { required unique id: int, required name: str }",
        "type Order { required id: int, user_id: int, required total: float }",
        "link Order.user -> User on user_id = id",
        "link User.orders -> Order on id = user_id",
        r#"insert User { id := 1, name := "alice" }"#,
        r#"insert User { id := 2, name := "bob" }"#,
        "insert Order { id := 1, user_id := 1, total := 5.0 }",
        "insert Order { id := 2, user_id := 1, total := 7.0 }",
        "insert Order { id := 3, user_id := 2, total := 9.0 }",
    ] {
        e.execute_powql(q)
            .unwrap_or_else(|err| panic!("fixture `{q}`: {err}"));
    }
    e
}

fn err_of(engine: &mut Engine, q: &str) -> String {
    match engine.execute_powql(q) {
        Err(e) => e.to_string(),
        Ok(ok) => panic!("expected `{q}` to error, got {ok:?}"),
    }
}

fn rows_of(engine: &mut Engine, q: &str) -> Vec<Vec<Value>> {
    match engine.execute_powql(q) {
        Ok(QueryResult::Rows { rows, .. }) => rows,
        other => panic!("expected rows from `{q}`, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// T1: bare dotted link path is a parse error, not a silent two-field split.
// ---------------------------------------------------------------------------

#[test]
fn bare_dotted_path_in_aliased_slot_is_a_parse_error() {
    let mut e = fixture("t1_aliased_slot");
    let msg = err_of(&mut e, "Order { .id, uname: .user.name }");
    assert!(
        msg.contains("alias the table"),
        "error should tell the user to alias the table, got: {msg}"
    );
    assert!(
        msg.contains("o.user.name"),
        "error should show the aliased link-path form, got: {msg}"
    );
}

#[test]
fn bare_dotted_path_in_bare_slot_is_a_parse_error() {
    let mut e = fixture("t1_bare_slot");
    let msg = err_of(&mut e, "Order { .user.name }");
    assert!(msg.contains("alias the table"), "got: {msg}");
}

#[test]
fn bare_dotted_path_inside_nested_block_is_a_parse_error() {
    let mut e = fixture("t1_nested_block");
    let msg = err_of(
        &mut e,
        "User as u { .name, x: Order as o filter o.user_id = u.id { .user.name } }",
    );
    assert!(msg.contains("alias the table"), "got: {msg}");
}

#[test]
fn bare_dotted_path_inside_link_block_is_a_parse_error() {
    let mut e = fixture("t1_link_block");
    let msg = err_of(
        &mut e,
        "User as u { .name, orders: u.orders { .user.name } }",
    );
    assert!(msg.contains("alias the table"), "got: {msg}");
}

#[test]
fn aliased_link_path_still_works() {
    let mut e = fixture("t1_aliased_ok");
    let rows = rows_of(
        &mut e,
        "Order as o filter o.id = 1 { o.id, uname: o.user.name }",
    );
    assert_eq!(rows, vec![vec![Value::Int(1), Value::Str("alice".into())]]);
}

#[test]
fn comma_separated_dotted_fields_still_work() {
    let mut e = fixture("t1_commas_ok");
    let rows = rows_of(&mut e, "Order filter .id = 1 { .id, .total }");
    assert_eq!(rows, vec![vec![Value::Int(1), Value::Float(5.0)]]);
}

// ---------------------------------------------------------------------------
// T2: aggregates over/inside nested or link projections are rejected.
// ---------------------------------------------------------------------------

#[test]
fn count_over_scalar_link_projection_errors_clearly() {
    let mut e = fixture("t2_count_scalar_link");
    let msg = err_of(&mut e, "count(Order as o { o.user.name })");
    assert!(
        msg.contains("aggregate") && msg.contains("link"),
        "error should name the aggregate-over-link rejection, got: {msg}"
    );
}

#[test]
fn count_over_block_link_projection_errors_clearly() {
    let mut e = fixture("t2_count_block_link");
    let msg = err_of(&mut e, "count(User as u { orders: u.orders { .id } })");
    assert!(msg.contains("aggregate"), "got: {msg}");
}

#[test]
fn count_over_nested_projection_errors_clearly() {
    let mut e = fixture("t2_count_nested");
    let msg = err_of(
        &mut e,
        "count(User as u { x: Order as o filter o.user_id = u.id { .total } })",
    );
    assert!(msg.contains("aggregate"), "got: {msg}");
}

#[test]
fn sum_with_lifted_link_path_argument_errors_instead_of_zero() {
    // The parser lifts a single unaliased projection field into the aggregate
    // argument; a link path there used to silently aggregate to 0.
    let mut e = fixture("t2_sum_link_arg");
    let msg = err_of(&mut e, "sum(Order as o { o.user.name })");
    assert!(
        msg.contains("aggregate") && msg.contains("link"),
        "got: {msg}"
    );
}

#[test]
fn aggregate_inside_nested_block_errors_clearly() {
    let mut e = fixture("t2_agg_inside_block");
    let msg = err_of(
        &mut e,
        "User as u { .name, orders: u.orders { c: count(.id) } }",
    );
    assert!(
        msg.contains("plain columns"),
        "nested-block fields must reject aggregates, got: {msg}"
    );
}

#[test]
fn plain_aggregates_are_unchanged() {
    let mut e = fixture("t2_plain_aggs");
    match e.execute_powql("count(Order)") {
        Ok(QueryResult::Scalar(Value::Int(3))) => {}
        other => panic!("count(Order) changed: {other:?}"),
    }
    match e.execute_powql("sum(Order { .total })") {
        Ok(QueryResult::Scalar(Value::Float(v))) if (v - 21.0).abs() < 1e-9 => {}
        other => panic!("sum(Order {{ .total }}) changed: {other:?}"),
    }
    match e.execute_powql("count(Order filter .user_id = 1)") {
        Ok(QueryResult::Scalar(Value::Int(2))) => {}
        other => panic!("filtered count changed: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// T3: EXPLAIN names links and their declared hop paths.
// ---------------------------------------------------------------------------

fn explain_text(engine: &mut Engine, q: &str) -> String {
    rows_of(engine, q)
        .into_iter()
        .flatten()
        .map(|v| match v {
            Value::Str(s) => s,
            other => panic!("explain row cell should be a string, got {other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn explain_names_scalar_link_hop_path() {
    let mut e = fixture("t3_explain_scalar");
    let plan = explain_text(&mut e, "explain Order as o { uname: o.user.name }");
    assert!(
        plan.contains("link uname: scalar to-one path o.user.name"),
        "EXPLAIN should name the scalar link and its declared path, got:\n{plan}"
    );
    assert!(
        plan.contains("hops [user] -> column name"),
        "EXPLAIN should show the hop chain and final column, got:\n{plan}"
    );
    assert!(
        plan.contains("resolved from catalog at execution"),
        "EXPLAIN should say resolution happens at execution, got:\n{plan}"
    );
    assert!(
        !plan.contains("(unresolved"),
        "opaque `(unresolved)` marker should be gone, got:\n{plan}"
    );
}

#[test]
fn explain_names_multi_hop_scalar_link_path() {
    let mut e = fixture("t3_explain_multihop");
    // No `company` link is declared; EXPLAIN never resolves against the
    // catalog, so the declared path alone is enough to format the plan.
    let plan = explain_text(&mut e, "explain Order as o { c: o.user.company.name }");
    assert!(
        plan.contains("hops [user, company] -> column name"),
        "multi-hop chain should list every hop, got:\n{plan}"
    );
}

#[test]
fn explain_names_block_link_path() {
    let mut e = fixture("t3_explain_block");
    let plan = explain_text(
        &mut e,
        "explain User as u { .name, orders: u.orders { .id } }",
    );
    assert!(
        plan.contains("nested orders: to-many link u.orders"),
        "EXPLAIN should name the block link and its path, got:\n{plan}"
    );
    assert!(
        plan.contains("resolved from catalog at execution"),
        "got:\n{plan}"
    );
    assert!(!plan.contains("(unresolved"), "got:\n{plan}");
}
