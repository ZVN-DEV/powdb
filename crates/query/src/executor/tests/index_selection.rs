use super::cancellation::cancel_after_checkpoint;
use super::*;
use crate::cancel::ExecCancel;
use std::sync::Arc as CancelArc;

// ── Lane A: conjunction index selection with residual recheck ──────
//
// These unit tests inspect the output of the lowering pass directly:
// a top-level `and` filter over a bare `SeqScan` must be rewritten to drive
// the scan from an indexed conjunct, re-checking the rest as a residual
// Filter. Selection follows a zero-stats heuristic
// (unique eq > non-unique eq > range) and never depends on probing.

use crate::plan::PlanNode;

/// `Doc { id, data: json }` with a non-unique expression index on
/// `.data->score` and no index on `.id`.
fn doc_score_index_engine() -> Engine {
    let mut engine = engine_only();
    engine
        .execute_powql("type Doc { required id: int, data: json }")
        .unwrap();
    engine
        .execute_powql("alter Doc add index (.data->score)")
        .unwrap();
    engine
}

/// `Doc { id, data: json }` with no indexes at all.
fn doc_no_index_engine() -> Engine {
    let mut engine = engine_only();
    engine
        .execute_powql("type Doc { required id: int, data: json }")
        .unwrap();
    engine
}

fn engine_only() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_laneA_{}_{}", std::process::id(), id));
    Engine::new(&dir).unwrap()
}

fn lower(engine: &Engine, query: &str) -> PlanNode {
    let plan = crate::planner::plan(query).unwrap();
    super::plan_exec::LoweredPlan::of(&engine.catalog, &plan)
        .node()
        .clone()
}

#[test]
fn conjunction_lowers_to_filter_over_expr_index_scan() {
    let engine = doc_score_index_engine();
    // `.data->score` is indexed, `.id` is not: the path conjunct drives the
    // scan and `.id = 1` becomes the residual.
    let lowered = lower(&engine, "Doc filter .data->score = 20 and .id = 1");
    match &lowered {
        PlanNode::Filter { input, predicate } => {
            assert!(
                matches!(input.as_ref(), PlanNode::ExprIndexScan { .. }),
                "driving scan should be ExprIndexScan, got {input:?}"
            );
            let residual = format!("{predicate:?}");
            assert!(
                residual.contains("\"id\""),
                "residual should recheck .id: {residual}"
            );
            assert!(
                !residual.contains("data"),
                "the driving path conjunct must not remain in the residual: {residual}"
            );
        }
        other => panic!("expected Filter(ExprIndexScan), got {other:?}"),
    }
}

#[test]
fn conjunction_without_any_index_is_left_byte_identical() {
    let engine = doc_no_index_engine();
    let plan = crate::planner::plan("Doc filter .data->score = 20 and .id = 1").unwrap();
    let before = format!("{plan:?}");
    let lowered = super::plan_exec::LoweredPlan::of(&engine.catalog, &plan)
        .node()
        .clone();
    assert_eq!(
        before,
        format!("{lowered:?}"),
        "no resolvable index means the plan must be returned unchanged"
    );
}

/// `Rec { a, b, c }` where `a` has a unique index, `b` and `c` have
/// non-unique indexes.
fn rec_engine(ddl: &[&str]) -> Engine {
    let mut engine = engine_only();
    engine
        .execute_powql("type Rec { required a: int, b: int, c: int }")
        .unwrap();
    for stmt in ddl {
        engine.execute_powql(stmt).unwrap();
    }
    engine
}

fn driving_column(plan: &PlanNode) -> String {
    match plan {
        PlanNode::Filter { input, .. } => match input.as_ref() {
            PlanNode::IndexScan { column, .. } | PlanNode::RangeScan { column, .. } => {
                column.clone()
            }
            other => panic!("expected an indexed driving scan, got {other:?}"),
        },
        other => panic!("expected Filter over an index scan, got {other:?}"),
    }
}

/// Seed 12 rows: `a` unique (est 1), `b` 2 distinct over 12 (est ~6), `c` unique.
fn seed_rec(engine: &mut Engine) {
    for id in 0..12i64 {
        engine
            .execute_powql(&format!(
                "insert Rec {{ a := {id}, b := {}, c := {id} }}",
                id % 2
            ))
            .unwrap();
    }
}

