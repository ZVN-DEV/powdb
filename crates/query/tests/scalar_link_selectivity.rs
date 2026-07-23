//! Scalar-link (`o.user.name`) parent selectivity: a to-one link's target key
//! is unique, so a selective outer query only needs a point probe per FK value
//! it references instead of a full scan of the target table. These tests lock
//! the semantics of the probe path to the full-scan path (results identical)
//! and prove the probe path does not materialize the whole target table under
//! a selective outer filter.
//!
//! Correctness contract mirrored from `entity_links.rs`: NULL FK -> Empty,
//! dangling FK -> Empty (LEFT JOIN semantics, parent never dropped), multi-hop
//! still correct, and a non-unique hop is still a hard error.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_scalar_sel_{name}_{}_{}",
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

fn rows_of(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// alice(1)->acme(10), bob(2)->no company, cara(3)->company 20 (dangling).
/// Orders: 1,2 -> alice; 3 -> bob; 4 -> NULL user; 5 -> user 99 (dangling).
/// Every to-one target key (`User.id`, `Company.id`) is unique, so the probe
/// path is eligible whenever the outer query is selective.
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
    exec(&mut engine, "link Order.user -> User on user_id = id");
    exec(
        &mut engine,
        "link User.company -> Company on company_id = id",
    );
    exec(
        &mut engine,
        r#"insert Company { id := 10, name := "acme" }"#,
    );
    exec(
        &mut engine,
        r#"insert User { id := 1, name := "alice", company_id := 10 }"#,
    );
    exec(&mut engine, r#"insert User { id := 2, name := "bob" }"#);
    exec(
        &mut engine,
        r#"insert User { id := 3, name := "cara", company_id := 20 }"#,
    );
    exec(
        &mut engine,
        "insert Order { id := 1, user_id := 1, total := 9.5 }",
    );
    exec(
        &mut engine,
        "insert Order { id := 2, user_id := 1, total := 20.25 }",
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

/// The scalar-link value the FULL-SCAN path produces for one order id, read out
/// of the fleet-shaped (all-parents) query that never triggers a probe. Used as
/// the oracle the selective probe path must match byte-for-byte.
fn scan_value_for_order(engine: &mut Engine, path: &str, order_id: i64) -> Value {
    let all = rows_of(exec(
        &mut *engine,
        &format!("Order as o {{ o.id, {path} }}"),
    ));
    all.into_iter()
        .find(|r| r[0] == Value::Int(order_id))
        .unwrap_or_else(|| panic!("order {order_id} not found"))[1]
        .clone()
}

#[test]
fn selective_single_parent_probe_matches_scan() {
    let mut engine = fixture("single");
    // Selective outer (one order) -> probe path.
    let rows = rows_of(exec(
        &mut engine,
        "Order as o filter o.id = 2 { o.user.name }",
    ));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Str("alice".into()));
    // ... identical to the full-scan path for the same order.
    assert_eq!(
        rows[0][0],
        scan_value_for_order(&mut engine, "o.user.name", 2)
    );
}

#[test]
fn null_fk_yields_empty_on_probe_path() {
    let mut engine = fixture("null_fk");
    // Order 4 has a NULL user_id: LEFT JOIN semantics -> Empty, and the probe
    // path must not probe the btree with a NULL key.
    let rows = rows_of(exec(
        &mut engine,
        "Order as o filter o.id = 4 { o.total, o.user.name }",
    ));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], Value::Empty);
    assert_eq!(
        rows[0][1],
        scan_value_for_order(&mut engine, "o.user.name", 4)
    );
}

#[test]
fn dangling_fk_yields_empty_on_probe_path() {
    let mut engine = fixture("dangling_fk");
    // Order 5 references user 99, which does not exist -> Empty, never dropped.
    let rows = rows_of(exec(
        &mut engine,
        "Order as o filter o.id = 5 { o.total, o.user.name }",
    ));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], Value::Empty);
    assert_eq!(
        rows[0][1],
        scan_value_for_order(&mut engine, "o.user.name", 5)
    );
}

#[test]
fn multi_hop_selective_probe_matches_scan() {
    let mut engine = fixture("multi_hop");
    // alice -> acme: a resolved value.
    let rows = rows_of(exec(
        &mut engine,
        "Order as o filter o.id = 1 { o.user.company.name }",
    ));
    assert_eq!(rows[0][0], Value::Str("acme".into()));
    assert_eq!(
        rows[0][0],
        scan_value_for_order(&mut engine, "o.user.company.name", 1)
    );
    // bob has no company_id (NULL at the second hop) -> Empty.
    let rows = rows_of(exec(
        &mut engine,
        "Order as o filter o.id = 3 { o.user.company.name }",
    ));
    assert_eq!(rows[0][0], Value::Empty);
    assert_eq!(
        rows[0][0],
        scan_value_for_order(&mut engine, "o.user.company.name", 3)
    );
    // cara -> company 20, which does not exist (dangling second hop) -> Empty.
    // Reached only by threading the first hop's output into the second probe.
    let mut engine2 = fixture("multi_hop_dangling");
    exec(
        &mut engine2,
        "insert Order { id := 6, user_id := 3, total := 7.0 }",
    );
    let rows = rows_of(exec(
        &mut engine2,
        "Order as o filter o.id = 6 { o.user.company.name }",
    ));
    assert_eq!(rows[0][0], Value::Empty);
    assert_eq!(
        rows[0][0],
        scan_value_for_order(&mut engine2, "o.user.company.name", 6)
    );
}

