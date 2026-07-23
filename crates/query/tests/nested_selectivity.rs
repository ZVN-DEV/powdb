//! Nested-projection parent selectivity: when the parent side of a nested
//! projection is selective and the child correlation column is indexed, the
//! executor fetches children per parent key instead of hash-building the
//! whole child table. These tests lock the semantics of that path to the
//! full-scan path: results must be byte-identical either way.
//!
//! The index path fires when `distinct parent keys * avg rows per key * 4
//! <= index total entries`, so fixtures use 10 parents x 10 children per
//! parent (est 10 * 4 = 40 <= 100) with a single-parent filter.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::pj1::parse_json_text;
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_nested_sel_{name}_{}_{}",
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

fn json(text: &str) -> Value {
    Value::Json(parse_json_text(text).unwrap().into())
}

/// 10 users (ids 0..9), 100 orders (10 per user, ids interleaved round-robin
/// so heap order is not clustered by user). `indexed` controls whether the
/// correlation column Order.user_id gets a non-unique index.
fn engine_10x10(name: &str, indexed: bool) -> Engine {
    let mut engine = Engine::new(&temp_dir(name)).unwrap();
    exec(
        &mut engine,
        "type User { required id: int, required name: str }",
    );
    exec(
        &mut engine,
        "type Order { required id: int, required user_id: int, required total: float, tag: str }",
    );
    for i in 0..10 {
        exec(
            &mut engine,
            &format!("insert User {{ id := {i}, name := \"u{i}\" }}"),
        );
    }
    for i in 0..100 {
        let user = i % 10; // round-robin: consecutive rids belong to different users
        exec(
            &mut engine,
            &format!(
                "insert Order {{ id := {i}, user_id := {user}, total := {}.5, tag := \"t{}\" }}",
                i,
                i % 3
            ),
        );
    }
    if indexed {
        exec(&mut engine, "alter Order add index .user_id");
    }
    engine
}