#[test]
fn unique_eq_beats_non_unique_eq_beats_range() {
    // All three conjuncts resolve; unique `a` (est 1) is the most selective and
    // must win over non-unique `b` (est ~6) and the range on `c`.
    let mut unique_and_more = rec_engine(&[
        "alter Rec add unique .a",
        "alter Rec add index .b",
        "alter Rec add index .c",
    ]);
    seed_rec(&mut unique_and_more);
    let lowered = lower(&unique_and_more, "Rec filter .c > 5 and .b = 2 and .a = 1");
    assert_eq!(
        driving_column(&lowered),
        "a",
        "unique eq should drive: {lowered:?}"
    );
    assert!(
        matches!(&lowered, PlanNode::Filter { input, .. } if matches!(input.as_ref(), PlanNode::IndexScan { .. }))
    );

    // No index on `a`: non-unique eq `b` beats the range on `c`.
    let mut non_unique = rec_engine(&["alter Rec add index .b", "alter Rec add index .c"]);
    seed_rec(&mut non_unique);
    let lowered = lower(&non_unique, "Rec filter .c > 5 and .b = 2 and .a = 1");
    assert_eq!(
        driving_column(&lowered),
        "b",
        "non-unique eq should beat range: {lowered:?}"
    );

    // Only the range column is indexed: the range drives the scan.
    let mut range_only = rec_engine(&["alter Rec add index .c"]);
    seed_rec(&mut range_only);
    let lowered = lower(&range_only, "Rec filter .c > 5 and .b = 2 and .a = 1");
    assert_eq!(
        driving_column(&lowered),
        "c",
        "range should drive when it is the only index: {lowered:?}"
    );
    assert!(
        matches!(&lowered, PlanNode::Filter { input, .. } if matches!(input.as_ref(), PlanNode::RangeScan { .. }))
    );
}

/// PLAN-AUDIT 2026-07-23 (powdb-plan-quality-audit) regression, now FIXED and
/// un-ignored. The old estimator modeled a non-unique equality as the UNIFORM
/// average `total_entries / distinct_keys`, which is literal-blind. On a Zipfian
/// column where one hot value covers most rows, the average is dragged down by
/// the many rare keys, so a probe of the HOT literal was estimated far too low
/// and the chooser drove the scan from the hot conjunct instead of the genuinely
/// selective one. `eq_candidate_est` now counts the actual literal (bounded), so
/// the selective conjunct `b` (~100 rows) drives over the hot `a = 0` (~1900).
#[test]
fn skew_hot_literal_should_not_drive_conjunction_over_selective_column() {
    let mut engine = engine_only();
    engine
        .execute_powql("type T { required a: int, b: int, id: int }")
        .unwrap();
    engine.execute_powql("alter T add index .a").unwrap();
    engine.execute_powql("alter T add index .b").unwrap();
    // 1900 hot rows (a = 0), 100 rows with a unique `a`. `b` is uniform 0..=19.
    engine.execute_powql("begin").unwrap();
    for id in 0..2000i64 {
        let a = if id < 1900 { 0 } else { id - 1899 }; // 1..=100 for the tail
        let b = id % 20;
        engine
            .execute_powql(&format!("insert T {{ a := {a}, b := {b}, id := {id} }}"))
            .unwrap();
    }
    engine.execute_powql("commit").unwrap();

    let lowered = lower(&engine, "T filter .a = 0 and .b = 5");
    assert_eq!(
        driving_column(&lowered),
        "b",
        "the selective conjunct `b` (~100 rows) should drive, not the hot `a = 0` \
         (~1900 rows): {lowered:?}"
    );
}

