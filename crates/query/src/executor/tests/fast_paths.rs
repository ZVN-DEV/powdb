use super::*;

// ─── Mission A fast-path tests ──────────────────────────────────────────
//
// Fixture: Mission A workload schema — the same User shape used by
// crates/compare. Deterministic generator so expected values are
// computable directly in the test without reimplementing the interpreter.

/// Build a Mission A User table with `n` rows and an index on id.
/// Row i (0-indexed, id = i):
///   id        = i
///   name      = format!("user_{i}")
///   age       = 18 + (i % 60)
///   status    = ["active","inactive","pending"][i % 3]
///   email     = format!("user_{i}@example.com")
///   created_at= 1_700_000_000 + i
pub(super) fn mission_a_engine(n: i64) -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_mission_a_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql(
            "type User { required id: int, required name: str, required age: int, \
         required status: str, required email: str, required created_at: int }",
        )
        .unwrap();
    engine
        .catalog_mut()
        .create_index_unique("User", "id", true)
        .unwrap();
    let statuses = ["active", "inactive", "pending"];
    for i in 0..n {
        let age = 18 + (i % 60);
        let status = statuses[(i as usize) % 3];
        let created_at = 1_700_000_000_i64 + i;
        let q = format!(
            r#"insert User {{ id := {i}, name := "user_{i}", age := {age}, status := "{status}", email := "user_{i}@example.com", created_at := {created_at} }}"#
        );
        engine.execute_powql(&q).unwrap();
    }
    engine
}

