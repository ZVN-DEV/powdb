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
fn nested_projection_residual_condition_filters_children() {
    let mut engine = engine_with_users_and_orders("residual");
    let QueryResult::Rows { rows, .. } = exec(
        &mut engine,
        "User as u { u.name, orders: Order as o filter o.user_id = u.id and o.total > 10.0 { o.total } }",
    ) else {
        panic!("expected rows");
    };
    assert_eq!(rows.len(), 3);
    // alice keeps only the 20.25 order; bob's single 5.5 order is filtered
    // out, so bob becomes childless-after-residual and still gets [].
    assert_eq!(rows[0][1], json(r#"[{"total":20.25}]"#));
    assert_eq!(rows[1][1], json("[]"));
    assert_eq!(rows[2][1], json("[]"));
}

#[test]
fn nested_projection_correlation_predicate_position_is_free() {
    let mut engine = engine_with_users_and_orders("residual_pos");
    // The correlation predicate may sit anywhere in the AND chain.
    let QueryResult::Rows { rows, .. } = exec(
        &mut engine,
        "User as u { u.name, orders: Order as o filter o.total > 10.0 and o.user_id = u.id { o.total } }",
    ) else {
        panic!("expected rows");
    };
    assert_eq!(rows[0][1], json(r#"[{"total":20.25}]"#));
    assert_eq!(rows[1][1], json("[]"));
}

#[test]
fn nested_projection_multiple_residual_conditions() {
    let mut engine = engine_with_users_and_orders("residual_multi");
    let QueryResult::Rows { rows, .. } = exec(
        &mut engine,
        "User as u { u.name, orders: Order as o filter o.user_id = u.id and o.total > 1.0 and o.product_id = 101 { o.total } }",
    ) else {
        panic!("expected rows");
    };
    assert_eq!(rows[0][1], json(r#"[{"total":9.5}]"#));
    assert_eq!(rows[1][1], json("[]"));
    assert_eq!(rows[2][1], json("[]"));
}

#[test]
fn nested_projection_outer_alias_in_residual_is_rejected() {
    let mut engine = engine_with_users_and_orders("residual_outer");
    let err = engine
        .execute_powql(
            "User as u { u.name, orders: Order as o filter o.user_id = u.id and u.id > 0 { o.total } }",
        )
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("outer alias") && msg.contains('u'),
        "expected an outer-alias rejection naming the alias, got: {msg}"
    );
}

#[test]
fn nested_projection_missing_correlation_is_rejected() {
    let mut engine = engine_with_users_and_orders("residual_nocorr");
    let err = engine
        .execute_powql("User as u { u.name, orders: Order as o filter o.total > 1.0 { o.total } }")
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("correlation") && msg.contains("o.<col> = u.<col>"),
        "expected a correlation-predicate error showing the required shape, got: {msg}"
    );
}

#[test]
fn nested_projection_duplicate_correlation_is_rejected() {
    let mut engine = engine_with_users_and_orders("residual_dupcorr");
    let err = engine
        .execute_powql(
            "User as u { u.name, orders: Order as o filter o.user_id = u.id and o.id = u.id { o.total } }",
        )
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("exactly one"),
        "expected an ambiguous-correlation error, got: {msg}"
    );
}

#[test]
fn nested_projection_reexecution_sees_new_children() {
    let mut engine = engine_with_users_and_orders("reexec");
    let q = "User as u { u.name, orders: Order as o filter o.user_id = u.id { o.total, o.product_id } }";
    assert_nested_shape(exec(&mut engine, q));
    // The second run may be served from the plan cache; plans carry no
    // data, so it must still see the new child row.
    exec(
        &mut engine,
        "insert Order { id := 4, user_id := 3, total := 1.5, product_id := 103 }",
    );
    let QueryResult::Rows { rows, .. } = exec(&mut engine, q) else {
        panic!("expected rows");
    };
    assert_eq!(rows[2][1], json(r#"[{"total":1.5,"product_id":103}]"#));
}

#[test]
fn nested_projection_order_sorts_within_each_array() {
    let mut engine = engine_with_users_and_orders("order");
    let QueryResult::Rows { rows, .. } = exec(
        &mut engine,
        "User as u { u.name, orders: Order as o filter o.user_id = u.id order o.total desc { o.total } }",
    ) else {
        panic!("expected rows");
    };
    assert_eq!(rows[0][1], json(r#"[{"total":20.25},{"total":9.5}]"#));
    assert_eq!(rows[1][1], json(r#"[{"total":5.5}]"#));
    assert_eq!(rows[2][1], json("[]"));
}

#[test]
fn nested_projection_limit_is_per_parent() {
    let mut engine = engine_with_users_and_orders("limit");
    // A global limit 1 would leave bob childless; per-parent top-N must not.
    let QueryResult::Rows { rows, .. } = exec(
        &mut engine,
        "User as u { u.name, orders: Order as o filter o.user_id = u.id order o.total desc limit 1 { o.total } }",
    ) else {
        panic!("expected rows");
    };
    assert_eq!(rows[0][1], json(r#"[{"total":20.25}]"#));
    assert_eq!(rows[1][1], json(r#"[{"total":5.5}]"#));
    assert_eq!(rows[2][1], json("[]"));
}

#[test]
fn nested_projection_offset_and_limit_apply_after_per_parent_sort() {
    let mut engine = engine_with_users_and_orders("offset");
    let q = "User as u { u.name, orders: Order as o filter o.user_id = u.id order o.total desc offset 1 limit 5 { o.total } }";
    let QueryResult::Rows { rows, .. } = exec(&mut engine, q) else {
        panic!("expected rows");
    };
    assert_eq!(rows[0][1], json(r#"[{"total":9.5}]"#));
    assert_eq!(rows[1][1], json("[]"));
    assert_eq!(rows[2][1], json("[]"));
}

#[test]
fn nested_projection_order_ties_keep_scan_order() {
    let mut engine = engine_with_users_and_orders("stable");
    exec(
        &mut engine,
        "insert Order { id := 10, user_id := 1, total := 9.5, product_id := 110 }",
    );
    exec(
        &mut engine,
        "insert Order { id := 11, user_id := 1, total := 9.5, product_id := 111 }",
    );
    let q = "User as u { u.name, orders: Order as o filter o.user_id = u.id order o.total asc { o.product_id } }";
    let expected = json(
        r#"[{"product_id":101},{"product_id":110},{"product_id":111},{"product_id":102}]"#,
    );
    for _ in 0..3 {
        let QueryResult::Rows { rows, .. } = exec(&mut engine, q) else {
            panic!("expected rows");
        };
        assert_eq!(rows[0][1], expected, "tie order must be stable");
    }
}

#[test]
fn nested_projection_limit_without_order_truncates_scan_order() {
    let mut engine = engine_with_users_and_orders("limit_noorder");
    let QueryResult::Rows { rows, .. } = exec(
        &mut engine,
        "User as u { u.name, orders: Order as o filter o.user_id = u.id limit 1 { o.total } }",
    ) else {
        panic!("expected rows");
    };
    assert_eq!(rows[0][1], json(r#"[{"total":9.5}]"#));
    assert_eq!(rows[1][1], json(r#"[{"total":5.5}]"#));
}

#[test]
fn nested_projection_order_key_must_be_child_column() {
    let mut engine = engine_with_users_and_orders("order_badkey");
    let err = engine
        .execute_powql(
            "User as u { u.name, orders: Order as o filter o.user_id = u.id order u.id desc { o.total } }",
        )
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("order") && msg.contains('o'),
        "expected an order-key error naming the child alias, got: {msg}"
    );
}

#[test]
fn nested_projection_limit_must_be_non_negative_integer() {
    let mut engine = engine_with_users_and_orders("limit_neg");
    let err = engine
        .execute_powql(
            "User as u { u.name, orders: Order as o filter o.user_id = u.id limit -1 { o.total } }",
        )
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("limit") && msg.contains("non-negative"),
        "expected a non-negative limit error, got: {msg}"
    );
}

/// Adds an Item table under Order: order 1 has two items, order 2 has one,
/// order 3 has none.
fn add_items(engine: &mut Engine) {
    exec(
        engine,
        "type Item { required id: int, required order_id: int, required sku: str }",
    );
    exec(
        engine,
        r#"insert Item { id := 1, order_id := 1, sku := "a" }"#,
    );
    exec(
        engine,
        r#"insert Item { id := 2, order_id := 1, sku := "b" }"#,
    );
    exec(
        engine,
        r#"insert Item { id := 3, order_id := 2, sku := "c" }"#,
    );
}

#[test]
fn nested_projection_recurses_two_levels() {
    let mut engine = engine_with_users_and_orders("two_levels");
    add_items(&mut engine);
    let QueryResult::Rows { rows, .. } = exec(
        &mut engine,
        "User as u { u.name, orders: Order as o filter o.user_id = u.id { o.total, \
         items: Item as i filter i.order_id = o.id { i.sku } } }",
    ) else {
        panic!("expected rows");
    };
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0][1],
        json(
            r#"[{"total":9.5,"items":[{"sku":"a"},{"sku":"b"}]},
                {"total":20.25,"items":[{"sku":"c"}]}]"#
        )
    );
    // bob's only order has no items: empty inner array, not a missing key.
    assert_eq!(rows[1][1], json(r#"[{"total":5.5,"items":[]}]"#));
    assert_eq!(rows[2][1], json("[]"));
}

#[test]
fn nested_projection_inner_level_supports_residual_order_and_limit() {
    let mut engine = engine_with_users_and_orders("two_levels_full");
    add_items(&mut engine);
    let QueryResult::Rows { rows, .. } = exec(
        &mut engine,
        r#"User as u { u.name, orders: Order as o filter o.user_id = u.id { o.total, items: Item as i filter i.order_id = o.id and i.sku != "b" order i.sku desc limit 1 { i.sku } } }"#,
    ) else {
        panic!("expected rows");
    };
    assert_eq!(
        rows[0][1],
        json(r#"[{"total":9.5,"items":[{"sku":"a"}]},{"total":20.25,"items":[{"sku":"c"}]}]"#)
    );
}

#[test]
fn nested_projection_depth_guard_rejects_pathological_nesting() {
    let mut engine = engine_with_users_and_orders("depth");
    let mut q = String::from("User as u { u.name");
    for level in 0..80 {
        q.push_str(&format!(
            ", n{level}: Order as o{level} filter o{level}.user_id = u.id {{ o{level}.total"
        ));
    }
    q.push_str(&"}".repeat(80));
    q.push('}');
    let err = engine.execute_powql(&q).unwrap_err();
    assert!(
        err.to_string().contains("nesting depth"),
        "expected a clean nesting-depth error, got: {err}"
    );
}

#[test]
fn nested_projection_plan_cache_rebinds_literals() {
    let mut engine = engine_with_users_and_orders("cache");
    // Same shape, different residual and limit literals. A stale cached
    // binding would serve the first call's 10.0/5 to the second call.
    let q1 = "User as u { u.name, orders: Order as o filter o.user_id = u.id and o.total > 10.0 order o.total desc limit 5 { o.total } }";
    let q2 = "User as u { u.name, orders: Order as o filter o.user_id = u.id and o.total > 1.0 order o.total desc limit 1 { o.total } }";
    let QueryResult::Rows { rows, .. } = exec(&mut engine, q1) else {
        panic!("expected rows");
    };
    assert_eq!(rows[0][1], json(r#"[{"total":20.25}]"#));
    assert_eq!(rows[1][1], json("[]"));
    let QueryResult::Rows { rows, .. } = exec(&mut engine, q2) else {
        panic!("expected rows");
    };
    assert_eq!(rows[0][1], json(r#"[{"total":20.25}]"#));
    assert_eq!(rows[1][1], json(r#"[{"total":5.5}]"#));
    // And back: the q1 shape again with its original literals.
    let QueryResult::Rows { rows, .. } = exec(&mut engine, q1) else {
        panic!("expected rows");
    };
    assert_eq!(rows[0][1], json(r#"[{"total":20.25}]"#));
}

#[test]
fn nested_projection_plans_are_cached() {
    let mut engine = engine_with_users_and_orders("cache_hit");
    let q = "User as u { u.name, orders: Order as o filter o.user_id = u.id and o.total > 10.0 limit 3 { o.total } }";
    exec(&mut engine, q);
    let (hits_before, _, _) = engine.plan_cache_stats();
    exec(&mut engine, q);
    let (hits_after, _, _) = engine.plan_cache_stats();
    assert_eq!(
        hits_after,
        hits_before + 1,
        "second execution of a nested projection must hit the plan cache"
    );
}

#[test]
fn nested_projection_offset_before_limit_still_correct() {
    let mut engine = engine_with_users_and_orders("cache_offlim");
    // `offset` written before `limit` is legal but never cached (its source
    // literal order defeats the substitution walk). Both orders must give
    // the same, correct answer on repeated runs.
    let q = "User as u { u.name, orders: Order as o filter o.user_id = u.id order o.total desc offset 1 limit 1 { o.total } }";
    for _ in 0..2 {
        let QueryResult::Rows { rows, .. } = exec(&mut engine, q) else {
            panic!("expected rows");
        };
        assert_eq!(rows[0][1], json(r#"[{"total":9.5}]"#));
        assert_eq!(rows[1][1], json("[]"));
    }
}

#[test]
fn explain_handles_nested_projection() {
    let mut engine = engine_with_users_and_orders("explain");
    let result = exec(
        &mut engine,
        "explain User as u { u.name, orders: Order as o filter o.user_id = u.id { o.total } }",
    );
    let text = format!("{result:?}");
    assert!(
        text.contains("NestedProject"),
        "explain output must name the nested plan node, got: {text}"
    );
}

#[test]
fn explain_shows_nested_structure_with_correlation_keys() {
    let mut engine = engine_with_users_and_orders("explain_full");
    add_items(&mut engine);
    let result = exec(
        &mut engine,
        "explain User as u { u.name, orders: Order as o filter o.user_id = u.id and o.total > 1.0 \
         order o.total desc limit 3 { o.total, items: Item as i filter i.order_id = o.id { i.sku } } }",
    );
    let text = format!("{result:?}");
    assert!(text.contains("NestedProject"), "missing node name: {text}");
    assert!(
        text.contains("nested orders: Order as o on o.user_id = u.id"),
        "missing labeled nested child with correlation key: {text}"
    );
    assert!(
        text.contains("order [total desc]") && text.contains("limit 3"),
        "missing per-parent order/limit: {text}"
    );
    assert!(text.contains("residual="), "missing residual: {text}");
    assert!(
        text.contains("    nested items: Item as i on i.order_id = o.id"),
        "missing indented second-level nested child: {text}"
    );
}

#[test]
fn plain_queries_are_unchanged() {
    let mut engine = engine_with_users_and_orders("smoke");
    let QueryResult::Rows { columns, rows } = exec(&mut engine, r#"User filter .id = 1 { .name }"#)
    else {
        panic!("expected rows");
    };
    assert_eq!(columns, vec!["name".to_string()]);
    assert_eq!(rows, vec![vec![Value::Str("alice".into())]]);
}