/// `Skew { s, id }` with `s` indexed: `hot` of the `hot_rows` rows carry `s = 0`,
/// the rest carry a distinct `s`. Total rows = `hot_rows + tail`.
fn skew_engine(hot_rows: i64, tail: i64) -> Engine {
    let mut engine = engine_only();
    engine
        .execute_powql("type Skew { required s: int, id: int }")
        .unwrap();
    engine.execute_powql("alter Skew add index .s").unwrap();
    engine.execute_powql("begin").unwrap();
    for id in 0..(hot_rows + tail) {
        let s = if id < hot_rows { 0 } else { id - hot_rows + 1 };
        engine
            .execute_powql(&format!("insert Skew {{ s := {s}, id := {id} }}"))
            .unwrap();
    }
    engine.execute_powql("commit").unwrap();
    engine
}

/// A lone equality on a HOT literal (> half the table) must NOT stay a naive
/// index scan: the compiled `Filter(SeqScan)` is faster. Correctness is
/// unchanged (the compiled predicate matches the same rows).
#[test]
fn single_hot_equality_falls_back_to_seq_scan() {
    let engine = skew_engine(90, 10); // s = 0 matches 90 of 100 rows
    let lowered = lower(&engine, "Skew filter .s = 0");
    assert!(
        matches!(&lowered, PlanNode::Filter { input, .. } if matches!(input.as_ref(), PlanNode::SeqScan { .. })),
        "a hot lone equality should lower to a compiled Filter(SeqScan): {lowered:?}"
    );
}

/// A lone equality on a RARE literal (<= half the table) must STILL use the
/// index -- the guard must not over-correct selective point lookups off it.
#[test]
fn rare_lone_equality_still_uses_index() {
    let engine = skew_engine(90, 10); // s = 3 matches exactly 1 of 100 rows
    let lowered = lower(&engine, "Skew filter .s = 3");
    assert!(
        matches!(&lowered, PlanNode::IndexScan { column, .. } if column == "s"),
        "a rare lone equality must keep its index scan: {lowered:?}"
    );
}

/// A NULL-heavy column: a real, rare value keeps the index (its non-null side is
/// tiny), and `= null` is left exactly as before (the empty/missing sentinel is
/// never treated as hot).
#[test]
fn null_heavy_column_selective_value_keeps_index_and_eq_null_unchanged() {
    let mut engine = engine_only();
    engine
        .execute_powql("type N { required id: int, opt: int }")
        .unwrap();
    engine.execute_powql("alter N add index .opt").unwrap();
    engine.execute_powql("begin").unwrap();
    for id in 0..100i64 {
        if id < 90 {
            // opt omitted -> missing/null; lands in the index's empty side list.
            engine
                .execute_powql(&format!("insert N {{ id := {id} }}"))
                .unwrap();
        } else {
            // 10 distinct real values 1..=10, each matching exactly one row.
            engine
                .execute_powql(&format!("insert N {{ id := {id}, opt := {} }}", id - 89))
                .unwrap();
        }
    }
    engine.execute_powql("commit").unwrap();

    // Rare real value: 1 of 10 non-null entries -> keeps the index.
    let real = lower(&engine, "N filter .opt = 5");
    assert!(
        matches!(&real, PlanNode::IndexScan { column, .. } if column == "opt"),
        "a rare real value in a null-heavy column must keep its index: {real:?}"
    );

    // `= null` is the planner's own null path (`Filter(SeqScan)` with an
    // `IsNull` predicate), not an equality index probe. The hot guard only ever
    // rewrites a countable `Field = literal` into an `Eq`-predicated seq scan, so
    // the null path must stay exactly as the planner emitted it: an `IsNull`
    // predicate, never the guard's `Eq` rewrite.
    let null = lower(&engine, "N filter .opt = null");
    match &null {
        PlanNode::Filter { input, predicate } => {
            assert!(
                matches!(input.as_ref(), PlanNode::SeqScan { .. }),
                "`= null` keeps the planner's SeqScan: {null:?}"
            );
            assert!(
                matches!(predicate, Expr::UnaryOp(crate::ast::UnaryOp::IsNull, _)),
                "`= null` must keep the planner's IsNull predicate, not the guard's Eq: {null:?}"
            );
        }
        other => panic!("expected the planner's Filter(SeqScan) null path: {other:?}"),
    }
}