#[test]
fn test_fastpath_point_lookup_nonindexed() {
    // `.email = literal` has no index — must short-circuit via compiled
    // predicate on the first match.
    let mut engine = mission_a_engine(50);
    let result = engine
        .execute_powql(r#"User filter .email = "user_17@example.com""#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            // id column is position 0
            assert_eq!(rows[0][0], Value::Int(17));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_fastpath_scan_filter_project_top100() {
    // Project(Limit(Filter(SeqScan))) — stream, stop at 100.
    let mut engine = mission_a_engine(1000);
    let result = engine
        .execute_powql("User filter .age > 30 limit 100 { .id, .name }")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["id", "name"]);
            assert_eq!(rows.len(), 100);
            // All rows must have age > 30 (age = 18 + (id % 60))
            // Verify via id: 18 + (id % 60) > 30  <=>  id % 60 > 12
            for row in &rows {
                if let Value::Int(id) = row[0] {
                    assert!(18 + (id % 60) > 30, "id={id} has age={}", 18 + (id % 60));
                } else {
                    panic!("expected int id");
                }
            }
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_fastpath_scan_filter_sort_limit10_desc() {
    // Project(Limit(Sort(Filter(SeqScan)))) — bounded top-N heap desc.
    let mut engine = mission_a_engine(500);
    let result = engine
        .execute_powql("User filter .age > 20 order .created_at desc limit 10 { .id, .created_at }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 10);
            // Must be monotonically non-increasing in created_at.
            let keys: Vec<i64> = rows
                .iter()
                .map(|r| {
                    if let Value::Int(v) = r[1] {
                        v
                    } else {
                        panic!("expected int");
                    }
                })
                .collect();
            for w in keys.windows(2) {
                assert!(w[0] >= w[1], "not desc sorted: {keys:?}");
            }
            // Highest created_at is id=499 (created_at=1_700_000_499),
            // age=18+(499%60)=37 which is > 20, so id=499 must be first.
            assert_eq!(rows[0][0], Value::Int(499));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_fastpath_scan_filter_sort_limit10_asc() {
    let mut engine = mission_a_engine(500);
    let result = engine
        .execute_powql("User filter .age > 20 order .created_at limit 10 { .id, .created_at }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 10);
            let keys: Vec<i64> = rows
                .iter()
                .map(|r| {
                    if let Value::Int(v) = r[1] {
                        v
                    } else {
                        panic!("expected int");
                    }
                })
                .collect();
            for w in keys.windows(2) {
                assert!(w[0] <= w[1], "not asc sorted: {keys:?}");
            }
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_top_n_fast_path_keeps_nulls_last_in_both_directions() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "powdb_topn_nulls_last_{}_{}",
        std::process::id(),
        id
    ));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Ranked { required id: int, score: int }")
        .unwrap();
    for statement in [
        "insert Ranked { id := 1, score := null }",
        "insert Ranked { id := 2, score := 20 }",
        "insert Ranked { id := 3, score := 10 }",
        "insert Ranked { id := 4, score := null }",
    ] {
        engine.execute_powql(statement).unwrap();
    }

    for (direction, expected) in [("asc", vec![3, 2, 1, 4]), ("desc", vec![2, 3, 1, 4])] {
        let QueryResult::Rows { rows, .. } = engine
            .execute_powql(&format!(
                "Ranked order .score {direction} limit 4 {{ .id, .score }}"
            ))
            .unwrap()
        else {
            panic!("expected rows");
        };
        let ids: Vec<i64> = rows
            .iter()
            .map(|row| match row[0] {
                Value::Int(value) => value,
                ref value => panic!("expected id, got {value:?}"),
            })
            .collect();
        assert_eq!(ids, expected, "{direction} must keep nulls last");
        assert!(matches!(rows[2][1], Value::Empty));
        assert!(matches!(rows[3][1], Value::Empty));
    }
}

#[test]
fn test_fastpath_agg_sum() {
    // sum over all rows of the age column. Deterministic expected value.
    let n: i64 = 300;
    let mut engine = mission_a_engine(n);
    let result = engine.execute_powql("sum(User { .age })").unwrap();
    let expected: i64 = (0..n).map(|i| 18 + (i % 60)).sum();
    match result {
        QueryResult::Scalar(Value::Int(v)) => assert_eq!(v, expected),
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn test_fastpath_agg_sum_with_filter() {
    let n: i64 = 300;
    let mut engine = mission_a_engine(n);
    let result = engine
        .execute_powql("sum(User filter .age > 30 { .age })")
        .unwrap();
    let expected: i64 = (0..n).map(|i| 18 + (i % 60)).filter(|a| *a > 30).sum();
    match result {
        QueryResult::Scalar(Value::Int(v)) => assert_eq!(v, expected),
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn test_fastpath_agg_avg() {
    let n: i64 = 300;
    let mut engine = mission_a_engine(n);
    let result = engine.execute_powql("avg(User { .age })").unwrap();
    let total: f64 = (0..n).map(|i| (18 + (i % 60)) as f64).sum();
    let expected = total / n as f64;
    match result {
        QueryResult::Scalar(Value::Float(v)) => {
            assert!((v - expected).abs() < 1e-9, "expected {expected}, got {v}");
        }
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn test_avg_over_nulls_generic_path() {
    // Regression: the generic (non-compiled) AVG path divided the sum by the
    // total row count instead of the count of contributing (non-null) values,
    // disagreeing with the compiled fast path. AVG must ignore NULLs.
    let mut engine = test_engine(); // Alice 30, Bob 25, Charlie 35
                                    // Diana has a NULL age (age column omitted).
    engine
        .execute_powql(r#"insert User { name := "Diana", email := "diana@ex.com" }"#)
        .unwrap();
    // The `upper(.name) != "ZZZ"` predicate keeps every row but is not
    // compilable, so this avg goes through the GENERIC aggregate path
    // (agg_single_col_fast bails on the scalar-func filter).
    let result = engine
        .execute_powql(r#"avg(User filter upper(.name) != "ZZZ" { .age })"#)
        .unwrap();
    match result {
        QueryResult::Scalar(Value::Float(v)) => {
            // sum(non-null ages)=90, count(non-null)=3 → 30.0 (NOT 90/4=22.5).
            assert!((v - 30.0).abs() < 1e-9, "expected 30.0, got {v}");
        }
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn test_fastpath_agg_min_max() {
    let n: i64 = 300;
    let mut engine = mission_a_engine(n);
    // age = 18 + (i % 60), so min=18 and max=77 (18+59)
    let result_min = engine.execute_powql("min(User { .age })").unwrap();
    match result_min {
        QueryResult::Scalar(Value::Int(v)) => assert_eq!(v, 18),
        other => panic!("expected Int, got {other:?}"),
    }
    let result_max = engine.execute_powql("max(User { .age })").unwrap();
    match result_max {
        QueryResult::Scalar(Value::Int(v)) => assert_eq!(v, 77),
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn test_fastpath_multi_col_and_filter() {
    // AND of int > and string = — both must be compiled into one closure.
    let n: i64 = 300;
    let mut engine = mission_a_engine(n);
    let result = engine
        .execute_powql(r#"count(User filter .age > 30 and .status = "active")"#)
        .unwrap();
    // Expected count via the same deterministic generator.
    let statuses = ["active", "inactive", "pending"];
    let expected = (0..n)
        .filter(|i| {
            let age = 18 + (i % 60);
            let status = statuses[(*i as usize) % 3];
            age > 30 && status == "active"
        })
        .count() as i64;
    match result {
        QueryResult::Scalar(Value::Int(v)) => assert_eq!(v, expected),
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn test_fastpath_update_by_pk() {
    // Update(IndexScan) — single-row mutation via B-tree lookup.
    let mut engine = mission_a_engine(50);
    let result = engine
        .execute_powql("User filter .id = 25 update { age := 99 }")
        .unwrap();
    match result {
        QueryResult::Modified(n) => assert_eq!(n, 1),
        _ => panic!("expected Modified"),
    }
    // Verify the row has the new age.
    let lookup = engine
        .execute_powql("User filter .id = 25 { .age }")
        .unwrap();
    match lookup {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Int(99));
        }
        _ => panic!("expected rows"),
    }
    // Verify no neighbouring rows were touched.
    let neighbour = engine
        .execute_powql("User filter .id = 24 { .age }")
        .unwrap();
    if let QueryResult::Rows { rows, .. } = neighbour {
        assert_eq!(rows[0][0], Value::Int(42));
    }
}

#[test]
fn test_fastpath_update_by_filter_single_pass() {
    // Regression test for the O(N*M) bug: update by a range filter must
    // not take quadratic time. We can't directly assert timing, but we
    // can assert correctness and that the call completes for a
    // reasonably-sized table (the old path at N=2000 was ~40M row-eq
    // comparisons; the new path is O(N)).
    let n: i64 = 2000;
    let mut engine = mission_a_engine(n);
    let result = engine
        .execute_powql("User filter .age > 50 update { age := 5 }")
        .unwrap();
    let expected = (0..n).filter(|i| 18 + (i % 60) > 50).count() as u64;
    match result {
        QueryResult::Modified(nn) => assert_eq!(nn, expected),
        _ => panic!("expected Modified"),
    }
    // Every row that matched the filter now has age=5. We verify both
    // directions:
    //   (a) no rows remain with age > 50 (the filter predicate)
    //   (b) count(age = 5) equals the number of rows we updated
    // Note: the original generator never produces age=5, so count(age=5)
    // is exactly the number of updated rows.
    let check_zero = engine
        .execute_powql(r#"count(User filter .age > 50)"#)
        .unwrap();
    match check_zero {
        QueryResult::Scalar(Value::Int(v)) => assert_eq!(v, 0, "some rows still have age > 50"),
        _ => panic!("expected Int"),
    }
    let check_five = engine
        .execute_powql(r#"count(User filter .age = 5)"#)
        .unwrap();
    match check_five {
        QueryResult::Scalar(Value::Int(v)) => assert_eq!(v as u64, expected),
        _ => panic!("expected Int"),
    }
    // Total row count unchanged.
    let total = engine.execute_powql("count(User)").unwrap();
    match total {
        QueryResult::Scalar(Value::Int(v)) => assert_eq!(v, n),
        _ => panic!("expected Int"),
    }
}

#[test]
fn test_fastpath_delete_by_filter_single_pass() {
    let n: i64 = 2000;
    let mut engine = mission_a_engine(n);
    let to_delete = (0..n).filter(|i| 18 + (i % 60) > 60).count() as u64;
    let result = engine
        .execute_powql("User filter .age > 60 delete")
        .unwrap();
    match result {
        QueryResult::Modified(nn) => assert_eq!(nn, to_delete),
        _ => panic!("expected Modified"),
    }
    let count = engine.execute_powql("count(User)").unwrap();
    match count {
        QueryResult::Scalar(Value::Int(v)) => assert_eq!(v as u64, n as u64 - to_delete),
        _ => panic!("expected Int"),
    }
}

#[test]
fn test_fastpath_delete_by_pk() {
    let mut engine = mission_a_engine(30);
    let result = engine.execute_powql("User filter .id = 7 delete").unwrap();
    match result {
        QueryResult::Modified(n) => assert_eq!(n, 1),
        _ => panic!("expected Modified"),
    }
    // The deleted row must be gone.
    let lookup = engine.execute_powql("User filter .id = 7").unwrap();
    match lookup {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 0),
        _ => panic!("expected rows"),
    }
    // Neighbours still present.
    let other = engine.execute_powql("User filter .id = 8 { .id }").unwrap();
    match other {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Int(8));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_fastpath_update_by_filter_matches_generic() {
    // Cross-check: running the fast-path update and counting the
    // modified rows must agree with counting matching rows via a
    // separate query. This catches off-by-one bugs in rid collection.
    let n: i64 = 500;
    let mut engine = mission_a_engine(n);
    let count_before = engine
        .execute_powql(r#"count(User filter .status = "active")"#)
        .unwrap();
    let expected_count = match count_before {
        QueryResult::Scalar(Value::Int(v)) => v as u64,
        _ => panic!("expected Int"),
    };

    let upd = engine
        .execute_powql(r#"User filter .status = "active" update { age := 42 }"#)
        .unwrap();
    match upd {
        QueryResult::Modified(n) => assert_eq!(n, expected_count),
        _ => panic!("expected Modified"),
    }

    // All "active" rows now have age = 42.
    let count_after = engine
        .execute_powql(r#"count(User filter .age = 42)"#)
        .unwrap();
    match count_after {
        QueryResult::Scalar(Value::Int(v)) => {
            // Some non-active rows may also happen to have age = 42 from
            // the original schedule (age = 18 + (i % 60) == 42 when
            // i % 60 == 24). So we assert >= expected_count.
            assert!(v as u64 >= expected_count);
        }
        _ => panic!("expected Int"),
    }
}