fn rows_of(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Run the same query against an indexed and an unindexed engine and demand
/// identical results. This is the core parity contract of the selective
/// index-assisted assembly path.
fn assert_parity(name: &str, query: &str) -> Vec<Vec<Value>> {
    let mut with_index = engine_10x10(&format!("{name}_idx"), true);
    let mut without_index = engine_10x10(&format!("{name}_scan"), false);
    let indexed_rows = rows_of(exec(&mut with_index, query));
    let scan_rows = rows_of(exec(&mut without_index, query));
    assert_eq!(
        indexed_rows, scan_rows,
        "index-assisted nested assembly diverged from scan path for `{query}`"
    );
    indexed_rows
}

#[test]
fn single_parent_parity_and_content() {
    let rows = assert_parity(
        "single",
        "User as u filter u.id = 4 { u.name, orders: Order as o filter o.user_id = u.id { o.id } }",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Str("u4".into()));
    // user 4 owns ids 4, 14, ..., 94 in insertion (rid) order.
    assert_eq!(
        rows[0][1],
        json(
            r#"[{"id":4},{"id":14},{"id":24},{"id":34},{"id":44},{"id":54},{"id":64},{"id":74},{"id":84},{"id":94}]"#
        )
    );
}

#[test]
fn selective_range_of_parents_parity() {
    let rows = assert_parity(
        "range",
        "User as u filter u.id >= 2 and u.id < 4 { u.name, orders: Order as o filter o.user_id = u.id { o.id, o.total } }",
    );
    assert_eq!(rows.len(), 2);
}

#[test]
fn residual_filter_applies_on_index_path() {
    let rows = assert_parity(
        "residual",
        "User as u filter u.id = 4 { u.name, orders: Order as o filter o.user_id = u.id and o.total > 50.0 { o.id } }",
    );
    // Only ids 54..94 have total > 50.
    assert_eq!(
        rows[0][1],
        json(r#"[{"id":54},{"id":64},{"id":74},{"id":84},{"id":94}]"#)
    );
}

#[test]
fn residual_filtering_everything_yields_empty_array() {
    let rows = assert_parity(
        "residual_empty",
        "User as u filter u.id = 4 { u.name, orders: Order as o filter o.user_id = u.id and o.total > 1000.0 { o.id } }",
    );
    assert_eq!(rows[0][1], json("[]"));
}

#[test]
fn order_limit_offset_apply_on_index_path() {
    let rows = assert_parity(
        "order_limit",
        "User as u filter u.id = 4 { u.name, orders: Order as o filter o.user_id = u.id order o.id desc limit 3 offset 1 { o.id } }",
    );
    assert_eq!(rows[0][1], json(r#"[{"id":84},{"id":74},{"id":64}]"#));
}

#[test]
fn unordered_arrays_keep_scan_order_on_index_path() {
    // Without an order clause the array order is child scan order (rid
    // order). The index path must reproduce it exactly, including with a
    // limit applied.
    let rows = assert_parity(
        "scan_order",
        "User as u filter u.id = 7 { orders: Order as o filter o.user_id = u.id limit 4 { o.id } }",
    );
    assert_eq!(
        rows[0][0],
        json(r#"[{"id":7},{"id":17},{"id":27},{"id":37}]"#)
    );
}

#[test]
fn parent_with_no_children_gets_empty_array() {
    let query =
        "User as u filter u.id = 4 { u.name, orders: Order as o filter o.user_id = u.id { o.id } }";
    let mut engine = engine_10x10("orphan", true);
    exec(&mut engine, "Order filter .user_id = 4 delete");
    let rows = rows_of(exec(&mut engine, query));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], json("[]"));
}

#[test]
fn null_parent_key_matches_nothing_on_index_path() {
    // A parent whose correlation value is NULL must get [] (NULL never
    // correlates), and the index path must not probe the btree with Empty.
    let mut engine = Engine::new(&temp_dir("null_parent")).unwrap();
    exec(&mut engine, "type P { required id: int, ref: int }");
    exec(
        &mut engine,
        "type C { required id: int, required p_ref: int }",
    );
    exec(&mut engine, "insert P { id := 1, ref := 5 }");
    exec(&mut engine, "insert P { id := 2 }"); // ref is NULL
    for i in 0..20 {
        exec(
            &mut engine,
            &format!("insert C {{ id := {i}, p_ref := {} }}", i % 4 + 4),
        );
    }
    exec(&mut engine, "alter C add index .p_ref");
    let rows = rows_of(exec(
        &mut engine,
        "P as p filter p.id >= 1 and p.id <= 2 { p.id, kids: C as c filter c.p_ref = p.ref { c.id } }",
    ));
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0][1],
        json(r#"[{"id":1},{"id":5},{"id":9},{"id":13},{"id":17}]"#)
    );
    assert_eq!(rows[1][1], json("[]"));
}

#[test]
fn cross_type_parent_key_matches_nothing() {
    // Value hash equality is strictly typed: Float(4.0) never equals Int(4)
    // in the scan path's hash build. The btree's Ord IS cross-type numeric,
    // so a naive index probe would wrongly match. Lock the strict semantics.
    let mut engine = Engine::new(&temp_dir("cross_type")).unwrap();
    exec(&mut engine, "type P { required id: int, ref: float }");
    exec(
        &mut engine,
        "type C { required id: int, required p_ref: int }",
    );
    exec(&mut engine, "insert P { id := 1, ref := 4.0 }");
    for i in 0..40 {
        exec(
            &mut engine,
            &format!("insert C {{ id := {i}, p_ref := {} }}", i % 8),
        );
    }
    exec(&mut engine, "alter C add index .p_ref");
    // Scan-path oracle first (no index variant).
    let mut oracle = Engine::new(&temp_dir("cross_type_oracle")).unwrap();
    exec(&mut oracle, "type P { required id: int, ref: float }");
    exec(
        &mut oracle,
        "type C { required id: int, required p_ref: int }",
    );
    exec(&mut oracle, "insert P { id := 1, ref := 4.0 }");
    for i in 0..40 {
        exec(
            &mut oracle,
            &format!("insert C {{ id := {i}, p_ref := {} }}", i % 8),
        );
    }
    let q = "P as p { p.id, kids: C as c filter c.p_ref = p.ref { c.id } }";
    let idx_rows = rows_of(exec(&mut engine, q));
    let scan_rows = rows_of(exec(&mut oracle, q));
    assert_eq!(idx_rows, scan_rows);
    assert_eq!(idx_rows[0][1], json("[]"));
}

#[test]
fn two_level_nesting_selective_parent_parity() {
    let build = |name: &str, indexed: bool| -> Engine {
        let mut e = Engine::new(&temp_dir(name)).unwrap();
        exec(&mut e, "type A { required id: int }");
        exec(&mut e, "type B { required id: int, required a_id: int }");
        exec(&mut e, "type C { required id: int, required b_id: int }");
        for i in 0..8 {
            exec(&mut e, &format!("insert A {{ id := {i} }}"));
        }
        for i in 0..64 {
            exec(
                &mut e,
                &format!("insert B {{ id := {i}, a_id := {} }}", i % 8),
            );
        }
        for i in 0..256 {
            exec(
                &mut e,
                &format!("insert C {{ id := {i}, b_id := {} }}", i % 64),
            );
        }
        if indexed {
            exec(&mut e, "alter B add index .a_id");
            exec(&mut e, "alter C add index .b_id");
        }
        e
    };
    let q = "A as a filter a.id = 3 { a.id, bs: B as b filter b.a_id = a.id { b.id, cs: C as c filter c.b_id = b.id { c.id } } }";
    let mut with_index = build("two_level_idx", true);
    let mut without_index = build("two_level_scan", false);
    let idx_rows = rows_of(exec(&mut with_index, q));
    let scan_rows = rows_of(exec(&mut without_index, q));
    assert_eq!(idx_rows, scan_rows);
    assert_eq!(idx_rows.len(), 1);
    // a=3 owns b in {3,11,19,...,59}; each b owns 4 c rows.
    let Value::Json(_) = &idx_rows[0][1] else {
        panic!("expected nested JSON");
    };
}

#[test]
fn unique_index_on_correlation_column_works() {
    // One child per parent through a UNIQUE index on the correlation column.
    let mut engine = Engine::new(&temp_dir("unique_corr")).unwrap();
    exec(&mut engine, "type P { required id: int }");
    exec(
        &mut engine,
        "type Profile { required id: int, required p_id: int, required bio: str }",
    );
    for i in 0..30 {
        exec(&mut engine, &format!("insert P {{ id := {i} }}"));
        exec(
            &mut engine,
            &format!("insert Profile {{ id := {i}, p_id := {i}, bio := \"b{i}\" }}"),
        );
    }
    exec(&mut engine, "alter Profile add unique .p_id");
    let rows = rows_of(exec(
        &mut engine,
        "P as p filter p.id = 12 { p.id, prof: Profile as pr filter pr.p_id = p.id { pr.bio } }",
    ));
    assert_eq!(rows[0][1], json(r#"[{"bio":"b12"}]"#));
}

#[test]
fn reexecution_sees_new_children_through_plan_cache() {
    // Cached plan + index path must see freshly inserted children.
    let mut engine = engine_10x10("reexec", true);
    let q = "User as u filter u.id = 4 { orders: Order as o filter o.user_id = u.id { o.id } }";
    let first = rows_of(exec(&mut engine, q));
    assert_eq!(first.len(), 1);
    exec(
        &mut engine,
        "insert Order { id := 100, user_id := 4, total := 1.5 }",
    );
    let second = rows_of(exec(&mut engine, q));
    assert_eq!(
        second[0][0],
        json(
            r#"[{"id":4},{"id":14},{"id":24},{"id":34},{"id":44},{"id":54},{"id":64},{"id":74},{"id":84},{"id":94},{"id":100}]"#
        )
    );
}

#[test]
fn fleet_shaped_query_unchanged_with_index_present() {
    // All parents selected: the executor must keep the single-scan build
    // (correctness here; the no-regression proof is the perf harness).
    let rows = assert_parity(
        "fleet",
        "User as u { u.name, orders: Order as o filter o.user_id = u.id { o.id } }",
    );
    assert_eq!(rows.len(), 10);
    for (i, row) in rows.iter().enumerate() {
        let Value::Json(_) = &row[1] else {
            panic!("row {i} missing orders array");
        };
    }
}

#[test]
fn duplicate_parent_key_values_share_children() {
    // Two parents with the SAME correlation value: both get the same array.
    let mut engine = Engine::new(&temp_dir("dup_keys")).unwrap();
    exec(
        &mut engine,
        "type P { required id: int, required grp: int }",
    );
    exec(
        &mut engine,
        "type C { required id: int, required grp: int }",
    );
    exec(&mut engine, "insert P { id := 1, grp := 7 }");
    exec(&mut engine, "insert P { id := 2, grp := 7 }");
    for i in 0..30 {
        exec(
            &mut engine,
            &format!("insert C {{ id := {i}, grp := {} }}", i % 6 + 4),
        );
    }
    exec(&mut engine, "alter C add index .grp");
    let rows = rows_of(exec(
        &mut engine,
        "P as p { p.id, kids: C as c filter c.grp = p.grp { c.id } }",
    ));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], rows[1][1]);
    assert_eq!(
        rows[0][1],
        json(r#"[{"id":3},{"id":9},{"id":15},{"id":21},{"id":27}]"#)
    );
}

#[test]
fn str_correlation_key_parity() {
    let build = |name: &str, indexed: bool| -> Engine {
        let mut e = Engine::new(&temp_dir(name)).unwrap();
        exec(&mut e, "type P { required code: str }");
        exec(&mut e, "type C { required id: int, required code: str }");
        for i in 0..12 {
            exec(&mut e, &format!("insert P {{ code := \"k{i}\" }}"));
        }
        for i in 0..120 {
            exec(
                &mut e,
                &format!("insert C {{ id := {i}, code := \"k{}\" }}", i % 12),
            );
        }
        if indexed {
            exec(&mut e, "alter C add index .code");
        }
        e
    };
    let q =
        "P as p filter p.code = \"k5\" { p.code, kids: C as c filter c.code = p.code { c.id } }";
    let mut with_index = build("str_idx", true);
    let mut without_index = build("str_scan", false);
    assert_eq!(
        rows_of(exec(&mut with_index, q)),
        rows_of(exec(&mut without_index, q))
    );
}

#[test]
fn empty_parent_result_still_validates_child_side() {
    // A parent filter matching no rows must not swallow child-side schema
    // errors: validation cannot appear and disappear with the data.
    let mut engine = engine_10x10("empty_validate", true);
    let err = engine
        .execute_powql(
            "User as u filter u.id = 999 { u.name, orders: Order as o filter o.user_id = u.id { o.no_such_column } }",
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("no_such_column"),
        "expected child column validation error, got: {err}"
    );
    // And the valid query over the same empty parent set returns 0 rows.
    let rows = rows_of(exec(
        &mut engine,
        "User as u filter u.id = 999 { u.name, orders: Order as o filter o.user_id = u.id { o.id } }",
    ));
    assert!(rows.is_empty());
}