#[test]
fn same_column_between_pair_merges_and_empties_the_residual() {
    // A hand-built `Filter(SeqScan)` whose predicate is a same-column
    // BETWEEN pair: lowering merges both bounds into one RangeScan and,
    // with nothing left over, emits the bare scan (no residual Filter).
    // The planner normally folds this shape itself, so we construct it
    // directly to exercise the residual-empty branch of the lowering.
    let engine = rec_engine(&["alter Rec add index .a"]);
    let between = Expr::BinaryOp(
        Box::new(Expr::BinaryOp(
            Box::new(Expr::Field("a".into())),
            BinOp::Gte,
            Box::new(Expr::Literal(Literal::Int(1))),
        )),
        BinOp::And,
        Box::new(Expr::BinaryOp(
            Box::new(Expr::Field("a".into())),
            BinOp::Lte,
            Box::new(Expr::Literal(Literal::Int(5))),
        )),
    );
    let plan = PlanNode::Filter {
        input: Box::new(PlanNode::SeqScan {
            table: "Rec".into(),
        }),
        predicate: between,
    };
    let lowered = super::plan_exec::LoweredPlan::of(&engine.catalog, &plan)
        .node()
        .clone();
    match &lowered {
        PlanNode::RangeScan {
            column, start, end, ..
        } => {
            assert_eq!(column, "a");
            assert!(
                start.is_some() && end.is_some(),
                "both bounds must survive the merge"
            );
        }
        other => panic!("expected a bare RangeScan with an empty residual, got {other:?}"),
    }
}

#[test]
fn update_conjunction_discovery_scan_is_lowered() {
    let engine = rec_engine(&["alter Rec add index .b"]);
    let lowered = lower(&engine, "Rec filter .b = 2 and .a = 1 update { c := 9 }");
    match &lowered {
        PlanNode::Update { input, .. } => match input.as_ref() {
            PlanNode::Filter { input, predicate } => {
                assert!(
                    matches!(input.as_ref(), PlanNode::IndexScan { column, .. } if column == "b")
                );
                assert!(
                    format!("{predicate:?}").contains("\"a\""),
                    "residual should recheck .a"
                );
            }
            other => panic!("expected Filter(IndexScan) under Update, got {other:?}"),
        },
        other => panic!("expected Update, got {other:?}"),
    }
}

#[test]
fn delete_conjunction_discovery_scan_is_lowered() {
    let engine = rec_engine(&["alter Rec add index .b"]);
    let lowered = lower(&engine, "Rec filter .b = 2 and .a = 1 delete");
    match &lowered {
        PlanNode::Delete { input, .. } => match input.as_ref() {
            PlanNode::Filter { input, .. } => {
                assert!(
                    matches!(input.as_ref(), PlanNode::IndexScan { column, .. } if column == "b")
                );
            }
            other => panic!("expected Filter(IndexScan) under Delete, got {other:?}"),
        },
        other => panic!("expected Delete, got {other:?}"),
    }
}

/// C2: a conjunction update/delete whose discovery scan lowered to
/// `Filter(IndexScan)` must collect its rids straight from the index and
/// recheck the residual per rid, never falling into the O(N*M)
/// `generic_rid_match`. This is asserted deterministically via a call counter
/// rather than wall-clock timing.
#[test]
fn conjunction_mutation_does_not_route_through_generic_rid_match() {
    let mut engine = rec_engine(&["alter Rec add index .b", "alter Rec add index .c"]);
    for id in 0..16i64 {
        engine
            .execute_powql(&format!(
                "insert Rec {{ a := {id}, b := {}, c := {} }}",
                id % 2,
                id % 4
            ))
            .unwrap();
    }

    // Eq-driven conjunction update: `.b = 1` drives (indexed), `.c = 1` is the
    // residual recheck.
    super::plan_exec::reset_generic_rid_match_calls();
    engine
        .execute_powql("Rec filter .b = 1 and .c = 1 update { a := 100 }")
        .unwrap();
    assert_eq!(
        super::plan_exec::generic_rid_match_calls(),
        0,
        "index-driven conjunction update must not use the quadratic generic matcher"
    );

    // Eq-driven conjunction delete: same shape, delete side.
    super::plan_exec::reset_generic_rid_match_calls();
    engine
        .execute_powql("Rec filter .b = 0 and .c = 2 delete")
        .unwrap();
    assert_eq!(
        super::plan_exec::generic_rid_match_calls(),
        0,
        "index-driven conjunction delete must not use the quadratic generic matcher"
    );
}

