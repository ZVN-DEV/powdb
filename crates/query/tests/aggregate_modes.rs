use powdb_query::ast::{AggFunc, AggregateMode, Expr, GroupKey};
use powdb_query::executor::Engine;
use powdb_query::plan::{GroupAgg, PlanNode, ProjectField};
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_aggregate_modes_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn engine(name: &str) -> Engine {
    let mut engine = Engine::new(&temp_dir(name)).unwrap();
    engine
        .execute_powql("type Account { dept: str, balance: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert Account { dept := "a", balance := 7 }"#)
        .unwrap();
    engine
}

fn scalar(result: QueryResult) -> Value {
    match result {
        QueryResult::Scalar(value) => value,
        other => panic!("expected scalar, got {other:?}"),
    }
}

fn rows(result: QueryResult) -> (Vec<String>, Vec<Vec<Value>>) {
    match result {
        QueryResult::Rows { columns, rows } => (columns, rows),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn fanout_engine(name: &str) -> Engine {
    let mut engine = Engine::new(&temp_dir(name)).unwrap();
    engine
        .execute_powql("type Account { id: int, dept: str, balance: int }")
        .unwrap();
    engine
        .execute_powql("type Entry { id: int, account_id: int }")
        .unwrap();
    for query in [
        r#"insert Account { id := 1, dept := "a", balance := 10 }"#,
        r#"insert Account { id := 2, dept := "a", balance := 30 }"#,
        "insert Entry { id := 1, account_id := 1 }",
        "insert Entry { id := 2, account_id := 1 }",
        "insert Entry { id := 3, account_id := 1 }",
        "insert Entry { id := 4, account_id := 2 }",
    ] {
        engine.execute_powql(query).unwrap();
    }
    engine
}

#[test]
fn symmetric_avg_uses_source_rids_while_raw_and_sql_keep_fanout() {
    let mut engine = fanout_engine("avg_wedge");
    assert_eq!(
        scalar(
            engine
                .execute_powql(
                    "avg(Account as a join Entry as e on a.id = e.account_id { a.balance })",
                )
                .unwrap()
        ),
        Value::Float(20.0)
    );
    assert_eq!(
        scalar(
            engine
                .execute_powql(
                    "avg(raw Account as a join Entry as e on a.id = e.account_id { a.balance })",
                )
                .unwrap()
        ),
        Value::Float(15.0)
    );
    let symmetric = rows(
        engine
            .execute_powql(
                "Account as a join Entry as e on a.id = e.account_id \
                 group a.dept { value: avg(a.balance) }",
            )
            .unwrap(),
    )
    .1[0][0]
        .clone();
    let raw = rows(
        engine
            .execute_powql(
                "Account as a join Entry as e on a.id = e.account_id \
                 group a.dept { value: avg(raw a.balance) }",
            )
            .unwrap(),
    )
    .1[0][0]
        .clone();
    let sql = scalar(
        engine
            .execute_sql(
                "SELECT AVG(a.balance) FROM Account a \
                 JOIN Entry e ON a.id = e.account_id",
            )
            .unwrap(),
    );
    assert_eq!(symmetric, Value::Float(20.0));
    assert_eq!(raw, Value::Float(15.0));
    assert_eq!(sql, Value::Float(15.0));
}

fn only_projected_value(engine: &mut Engine, query: &str) -> Value {
    rows(engine.execute_powql(query).unwrap()).1[0][0].clone()
}

#[test]
fn symmetric_sum_count_and_expression_dedup_by_rid_not_value() {
    let mut engine = fanout_engine("sum_count_expression");
    let base = "Account as a join Entry as e on a.id = e.account_id and e.id > 0 group a.dept";
    assert_eq!(
        only_projected_value(&mut engine, &format!("{base} {{ v: sum(a.balance) }}")),
        Value::Int(40)
    );
    assert_eq!(
        only_projected_value(&mut engine, &format!("{base} {{ v: sum(raw a.balance) }}")),
        Value::Int(60)
    );
    assert_eq!(
        only_projected_value(&mut engine, &format!("{base} {{ v: count(a.balance) }}")),
        Value::Int(2)
    );
    assert_eq!(
        only_projected_value(&mut engine, &format!("{base} {{ v: sum(a.balance + 1) }}")),
        Value::Int(42)
    );

    engine
        .execute_powql("Account filter .id = 2 update { balance := 10 }")
        .unwrap();
    assert_eq!(
        only_projected_value(&mut engine, &format!("{base} {{ v: sum(a.balance) }}")),
        Value::Int(20),
        "distinct source RIDs with equal values must both contribute"
    );
}

#[test]
fn symmetric_provenance_survives_nested_outer_and_multi_joins() {
    let mut engine = fanout_engine("join_shapes");
    let nested = "Account as a join Entry as e on a.id + 0 = e.account_id group a.dept";
    assert_eq!(
        only_projected_value(&mut engine, &format!("{nested} {{ v: sum(a.balance) }}")),
        Value::Int(40)
    );

    engine
        .execute_powql(r#"insert Account { id := 3, dept := "a", balance := 50 }"#)
        .unwrap();
    let left = "Account as a left join Entry as e on a.id = e.account_id group a.dept";
    assert_eq!(
        only_projected_value(&mut engine, &format!("{left} {{ v: sum(a.balance) }}")),
        Value::Int(90)
    );
    engine
        .execute_powql("insert Entry { id := 99, account_id := 99 }")
        .unwrap();
    let right = "Account as a right join Entry as e on a.id = e.account_id group e.account_id";
    let (_, right_rows) = rows(
        engine
            .execute_powql(&format!("{right} {{ e.account_id, v: sum(a.balance) }}"))
            .unwrap(),
    );
    let orphan = right_rows
        .iter()
        .find(|row| row[0] == Value::Int(99))
        .expect("orphan right row");
    assert_eq!(orphan[1], Value::Int(0));

    engine
        .execute_powql("type Tag { id: int, entry_id: int }")
        .unwrap();
    for entry_id in 1..=4 {
        for copy in 0..2 {
            engine
                .execute_powql(&format!(
                    "insert Tag {{ id := {}, entry_id := {entry_id} }}",
                    entry_id * 10 + copy
                ))
                .unwrap();
        }
    }
    let multi = "Account as a join Entry as e on a.id = e.account_id \
                 join Tag as t on e.id = t.entry_id group a.dept";
    assert_eq!(
        only_projected_value(&mut engine, &format!("{multi} {{ v: sum(a.balance) }}")),
        Value::Int(40)
    );
}

#[test]
fn symmetric_nullable_hash_joins_match_raw_inner_and_outer_semantics() {
    let mut engine = Engine::new(&temp_dir("nullable_hash_join")).unwrap();
    engine
        .execute_powql("type LeftRow { id: int, join_key: int, amount: int }")
        .unwrap();
    engine
        .execute_powql("type RightRow { id: int, join_key: int }")
        .unwrap();
    for query in [
        "insert LeftRow { id := 1, amount := 20 }",
        "insert LeftRow { id := 2, join_key := 7, amount := 30 }",
        "insert RightRow { id := 10 }",
        "insert RightRow { id := 11 }",
        "insert RightRow { id := 12, join_key := 9 }",
    ] {
        engine.execute_powql(query).unwrap();
    }

    let inner = "LeftRow as l join RightRow as r on l.join_key = r.join_key group l.id";
    assert_eq!(
        only_projected_value(&mut engine, &format!("{inner} {{ v: sum(l.amount) }}")),
        Value::Int(20)
    );
    assert_eq!(
        only_projected_value(&mut engine, &format!("{inner} {{ v: sum(raw l.amount) }}")),
        Value::Int(40),
        "raw mode must retain both nullable-key matches"
    );

    let (_, left_rows) = rows(
        engine
            .execute_powql(
                "LeftRow as l left join RightRow as r on l.join_key = r.join_key \
                 group l.id { l.id, v: sum(l.amount) }",
            )
            .unwrap(),
    );
    assert_eq!(
        left_rows
            .iter()
            .find(|row| row[0] == Value::Int(1))
            .unwrap()[1],
        Value::Int(20)
    );
    assert_eq!(
        left_rows
            .iter()
            .find(|row| row[0] == Value::Int(2))
            .unwrap()[1],
        Value::Int(30)
    );

    let (_, right_rows) = rows(
        engine
            .execute_powql(
                "LeftRow as l right join RightRow as r on l.join_key = r.join_key \
                 group r.id { r.id, v: sum(l.amount) }",
            )
            .unwrap(),
    );
    for right_id in [10, 11] {
        assert_eq!(
            right_rows
                .iter()
                .find(|row| row[0] == Value::Int(right_id))
                .unwrap()[1],
            Value::Int(20)
        );
    }
    assert_eq!(
        right_rows
            .iter()
            .find(|row| row[0] == Value::Int(12))
            .unwrap()[1],
        Value::Int(0)
    );
}

#[test]
fn min_max_and_count_star_remain_fanout_invariant_or_raw() {
    let mut engine = fanout_engine("fanout_invariants");
    let base = "Account as a join Entry as e on a.id = e.account_id group a.dept";
    assert_eq!(
        only_projected_value(&mut engine, &format!("{base} {{ v: min(a.balance) }}")),
        Value::Int(10)
    );
    assert_eq!(
        only_projected_value(&mut engine, &format!("{base} {{ v: max(a.balance) }}")),
        Value::Int(30)
    );
    assert_eq!(
        only_projected_value(&mut engine, &format!("{base} {{ v: count(*) }}")),
        Value::Int(4),
        "count(*) always counts joined rows"
    );
}

#[test]
fn invalid_symmetric_sources_require_explicit_raw() {
    let mut engine = fanout_engine("invalid_sources");
    let base = "Account as a join Entry as e on a.id = e.account_id group a.dept";
    for expression in ["sum(.balance)", "sum(1)", "sum(a.balance + e.id)"] {
        let error = engine
            .execute_powql(&format!("{base} {{ v: {expression} }}"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("use sum(raw ...)"), "{error}");
    }
    assert_eq!(
        only_projected_value(
            &mut engine,
            &format!("{base} {{ v: sum(raw a.balance + e.id) }}")
        ),
        Value::Int(70)
    );
}

#[test]
fn projection_before_group_preserves_underlying_source_provenance() {
    let mut engine = fanout_engine("projection_before_group");
    let plan = PlanNode::GroupBy {
        input: Box::new(PlanNode::Project {
            input: Box::new(PlanNode::NestedLoopJoin {
                left: Box::new(PlanNode::AliasScan {
                    table: "Account".into(),
                    alias: "a".into(),
                }),
                right: Box::new(PlanNode::AliasScan {
                    table: "Entry".into(),
                    alias: "e".into(),
                }),
                on: Some(Expr::BinaryOp(
                    Box::new(Expr::QualifiedField {
                        qualifier: "a".into(),
                        field: "id".into(),
                    }),
                    powdb_query::ast::BinOp::Eq,
                    Box::new(Expr::QualifiedField {
                        qualifier: "e".into(),
                        field: "account_id".into(),
                    }),
                )),
                kind: powdb_query::ast::JoinKind::Inner,
            }),
            fields: vec![
                ProjectField {
                    alias: Some("dept".into()),
                    expr: Expr::QualifiedField {
                        qualifier: "a".into(),
                        field: "dept".into(),
                    },
                },
                ProjectField {
                    alias: Some("amount".into()),
                    expr: Expr::QualifiedField {
                        qualifier: "a".into(),
                        field: "balance".into(),
                    },
                },
            ],
        }),
        keys: vec![GroupKey {
            expr: Expr::Field("dept".into()),
            output_name: "dept".into(),
        }],
        aggregates: vec![GroupAgg {
            function: AggFunc::Sum,
            argument: Expr::Field("amount".into()),
            mode: AggregateMode::Symmetric,
            provenance_alias: Some("a".into()),
            output_name: "total".into(),
        }],
        having: None,
    };
    assert_eq!(
        rows(engine.execute_plan(&plan).unwrap()).1,
        vec![vec![Value::Str("a".into()), Value::Int(40)]]
    );
}

#[test]
fn raw_and_symmetric_cache_entries_are_warm_order_independent() {
    for (name, sql_first) in [("powql_first", false), ("sql_first", true)] {
        let mut engine = engine(name);
        let (_, misses_before, entries_before) = engine.plan_cache_stats();

        let run_sql = |engine: &mut Engine| {
            scalar(
                engine
                    .execute_sql("SELECT SUM(balance) FROM Account")
                    .unwrap(),
            )
        };
        let run_powql = |engine: &mut Engine| {
            scalar(engine.execute_powql("sum(Account { .balance })").unwrap())
        };

        let (first, second) = if sql_first {
            (run_sql(&mut engine), run_powql(&mut engine))
        } else {
            (run_powql(&mut engine), run_sql(&mut engine))
        };
        assert_eq!(first, Value::Int(7));
        assert_eq!(second, Value::Int(7));

        let (_, misses_after, entries_after) = engine.plan_cache_stats();
        assert_eq!(misses_after - misses_before, 2);
        assert_eq!(entries_after - entries_before, 2);
    }
}

#[test]
fn fanout_sql_and_symmetric_cache_entries_are_warm_order_independent() {
    for (name, sql_first) in [("fanout_powql_first", false), ("fanout_sql_first", true)] {
        let mut engine = fanout_engine(name);
        let (_, misses_before, entries_before) = engine.plan_cache_stats();
        let run_sql = |engine: &mut Engine| {
            rows(
                engine
                    .execute_sql(
                        "SELECT a.dept, AVG(a.balance) FROM Account a \
                         JOIN Entry e ON a.id = e.account_id GROUP BY a.dept",
                    )
                    .unwrap(),
            )
            .1[0][1]
                .clone()
        };
        let run_powql = |engine: &mut Engine| {
            rows(
                engine
                    .execute_powql(
                        "Account as a join Entry as e on a.id = e.account_id \
                         group a.dept { value: avg(a.balance) }",
                    )
                    .unwrap(),
            )
            .1[0][0]
                .clone()
        };
        let (first, second) = if sql_first {
            (run_sql(&mut engine), run_powql(&mut engine))
        } else {
            (run_powql(&mut engine), run_sql(&mut engine))
        };
        let expected = if sql_first {
            (Value::Float(15.0), Value::Float(20.0))
        } else {
            (Value::Float(20.0), Value::Float(15.0))
        };
        assert_eq!((first, second), expected);
        let (_, misses_after, entries_after) = engine.plan_cache_stats();
        assert_eq!(misses_after - misses_before, 2);
        assert_eq!(entries_after - entries_before, 2);
    }
}

#[test]
fn explicit_raw_powql_executes_through_the_same_surface() {
    let mut engine = engine("explicit_raw");
    assert_eq!(
        scalar(
            engine
                .execute_powql("sum(raw Account { .balance })")
                .unwrap()
        ),
        Value::Int(7)
    );
}

#[test]
fn materialized_view_refresh_preserves_explicit_raw_aggregate_mode() {
    let mut engine = fanout_engine("raw_view_refresh");
    engine
        .execute_powql(
            "materialize RawTotals as Account as a join Entry as e on a.id = e.account_id \
             group a.dept { value: avg(raw a.balance) }",
        )
        .unwrap();
    assert_eq!(
        rows(engine.execute_powql("RawTotals").unwrap()).1,
        vec![vec![Value::Float(15.0)]]
    );

    // This insert dirties the view. Its next read auto-refreshes by executing
    // the stored source text, which must retain the explicit raw modifier.
    engine
        .execute_powql("insert Entry { id := 5, account_id := 2 }")
        .unwrap();
    assert_eq!(
        rows(engine.execute_powql("RawTotals").unwrap()).1,
        vec![vec![Value::Float(18.0)]]
    );
}

#[test]
fn grouped_projection_cache_preserves_scalar_and_aggregate_literal_ordinals() {
    let mut engine = engine("group_literal_ordinals");
    let first = engine
        .execute_powql("Account group .dept { marker: 10, total: sum(.balance + 1) }")
        .unwrap();
    let second = engine
        .execute_powql("Account group .dept { marker: 20, total: sum(.balance + 2) }")
        .unwrap();
    assert_eq!(
        rows(first),
        (
            vec!["marker".into(), "total".into()],
            vec![vec![Value::Int(10), Value::Int(8)]],
        )
    );
    assert_eq!(
        rows(second),
        (
            vec!["marker".into(), "total".into()],
            vec![vec![Value::Int(20), Value::Int(9)]],
        )
    );
    assert!(engine.plan_cache_stats().0 >= 1, "second query hit cache");
}

#[test]
fn grouped_having_after_projection_replans_instead_of_rebinding_literals() {
    let mut engine = engine("having_after_projection");
    let (hits_before, misses_before, entries_before) = engine.plan_cache_stats();
    let first = engine
        .execute_powql(
            "Account group .dept { marker: 10, total: sum(.balance + 1) } having total > 5",
        )
        .unwrap();
    let second = engine
        .execute_powql(
            "Account group .dept { marker: 20, total: sum(.balance + 2) } having total > 6",
        )
        .unwrap();
    assert_eq!(rows(first).1, vec![vec![Value::Int(10), Value::Int(8)]],);
    assert_eq!(rows(second).1, vec![vec![Value::Int(20), Value::Int(9)]],);
    let (hits_after, misses_after, entries_after) = engine.plan_cache_stats();
    assert_eq!(hits_after - hits_before, 0);
    assert_eq!(misses_after - misses_before, 2);
    assert_eq!(entries_after, entries_before);
}

#[test]
fn grouped_having_before_projection_replans_instead_of_rebinding_literals() {
    let mut engine = engine("having_before_projection");
    let (hits_before, misses_before, entries_before) = engine.plan_cache_stats();
    let first = engine
        .execute_powql(
            "Account group .dept having sum(.balance + 1) > 5 { marker: 10, total: sum(.balance + 2) }",
        )
        .unwrap();
    let second = engine
        .execute_powql(
            "Account group .dept having sum(.balance + 3) > 6 { marker: 20, total: sum(.balance + 4) }",
        )
        .unwrap();
    assert_eq!(rows(first).1, vec![vec![Value::Int(10), Value::Int(9)]],);
    assert_eq!(rows(second).1, vec![vec![Value::Int(20), Value::Int(11)]],);
    let (hits_after, misses_after, entries_after) = engine.plan_cache_stats();
    assert_eq!(hits_after - hits_before, 0);
    assert_eq!(misses_after - misses_before, 2);
    assert_eq!(entries_after, entries_before);
}

#[test]
fn multiple_expression_group_keys_have_distinct_output_identities() {
    let mut engine = Engine::new(&temp_dir("expression_keys")).unwrap();
    engine
        .execute_powql("type Pair { a: int, b: int }")
        .unwrap();
    for query in [
        "insert Pair { a := 1, b := 1 }",
        "insert Pair { a := 1, b := 2 }",
    ] {
        engine.execute_powql(query).unwrap();
    }
    let QueryResult::Rows { columns, rows } = engine
        .execute_powql("Pair group .a + 1, .b + 1 { x: .a + 1, y: .b + 1, n: count(*) }")
        .unwrap()
    else {
        panic!("expected rows");
    };
    assert_eq!(columns, vec!["x", "y", "n"]);
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(2), Value::Int(2), Value::Int(1)],
            vec![Value::Int(2), Value::Int(3), Value::Int(1)],
        ]
    );
    let QueryResult::Rows { rows, .. } = engine
        .execute_powql("Pair group .a + 10, .b + 20 { x: .a + 10, y: .b + 20, n: count(*) }")
        .unwrap()
    else {
        panic!("expected rows");
    };
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(11), Value::Int(21), Value::Int(1)],
            vec![Value::Int(11), Value::Int(22), Value::Int(1)],
        ]
    );
}