#[test]
fn selective_range_probe_matches_scan_for_every_row() {
    let mut engine = fixture("range");
    // A small-but-not-single selective set (orders 1..3) still probes; every
    // row must match the full-scan oracle.
    let probe_rows = rows_of(exec(
        &mut engine,
        "Order as o filter o.id >= 1 and o.id <= 3 { o.id, o.user.name }",
    ));
    for row in &probe_rows {
        let Value::Int(id) = row[0] else {
            panic!("order id not an int");
        };
        assert_eq!(row[1], scan_value_for_order(&mut engine, "o.user.name", id));
    }
}

#[test]
fn non_unique_hop_is_still_a_hard_error() {
    // A link declared on a NON-unique target key is a to-many link, so a scalar
    // path through it is a pinned kind-mismatch error -- unchanged by the
    // selectivity work, whether the outer is selective or not.
    let mut engine = Engine::new(&temp_dir("nonunique")).unwrap();
    exec(
        &mut engine,
        "type U { required id: int, required name: str }",
    );
    exec(&mut engine, "type O { required id: int, u_id: int }");
    exec(&mut engine, "link O.u -> U on u_id = id"); // U.id NOT unique -> to-many
    exec(&mut engine, r#"insert U { id := 1, name := "x" }"#);
    exec(&mut engine, "insert O { id := 1, u_id := 1 }");
    let err = engine
        .execute_powql("O as o filter o.id = 1 { o.u.name }")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("to-many") && err.contains("unique"),
        "expected a non-unique / to-many error, got: {err}"
    );
}

/// A budget far smaller than a full materialization of the target table, but
/// far larger than a handful of probed rows. The scalar-link build charges its
/// materialized rows against this budget (like a join build side): the
/// full-scan strategy charges all 200 target rows and blows it, while a per-key
/// probe charges only the referenced rows and fits.
const TIGHT_BUDGET_BYTES: usize = 4 * 1024;

const N: usize = 200;

fn big_user_fixture(name: &str, limit: usize) -> Engine {
    let mut engine = Engine::with_memory_limit(&temp_dir(name), limit).unwrap();
    exec(
        &mut engine,
        "type User { required unique id: int, required name: str }",
    );
    exec(&mut engine, "type Order { required id: int, user_id: int }");
    exec(&mut engine, "link Order.user -> User on user_id = id");
    // `N` users; `N` orders each pointing at a distinct user. A fleet-shaped
    // (all-orders) query references all `N` keys -> the stats chooser picks a
    // full scan; a single-order query references one -> it probes.
    for i in 0..N {
        exec(
            &mut engine,
            &format!(r#"insert User {{ id := {i}, name := "user_number_{i}" }}"#),
        );
    }
    for i in 0..N {
        exec(
            &mut engine,
            &format!("insert Order {{ id := {i}, user_id := {i} }}"),
        );
    }
    engine
}

#[test]
fn selective_scalar_link_does_not_full_scan_target() {
    let mut engine = big_user_fixture("budget", TIGHT_BUDGET_BYTES);
    // Same data, only outer breadth differs. The fleet-shaped query references
    // every key, so the link build full-scans the target table and its charge
    // blows the tight budget -- the pre-selectivity behavior for every outer.
    let fleet = engine.execute_powql("Order as o { o.user.name }");
    assert!(
        matches!(
            fleet,
            Err(powdb_query::result::QueryError::MemoryLimitExceeded { .. })
        ),
        "expected the fleet-shaped link build to full-scan and blow the budget, got: {fleet:?}"
    );
    // The selective scalar link succeeds under the same budget -- it can only
    // fit by probing the one referenced user, not scanning all 8000.
    let rows = rows_of(exec(
        &mut engine,
        "Order as o filter o.id = 100 { o.user.name }",
    ));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Str("user_number_100".into()));
}

#[test]
fn fleet_shaped_scalar_link_unchanged() {
    // All parents selected: correctness of the full-scan (non-probe) path is
    // unchanged. (The no-regression proof is the perf harness.)
    let mut engine = fixture("fleet");
    let rows = rows_of(exec(&mut engine, "Order as o { o.id, o.user.name }"));
    assert_eq!(rows.len(), 5);
    let name = |id: i64| {
        rows.iter()
            .find(|r| r[0] == Value::Int(id))
            .map(|r| r[1].clone())
            .unwrap()
    };
    assert_eq!(name(1), Value::Str("alice".into()));
    assert_eq!(name(3), Value::Str("bob".into()));
    assert_eq!(name(4), Value::Empty); // NULL FK
    assert_eq!(name(5), Value::Empty); // dangling FK
}
