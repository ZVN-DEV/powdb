use super::*;

// ─── Mission E1.2 join executor tests ───────────────────────────────────
//
// Fixture: two-table User + Order schema. User has 3 rows; Order has 4
// rows referencing users 1 and 2 (plus one orphan user_id 99 so we can
// probe LEFT OUTER semantics). Charlie (user 3) has no orders.

pub(super) fn join_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_join_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type User { required id: int, required name: str }")
        .unwrap();
    engine
        .execute_powql(
            "type Order { required id: int, required user_id: int, required total: int }",
        )
        .unwrap();
    engine
        .execute_powql(r#"insert User { id := 1, name := "Alice" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert User { id := 2, name := "Bob" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert User { id := 3, name := "Charlie" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Order { id := 10, user_id := 1, total := 100 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Order { id := 11, user_id := 1, total := 200 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Order { id := 12, user_id := 2, total := 50  }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Order { id := 13, user_id := 99, total := 999 }"#)
        .unwrap();
    engine
}

#[test]
fn test_inner_join_matches_rows() {
    let mut engine = join_engine();
    let result = engine
        .execute_powql("User as u join Order as o on u.id = o.user_id")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            // 3 matches: Alice has 2 orders, Bob has 1. Charlie + orphan
            // are dropped under INNER semantics.
            assert_eq!(rows.len(), 3);
            // Columns are concatenated alias.field for both sides.
            assert!(columns.contains(&"u.id".to_string()));
            assert!(columns.contains(&"u.name".to_string()));
            assert!(columns.contains(&"o.id".to_string()));
            assert!(columns.contains(&"o.user_id".to_string()));
            assert!(columns.contains(&"o.total".to_string()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_inner_join_with_qualified_projection_and_filter() {
    let mut engine = join_engine();
    let result = engine
        .execute_powql(
            "User as u join Order as o on u.id = o.user_id \
         filter o.total > 75 { u.name, o.total }",
        )
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["u.name", "o.total"]);
            // Alice/100, Alice/200 (Bob's 50 filtered out).
            assert_eq!(rows.len(), 2);
            let names: Vec<_> = rows.iter().map(|r| r[0].clone()).collect();
            assert!(names
                .iter()
                .all(|v| matches!(v, Value::Str(s) if s == "Alice")));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_join_projection_with_aliased_right_table_column() {
    // Regression: the TS client reported right-table projections being
    // silently dropped. Confirm that `{ u.name, tot: o.total }` emits
    // both columns (the right-table one under its explicit alias).
    let mut engine = join_engine();
    let result = engine
        .execute_powql("User as u join Order as o on u.id = o.user_id { u.name, tot: o.total }")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["u.name", "tot"]);
            assert_eq!(rows.len(), 3);
            // Every row must have a populated `tot` value (not Empty).
            for row in &rows {
                assert!(
                    matches!(row[1], Value::Int(_)),
                    "tot should be Int, got {:?}",
                    row[1]
                );
            }
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_match_keyword_rejected_as_invalid_join() {
    // `match` is not a join keyword in PowQL — only `join`, `inner join`,
    // `left join`, `right join`, and `cross join` are recognised. With
    // the parser's EOF check in place, writing `match` produces a clean
    // error instead of silently dropping the rest of the query.
    let mut engine = join_engine();
    let err = engine
        .execute_powql("User match Order on u.id = o.user_id { u.name }")
        .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("match")
            || err.to_string().to_lowercase().contains("trailing")
            || err.to_string().to_lowercase().contains("unexpected"),
        "expected parse error mentioning trailing/unexpected token, got: {err}"
    );
}

#[test]
fn test_left_outer_join_emits_orphan_left_rows() {
    let mut engine = join_engine();
    let result = engine
        .execute_powql("User as u left join Order as o on u.id = o.user_id")
        .unwrap();
    match result {
        QueryResult::Rows { rows, columns } => {
            // Alice(2) + Bob(1) + Charlie(padding) = 4 rows.
            assert_eq!(rows.len(), 4);
            // Find Charlie's row and verify the right-side columns are Empty.
            let u_name_idx = columns.iter().position(|c| c == "u.name").unwrap();
            let o_total_idx = columns.iter().position(|c| c == "o.total").unwrap();
            let charlie = rows
                .iter()
                .find(|r| matches!(&r[u_name_idx], Value::Str(s) if s == "Charlie"))
                .expect("Charlie row present");
            assert_eq!(charlie[o_total_idx], Value::Empty);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_right_outer_join_emits_orphan_right_rows() {
    let mut engine = join_engine();
    // The orphan order (user_id = 99) has no matching User; RIGHT OUTER
    // should still emit it with the left-side (User) columns as Empty.
    let result = engine
        .execute_powql("User as u right join Order as o on u.id = o.user_id")
        .unwrap();
    match result {
        QueryResult::Rows { rows, columns } => {
            // All 4 orders appear (3 matched + 1 orphan).
            assert_eq!(rows.len(), 4);
            let u_name_idx = columns.iter().position(|c| c == "u.name").unwrap();
            let o_total_idx = columns.iter().position(|c| c == "o.total").unwrap();
            let orphan = rows
                .iter()
                .find(|r| r[o_total_idx] == Value::Int(999))
                .expect("orphan order row present");
            assert_eq!(orphan[u_name_idx], Value::Empty);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_cross_join_emits_full_product() {
    let mut engine = join_engine();
    let result = engine
        .execute_powql("User as u cross join Order as o")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3 * 4);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_hash_join_handles_swapped_predicate_orientation() {
    // `on o.user_id = u.id` should resolve the same as `u.id = o.user_id`
    // — exercises the swapped-orientation branch in
    // `try_extract_equi_join_keys`.
    let mut engine = join_engine();
    let result = engine
        .execute_powql("User as u join Order as o on o.user_id = u.id { u.name, o.total }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, columns } => {
            assert_eq!(columns, vec!["u.name", "o.total"]);
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }
}

fn result_rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_compound_join_hash_key_is_order_and_orientation_independent() {
    let mut engine = join_engine();
    let key_first = result_rows(
        engine
            .execute_powql(
                "User as u join Order as o on u.id = o.user_id and o.total > 75 \
                 { u.name, o.total }",
            )
            .unwrap(),
    );
    let residual_first_swapped = result_rows(
        engine
            .execute_powql(
                "User as u join Order as o on o.total > 75 and o.user_id = u.id \
                 { u.name, o.total }",
            )
            .unwrap(),
    );
    assert_eq!(key_first, residual_first_swapped);
    assert_eq!(key_first.len(), 2);
}

#[test]
fn test_compound_hash_join_matches_forced_nested_loop_semantics() {
    let mut engine = join_engine();
    let hashed = result_rows(
        engine
            .execute_powql(
                "User as u join Order as o on u.id = o.user_id and o.total > 75 \
                 { u.name, o.total }",
            )
            .unwrap(),
    );
    let forced_nested = result_rows(
        engine
            .execute_powql(
                "User as u join Order as o on u.id + 0 = o.user_id and o.total > 75 \
                 { u.name, o.total }",
            )
            .unwrap(),
    );
    assert_eq!(hashed, forced_nested);
}

#[test]
fn test_nullable_duplicate_hash_keys_match_nested_semantics_for_all_outer_kinds() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("powdb_nullable_join_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type L { required id: int, k: int }")
        .unwrap();
    engine
        .execute_powql("type R { required id: int, k: int }")
        .unwrap();
    engine
        .execute_powql(
            "insert L { id := 10, k := null }, { id := 11, k := 1 }, { id := 12, k := 1 }",
        )
        .unwrap();
    engine
        .execute_powql(
            "insert R { id := 20, k := null }, { id := 21, k := 1 }, { id := 22, k := 1 }",
        )
        .unwrap();

    for kind in ["join", "left join", "right join"] {
        let mut hashed = result_rows(
            engine
                .execute_powql(&format!(
                    "L as l {kind} R as r on l.k = r.k {{ l.id, r.id }}"
                ))
                .unwrap(),
        );
        let mut nested = result_rows(
            engine
                .execute_powql(&format!(
                    "L as l {kind} R as r on l.k + 0 = r.k {{ l.id, r.id }}"
                ))
                .unwrap(),
        );
        hashed.sort();
        nested.sort();
        assert_eq!(hashed, nested, "{kind} hash/nested parity");
        assert_eq!(hashed.len(), 5, "{kind} includes NULL/NULL plus duplicates");
    }
}

#[test]
fn test_left_compound_join_pads_when_residual_rejects_hash_bucket() {
    let mut engine = join_engine();
    let result = engine
        .execute_powql(
            "User as u left join Order as o on u.id = o.user_id and o.total > 75 \
             { u.name, o.total }",
        )
        .unwrap();
    let rows = result_rows(result);
    assert_eq!(rows.len(), 4);
    let bob = rows
        .iter()
        .find(|row| row[0] == Value::Str("Bob".into()))
        .expect("Bob must be preserved by the left join");
    assert_eq!(bob[1], Value::Empty);
}

#[test]
fn test_right_compound_join_preserves_residual_rejections_after_rewrite() {
    let mut engine = join_engine();
    let result = engine
        .execute_powql(
            "User as u right join Order as o on u.id = o.user_id and o.total > 75 \
             { u.name, o.total }",
        )
        .unwrap();
    let rows = result_rows(result);
    assert_eq!(rows.len(), 4);
    for total in [50, 999] {
        let row = rows
            .iter()
            .find(|row| row[1] == Value::Int(total))
            .expect("right-side row must be preserved");
        assert_eq!(row[0], Value::Empty);
    }
}

#[test]
fn test_nested_loop_pair_limit_allows_exact_cap_and_rejects_larger_products() {
    assert_eq!(
        super::plan_exec::check_nested_loop_pair_limit(2_500, 2_560, super::MAX_NESTED_LOOP_PAIRS),
        Ok(super::MAX_NESTED_LOOP_PAIRS)
    );
    assert!(matches!(
        super::plan_exec::check_nested_loop_pair_limit(2_501, 2_560, super::MAX_NESTED_LOOP_PAIRS),
        Err(QueryError::NestedLoopPairLimitExceeded {
            left_rows: 2_501,
            right_rows: 2_560,
            limit: super::MAX_NESTED_LOOP_PAIRS,
        })
    ));
    assert!(matches!(
        super::plan_exec::check_nested_loop_pair_limit(usize::MAX, 2, super::MAX_NESTED_LOOP_PAIRS),
        Err(QueryError::NestedLoopPairLimitExceeded { .. })
    ));
}

#[test]
fn test_cross_and_non_equi_products_are_bounded_before_iteration() {
    let left = vec![vec![Value::Int(1)]; 2_501];
    let right = vec![vec![Value::Int(2)]; 2_560];
    let non_equi = Expr::BinaryOp(
        Box::new(Expr::QualifiedField {
            qualifier: "a".into(),
            field: "id".into(),
        }),
        BinOp::Lt,
        Box::new(Expr::QualifiedField {
            qualifier: "b".into(),
            field: "id".into(),
        }),
    );
    let columns_left = vec!["a.id".to_string()];
    let columns_right = vec!["b.id".to_string()];
    let non_equi_result = super::plan_exec::execute_materialized_join(
        columns_left.clone(),
        left.clone(),
        columns_right.clone(),
        right.clone(),
        Some(&non_equi),
        JoinKind::Inner,
        super::MAX_NESTED_LOOP_PAIRS,
    );
    assert!(matches!(
        non_equi_result,
        Err(QueryError::NestedLoopPairLimitExceeded { .. })
    ));

    let cross_result = super::plan_exec::execute_materialized_join(
        columns_left,
        left,
        columns_right,
        right,
        None,
        JoinKind::Cross,
        super::MAX_NESTED_LOOP_PAIRS,
    );
    assert!(matches!(
        cross_result,
        Err(QueryError::NestedLoopPairLimitExceeded { .. })
    ));
}

#[test]
fn test_non_equi_join_falls_back_to_nested_loop() {
    // `u.id < o.user_id` isn't an equi-join, so the executor must
    // drop into the nested-loop path and still return correct rows.
    let mut engine = join_engine();
    let result = engine
        .execute_powql("User as u join Order as o on u.id < o.user_id")
        .unwrap();
    match result {
        QueryResult::Rows { rows, columns } => {
            // Pairs where u.id < o.user_id:
            //   User 1 < orders 2,99 = 2 rows (o.user_id=2 twice? no, only one order for user 2)
            //   Actually: orders have user_ids [1,1,2,99].
            //   User 1 (id=1): 1<1 no, 1<1 no, 1<2 yes, 1<99 yes → 2
            //   User 2 (id=2): 2<1 no, 2<1 no, 2<2 no, 2<99 yes → 1
            //   User 3 (id=3): 3<1 no, 3<1 no, 3<2 no, 3<99 yes → 1
            // Total 4.
            assert_eq!(rows.len(), 4);
            let u_id_idx = columns.iter().position(|c| c == "u.id").unwrap();
            let o_uid_idx = columns.iter().position(|c| c == "o.user_id").unwrap();
            for row in &rows {
                match (&row[u_id_idx], &row[o_uid_idx]) {
                    (Value::Int(u), Value::Int(o)) => assert!(u < o),
                    _ => panic!("expected int columns"),
                }
            }
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_nested_loop_pair_limit_env_override_lowers_and_restores_cap() {
    // A tiny cap rejects a join whose candidate-pair count is small but above
    // the override; restoring the default cap admits the same join. This is
    // exactly how POWDB_MAX_NESTED_LOOP_PAIRS reaches the executor: the server
    // parses the env value and applies it via `set_nested_loop_pair_limit`
    // (see powdb-server `parse_nested_loop_pair_limit`), so the override is
    // tested here without racing on a process-global env var.
    let mut engine = join_engine();
    // 3 users x 4 orders = 12 candidate pairs on the non-equi nested-loop path.
    engine.set_nested_loop_pair_limit(4);
    match engine.execute_powql("User as u join Order as o on u.id < o.user_id") {
        Err(QueryError::NestedLoopPairLimitExceeded { limit, .. }) => {
            assert_eq!(limit, 4, "the executor must honor the lowered cap");
            assert!(
                QueryError::NestedLoopPairLimitExceeded {
                    left_rows: 3,
                    right_rows: 4,
                    limit: 4,
                }
                .to_string()
                .contains("POWDB_MAX_NESTED_LOOP_PAIRS"),
                "the pair-limit error must name the env-var remediation"
            );
        }
        other => panic!("expected the tiny cap to reject the join, got {other:?}"),
    }

    // Raising the cap back to the default admits the same join above the
    // previous cap.
    engine.set_nested_loop_pair_limit(super::MAX_NESTED_LOOP_PAIRS);
    match engine.execute_powql("User as u join Order as o on u.id < o.user_id") {
        Ok(QueryResult::Rows { rows, .. }) => assert_eq!(rows.len(), 4),
        other => panic!("expected the raised cap to admit the join, got {other:?}"),
    }
}

#[test]
fn test_hash_join_with_string_key() {
    // Exercise the Value::Str hash path — plus verifies Hash impl for
    // Value works end to end via FxHashMap.
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_strjoin_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type A { required code: str, required label: str }")
        .unwrap();
    engine
        .execute_powql("type B { required code: str, required score: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert A { code := "x", label := "X-label" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert A { code := "y", label := "Y-label" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert B { code := "x", score := 100 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert B { code := "y", score := 200 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert B { code := "z", score := 300 }"#)
        .unwrap();

    let result = engine
        .execute_powql("A as a join B as b on a.code = b.code { a.label, b.score }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // x→100, y→200. z has no matching A.
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_multi_join_chain() {
    // Third source — verify left-deep chains compose correctly.
    let mut engine = join_engine();
    engine
        .execute_powql("type Product { required id: int, required name: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert Product { id := 100, name := "Widget" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Product { id := 200, name := "Gadget" }"#)
        .unwrap();
    // Re-create Orders with a product_id column wouldn't work without
    // table alter; instead we pick a test that exercises the shape only.
    let result = engine
        .execute_powql(
            "User as u join Order as o on u.id = o.user_id \
         cross join Product as p",
        )
        .unwrap();
    match result {
        QueryResult::Rows { rows, columns } => {
            // 3 inner matches × 2 products = 6 rows.
            assert_eq!(rows.len(), 6);
            assert!(columns.contains(&"u.name".to_string()));
            assert!(columns.contains(&"o.total".to_string()));
            assert!(columns.contains(&"p.name".to_string()));
        }
        _ => panic!("expected rows"),
    }
}