fn sorted_debug(result: QueryResult) -> Vec<String> {
    match result {
        QueryResult::Rows { rows, .. } => {
            let mut out: Vec<String> = rows.iter().map(|row| format!("{row:?}")).collect();
            out.sort();
            out
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The compiled residual fast path (Filter over an index scan) must produce
/// exactly the same rows as the general SeqScan path for the same query.
#[test]
fn residual_fast_path_agrees_with_general_path() {
    let mut engine = engine_only();
    engine
        .execute_powql("type Doc { required id: int, model_id: int, data: json }")
        .unwrap();
    for id in 0..40i64 {
        let tag = if id % 2 == 0 { "x" } else { "y" };
        engine
            .execute_powql(&format!(
                r#"insert Doc {{ id := {id}, model_id := {}, data := "{{\"tag\":\"{tag}\"}}" }}"#,
                id % 3
            ))
            .unwrap();
    }
    engine
        .execute_powql("alter Doc add index (.data->tag)")
        .unwrap();

    let query = r#"Doc filter .data->tag = "x" and .model_id = 1"#;
    let raw_plan = crate::planner::plan(query).unwrap();
    // General path: execute the un-lowered Filter(SeqScan) directly. This has
    // to reach the dispatch recursion target rather than `Engine::execute_plan`,
    // which lowers what it is given: through that entry point both halves of
    // this comparison would be the lowered plan and the test would compare the
    // fast path with itself.
    let general = engine.dispatch_mut(&raw_plan).unwrap();

    // Fast path: the lowered plan must be driven by the expression index.
    let lowered = super::plan_exec::LoweredPlan::of(&engine.catalog, &raw_plan)
        .node()
        .clone();
    match &lowered {
        PlanNode::Filter { input, .. } => {
            assert!(
                matches!(input.as_ref(), PlanNode::ExprIndexScan { .. }),
                "expected an ExprIndexScan-driven fast path, got {input:?}"
            );
        }
        other => panic!("expected Filter over an index scan, got {other:?}"),
    }
    let fast = engine.execute_plan(&lowered).unwrap();

    assert_eq!(sorted_debug(general), sorted_debug(fast));
}

/// The fast-path candidate loop must remain cancellable when a driving key
/// resolves to a large rid set.
#[test]
fn conjunction_fast_path_honors_explicit_cancel() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("powdb_laneA_cancel_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine.set_wal_sync_mode(super::WalSyncMode::Off);
    engine
        .execute_powql("type Big { required id: int, required model_id: int, required v: int }")
        .unwrap();
    engine
        .execute_powql("alter Big add index .model_id")
        .unwrap();
    // 6000 rows share model_id = 1, so the driving index lookup returns 6000
    // rids and the fast-path loop crosses the 4096-tick cancellation interval.
    for i in 0..6000 {
        engine
            .execute_powql(&format!(
                "insert Big {{ id := {i}, model_id := 1, v := {i} }}"
            ))
            .unwrap();
    }

    let cancel = CancelArc::new(ExecCancel::new());
    // Entry is checkpoint 1; the index lookup adds none, so checkpoint 2 is
    // necessarily the first in-loop tick.
    let cancel_thread = cancel_after_checkpoint(CancelArc::clone(&cancel), 2);
    let result =
        engine.execute_powql_readonly_with_cancel("Big filter .model_id = 1 and .v > -1", cancel);
    cancel_thread.join().unwrap();
    assert!(
        matches!(result, Err(QueryError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    // The engine is still usable afterward.
    assert!(matches!(
        engine.execute_powql_readonly("count(Big)").unwrap(),
        QueryResult::Scalar(Value::Int(6000))
    ));
}

/// Build a quiescent (checkpointed, WAL-clean) data dir with a table, an index,
/// and rows, then return its path for a read-only reopen.
fn seed_read_only_dir(tag: &str) -> std::path::PathBuf {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_ro_{tag}_{}_{}", std::process::id(), id));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let mut engine = Engine::new(&dir).unwrap();
        engine
            .execute_powql("type User { required name: str, age: int }")
            .unwrap();
        engine.execute_powql("alter User add index .age").unwrap();
        engine
            .execute_powql(r#"insert User { name := "Ada", age := 36 }"#)
            .unwrap();
        engine
            .execute_powql(r#"insert User { name := "Bo", age := 20 }"#)
            .unwrap();
        // Clean drop checkpoints (flush + WAL truncate): quiescent directory.
    }
    dir
}

#[test]
fn read_only_engine_serves_reads() {
    let dir = seed_read_only_dir("reads");
    let engine = Engine::open_read_only(&dir).unwrap();
    assert!(engine.is_read_only());
    // count read
    assert!(matches!(
        engine.execute_powql_readonly("count(User)").unwrap(),
        QueryResult::Scalar(Value::Int(2))
    ));
    // filter read
    match engine
        .execute_powql_readonly("User filter .age > 27 { .name }")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Ada".into()));
        }
        other => panic!("expected rows, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_only_engine_explain_works() {
    let dir = seed_read_only_dir("explain");
    let mut engine = Engine::open_read_only(&dir).unwrap();
    // explain is a read; goes through execute_powql, which in read-only mode
    // routes through the read-only executor.
    match engine
        .execute_powql("explain User filter .age = 36")
        .unwrap()
    {
        QueryResult::Rows { .. } | QueryResult::Executed { .. } => {}
        other => panic!("expected an explain result, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_only_engine_rejects_every_mutation_shape() {
    let dir = seed_read_only_dir("mut");
    let mut engine = Engine::open_read_only(&dir).unwrap();
    for stmt in [
        r#"insert User { name := "X", age := 1 }"#,
        "User filter .age = 36 update { age := 99 }",
        "User filter .age = 20 delete",
        "type Other { required id: int }",
        "alter User add index .name",
        "begin",
    ] {
        let err = engine.execute_powql(stmt).unwrap_err();
        assert_eq!(
            err,
            QueryError::ReadonlyMode,
            "statement {stmt:?} must return the terminal ReadonlyMode error, got {err:?}"
        );
        // The internal sentinel must never leak to the operator.
        assert!(!err.to_string().contains("__POWDB_READONLY_NEEDS_WRITE__"));
        assert!(err.to_string().contains("readonly mode"));
    }
    // Reads still work after a rejected mutation: the engine is not wedged.
    assert!(matches!(
        engine.execute_powql("count(User)").unwrap(),
        QueryResult::Scalar(Value::Int(2))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_only_engine_sql_mutation_is_terminal() {
    let dir = seed_read_only_dir("sql");
    let mut engine = Engine::open_read_only(&dir).unwrap();
    let err = engine
        .execute_sql("insert into User (name, age) values ('Z', 5)")
        .unwrap_err();
    assert_eq!(err, QueryError::ReadonlyMode);
    // A SQL read still works.
    match engine.execute_sql("select count(*) from User").unwrap() {
        QueryResult::Scalar(Value::Int(2)) => {}
        other => panic!("expected scalar 2, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A table with a `datetime` column, plus an `int` column holding the same
/// timestamps, so every datetime assertion can be checked against the Int
/// path that is already known to be correct.
///
/// `created_at` is deliberately out of insertion order, and one row leaves it
/// null so the bitmap-skip branch is exercised.
fn datetime_fast_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_dt_fast_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Ev { required name: str, created_at: datetime, mirror: int }")
        .unwrap();
    let rows = [
        ("a", 300i64),
        ("b", 100),
        ("c", 500),
        ("d", 200),
        ("e", 400),
    ];
    for (name, ts) in rows {
        engine
            .execute_powql(&format!(
                "insert Ev {{ name := \"{name}\", created_at := {ts}, mirror := {ts} }}"
            ))
            .unwrap();
    }
    engine.execute_powql("insert Ev { name := \"z\" }").unwrap();
    engine
}

#[test]
fn datetime_filter_compiles_and_matches_the_int_path() {
    // `WHERE created_at > <micros>` is one of the two most common shapes an
    // ORM emits. The compiled leaf rejected datetime columns, so this fell to
    // the generic decode path. It must now compile AND agree with the
    // equivalent Int-column query row for row.
    let mut engine = datetime_fast_engine();
    for (op, lit) in [(">", 200), ("<", 400), (">=", 300), ("<=", 300), ("=", 500)] {
        let dt = engine
            .execute_powql(&format!("count(Ev filter .created_at {op} {lit})"))
            .unwrap();
        let int = engine
            .execute_powql(&format!("count(Ev filter .mirror {op} {lit})"))
            .unwrap();
        assert_eq!(
            format!("{dt:?}"),
            format!("{int:?}"),
            "datetime and int disagreed for `{op} {lit}`"
        );
    }
}

#[test]
fn datetime_filter_never_matches_a_null_timestamp() {
    // Row "z" has no created_at. A comparison against a missing value must
    // not match, matching filter NULL semantics and the Int leaf's null guard.
    let mut engine = datetime_fast_engine();
    let result = engine
        .execute_powql("count(Ev filter .created_at > -9999999)")
        .unwrap();
    match result {
        QueryResult::Scalar(Value::Int(n)) => {
            assert_eq!(n, 5, "the null-timestamp row must be excluded, got {n}")
        }
        other => panic!("expected scalar int, got {other:?}"),
    }
}

#[test]
fn datetime_filter_accepts_a_reversed_literal() {
    // `<literal> op .field` must compile too, with the operator flipped.
    let mut engine = datetime_fast_engine();
    let dt = engine
        .execute_powql("count(Ev filter 400 > .created_at)")
        .unwrap();
    let int = engine
        .execute_powql("count(Ev filter 400 > .mirror)")
        .unwrap();
    assert_eq!(
        format!("{dt:?}"),
        format!("{int:?}"),
        "reversed-literal datetime disagreed with int"
    );
}

#[test]
fn datetime_top_n_sort_matches_the_int_path_and_keeps_type() {
    // `ORDER BY created_at DESC LIMIT n` is the other most common ORM shape.
    // The top-N fast path gated on Int|Float, so datetime sorts fell back.
    // Ordering must match the Int mirror, and the projected value must still
    // come back as a DateTime, not silently as an Int.
    let mut engine = datetime_fast_engine();
    for dir in ["desc", "asc"] {
        let dt = engine
            .execute_powql(&format!("Ev order .created_at {dir} limit 3 {{ .name }}"))
            .unwrap();
        let int = engine
            .execute_powql(&format!("Ev order .mirror {dir} limit 3 {{ .name }}"))
            .unwrap();
        assert_eq!(
            format!("{dt:?}"),
            format!("{int:?}"),
            "datetime top-N order disagreed with int ({dir})"
        );
    }

    let typed = engine
        .execute_powql("Ev order .created_at desc limit 1 { .created_at }")
        .unwrap();
    match typed {
        QueryResult::Rows { rows, .. } => match &rows[0][0] {
            Value::DateTime(v) => assert_eq!(*v, 500, "wrong row sorted first"),
            other => panic!("top-N must preserve the DateTime type, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn datetime_filter_agrees_whether_or_not_an_index_exists() {
    // The index path already accepted an int literal as a datetime index key
    // (`lowering::coerce_column_index_key`), while the scan path compared type
    // discriminants and matched every non-null row. So the SAME predicate
    // returned different answers depending on whether an index happened to
    // exist, which is the worst shape a correctness bug can take.
    let mut engine = datetime_fast_engine();
    let queries = [
        "count(Ev filter .created_at > 200)",
        "count(Ev filter .created_at = 500)",
        "count(Ev filter .created_at <= 300)",
    ];

    let before: Vec<String> = queries
        .iter()
        .map(|q| format!("{:?}", engine.execute_powql(q).unwrap()))
        .collect();

    engine
        .execute_powql("alter Ev add index .created_at")
        .unwrap();

    for (q, unindexed) in queries.iter().zip(&before) {
        let indexed = format!("{:?}", engine.execute_powql(q).unwrap());
        assert_eq!(
            *unindexed, indexed,
            "`{q}` answered differently with and without an index"
        );
    }
}
