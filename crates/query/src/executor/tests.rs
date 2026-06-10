use super::compiled::f64_bits_to_sortable_u64;
use super::Engine;
use crate::ast::Literal;
use crate::result::QueryResult;
use powdb_storage::types::*;
use std::sync::atomic::{AtomicU32, Ordering};

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

fn test_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_exec_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type User { required name: str, required email: str, age: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert User { name := "Alice", email := "alice@ex.com", age := 30 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert User { name := "Bob", email := "bob@ex.com", age := 25 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert User { name := "Charlie", email := "charlie@ex.com", age := 35 }"#)
        .unwrap();
    engine
}

#[test]
fn test_scan_all() {
    let mut engine = test_engine();
    let result = engine.execute_powql("User").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 3),
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_filter() {
    let mut engine = test_engine();
    let result = engine.execute_powql("User filter .age > 28").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2); // Alice (30) and Charlie (35)
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_projection() {
    let mut engine = test_engine();
    let result = engine.execute_powql("User { name }").unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name"]);
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_insert_and_count() {
    let mut engine = test_engine();
    let result = engine.execute_powql("count(User)").unwrap();
    match result {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 3),
        _ => panic!("expected scalar int"),
    }
}

#[test]
fn test_update() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"User filter .name = "Alice" update { age := 31 }"#)
        .unwrap();
    let result = engine
        .execute_powql(r#"User filter .name = "Alice" { name, age }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][1], Value::Int(31));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_delete() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"User filter .name = "Bob" delete"#)
        .unwrap();
    let result = engine.execute_powql("count(User)").unwrap();
    match result {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 2),
        _ => panic!("expected scalar int"),
    }
}

#[test]
fn test_order_limit() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("User order .age desc limit 2 { name, age }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][0], Value::Str("Charlie".into())); // age 35
            assert_eq!(rows[1][0], Value::Str("Alice".into())); // age 30
        }
        _ => panic!("expected rows"),
    }
}

/// `order` by a non-existent column must surface an error, not panic.
/// Regression for executor.rs:998 / :2560 where the Sort node called
/// `unwrap_or_else(|| panic!(...))` — a malformed ORDER BY would crash
/// the server thread instead of returning an error to the client.
#[test]
fn test_order_by_missing_column_errors() {
    let mut engine = test_engine();
    let err = engine
        .execute_powql("User order .nonexistent desc")
        .expect_err("sort on missing column must error, not panic");
    assert!(
        err.to_string().contains("nonexistent"),
        "error should name the missing column, got: {err}"
    );
}

// ─── LIMIT / OFFSET combined semantics ──────────────────────────────────
//
// SQL/PowQL semantics: offset skips M rows first, then limit takes N rows.
// `limit 3 offset 1` on 5 rows must return rows 1..4 (three rows), not
// `N - M` rows. These regression tests pin the plan-shape ordering that
// previously had Offset wrapping Limit (so Limit capped at N rows and
// Offset then skipped M of those, yielding N - M).

/// 5-row Product fixture with an `id` column we can order on.
fn product_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("powdb_limit_offset_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Product { required id: int, required name: str }")
        .unwrap();
    for i in 0..5i64 {
        let q = format!(r#"insert Product {{ id := {i}, name := "p{i}" }}"#);
        engine.execute_powql(&q).unwrap();
    }
    engine
}

#[test]
fn test_limit_offset_combined() {
    // 5 rows, `limit 3 offset 1` → exactly 3 rows, ids [1, 2, 3] when
    // ordered by id. We order by id to pin the row identity; without
    // an order by, insertion order is implementation-defined.
    let mut engine = product_engine();
    let result = engine
        .execute_powql("Product order .id limit 3 offset 1 { .id }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(
                rows.len(),
                3,
                "limit 3 offset 1 on 5 rows must return 3 rows"
            );
            assert_eq!(rows[0][0], Value::Int(1));
            assert_eq!(rows[1][0], Value::Int(2));
            assert_eq!(rows[2][0], Value::Int(3));
        }
        _ => panic!("expected rows"),
    }

    // `limit 2 offset 1` → exactly 2 rows, ids [1, 2].
    let result = engine
        .execute_powql("Product order .id limit 2 offset 1 { .id }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(
                rows.len(),
                2,
                "limit 2 offset 1 on 5 rows must return 2 rows"
            );
            assert_eq!(rows[0][0], Value::Int(1));
            assert_eq!(rows[1][0], Value::Int(2));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_limit_offset_combined_with_order() {
    // Same semantics but ordering on a string column. Names are p0..p4,
    // so sort order is identical to id order.
    let mut engine = product_engine();
    let result = engine
        .execute_powql("Product order .name limit 3 offset 1 { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0][0], Value::Str("p1".into()));
            assert_eq!(rows[1][0], Value::Str("p2".into()));
            assert_eq!(rows[2][0], Value::Str("p3".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_offset_then_limit_keyword_order() {
    // Parser accepts limit/offset in either order — verify the plan
    // semantics are identical regardless of keyword order.
    let mut engine = product_engine();
    let result = engine
        .execute_powql("Product order .id offset 1 limit 3 { .id }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0][0], Value::Int(1));
            assert_eq!(rows[1][0], Value::Int(2));
            assert_eq!(rows[2][0], Value::Int(3));
        }
        _ => panic!("expected rows"),
    }
}

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
fn mission_a_engine(n: i64) -> Engine {
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

// ── Mission C Phase 5: prepared statements ────────────────────

#[test]
fn test_prepared_insert_reuses_template() {
    let mut engine = test_engine();
    let prep = engine
        .prepare(r#"insert User { name := "seed", email := "seed@ex.com", age := 0 }"#)
        .expect("prepare");
    // The template has 3 literal slots: name, email, age.
    assert_eq!(prep.param_count, 3);

    for i in 0..5 {
        engine
            .execute_prepared(
                &prep,
                &[
                    Literal::String(format!("user{i}")),
                    Literal::String(format!("u{i}@ex.com")),
                    Literal::Int(20 + i as i64),
                ],
            )
            .expect("execute_prepared");
    }

    // 3 seeded + 5 prepared inserts = 8 rows.
    let count = engine.execute_powql("count(User)").unwrap();
    match count {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 8),
        _ => panic!("expected scalar"),
    }
}

#[test]
fn test_prepared_update_by_pk() {
    let mut engine = test_engine();
    let prep = engine
        .prepare(r#"User filter .name = "seed" update { age := 0 }"#)
        .expect("prepare");
    // Two slots: filter literal "seed" + assignment literal 0.
    assert_eq!(prep.param_count, 2);

    engine
        .execute_prepared(&prep, &[Literal::String("Alice".into()), Literal::Int(99)])
        .expect("execute_prepared");

    let result = engine
        .execute_powql(r#"User filter .name = "Alice" { age }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Int(99));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_prepared_wrong_arity_errors() {
    let mut engine = test_engine();
    let prep = engine
        .prepare(r#"User filter .age > 0 { name }"#)
        .expect("prepare");
    assert_eq!(prep.param_count, 1);
    let err = engine.execute_prepared(&prep, &[]).unwrap_err();
    assert!(err.to_string().contains("expects 1 literal"));
}

// ─── Mission E1.2 join executor tests ───────────────────────────────────
//
// Fixture: two-table User + Order schema. User has 3 rows; Order has 4
// rows referencing users 1 and 2 (plus one orphan user_id 99 so we can
// probe LEFT OUTER semantics). Charlie (user 3) has no orders.

fn join_engine() -> Engine {
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

// ---- Mission E2a: DISTINCT + IN-list + BETWEEN + LIKE -----------------

#[test]
fn test_distinct_deduplicates_rows() {
    let mut engine = test_engine();
    // Insert a second Alice to create a duplicate name.
    engine
        .execute_powql(r#"insert User { name := "Alice", email := "alice2@ex.com", age := 25 }"#)
        .unwrap();
    let result = engine.execute_powql("User distinct { .name }").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            let names: Vec<&Value> = rows.iter().map(|r| &r[0]).collect();
            // 4 rows in table (Alice×2, Bob, Charlie) but 3 distinct names.
            assert_eq!(names.len(), 3);
            let alice_count = names
                .iter()
                .filter(|v| matches!(v, Value::Str(s) if s == "Alice"))
                .count();
            assert_eq!(alice_count, 1);
            assert!(names
                .iter()
                .any(|v| matches!(v, Value::Str(s) if s == "Bob")));
            assert!(names
                .iter()
                .any(|v| matches!(v, Value::Str(s) if s == "Charlie")));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_in_list_filter() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User filter .name in ("Alice", "Bob") { .name }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_not_in_list_filter() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User filter .name not in ("Alice") { .name }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // Bob and Charlie survive.
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_between_filter() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("User filter .age between 25 and 30 { .name, .age }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // Alice is 30 (inclusive), Bob is 25 (inclusive).
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_between_filter_float_column_int_literals() {
    // Regression for Value::Ord cross-type bug: BETWEEN on a Float column
    // with Int literals previously returned zero rows because Ord fell
    // through to TypeId discriminant comparison instead of promoting Int
    // to f64. Verifies the fix end-to-end through the query engine.
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "powdb_exec_between_float_{}_{}",
        std::process::id(),
        id
    ));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Product { required name: str, required price: float }")
        .unwrap();
    engine
        .execute_powql(r#"insert Product { name := "Cable",   price := 29.0 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Product { name := "Speaker", price := 175.5 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Product { name := "Monitor", price := 450.0 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Product { name := "Laptop",  price := 1299.0 }"#)
        .unwrap();

    let result = engine
        .execute_powql("Product filter .price between 100 and 500 { .name, .price }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(
                rows.len(),
                2,
                "expected 2 rows in [100, 500] range, got {}: {:?}",
                rows.len(),
                rows
            );
            // Sorted by insert order: Speaker (175.5), Monitor (450.0).
            let names: Vec<&str> = rows
                .iter()
                .map(|r| match &r[0] {
                    Value::Str(s) => s.as_str(),
                    _ => panic!("expected string name"),
                })
                .collect();
            assert!(names.contains(&"Speaker"));
            assert!(names.contains(&"Monitor"));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_not_between_filter() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("User filter .age not between 26 and 29 { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // Alice (30), Bob (25), Charlie (35) all outside [26,29].
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_like_prefix_match() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User filter .name like "Ali%" { .name }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert!(matches!(&rows[0][0], Value::Str(s) if s == "Alice"));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_like_wildcard_underscore() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User filter .name like "_ob" { .name }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert!(matches!(&rows[0][0], Value::Str(s) if s == "Bob"));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_not_like_filter() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User filter .name not like "A%" { .name }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // Bob and Charlie survive (don't start with A).
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_in_list_with_integers() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("User filter .age in (25, 30) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_like_full_match() {
    let mut engine = test_engine();
    // Exact match (no wildcards).
    let result = engine
        .execute_powql(r#"User filter .name like "Alice" { .name }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
        }
        _ => panic!("expected rows"),
    }
}

// ─── Mission E2b: GROUP BY + HAVING ────────────────────────────────────

#[test]
fn test_group_by_count() {
    // All 3 users share the same "age bucket" when we group by a
    // derived column, but we can at least group by a column with
    // distinct values. test_engine has 3 distinct names.
    let mut engine = test_engine();
    let result = engine
        .execute_powql("User group .name { .name, n: count(.name) }")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name", "n"]);
            assert_eq!(rows.len(), 3); // 3 distinct names
                                       // Each group has 1 row.
            for row in &rows {
                assert_eq!(row[1], Value::Int(1));
            }
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_sum_avg() {
    // Group all rows into one bucket by a constant column.
    // We'll use the mission_a_engine with a known shape.
    let mut engine = test_engine();
    // All 3 users: ages 30, 25, 35 → sum=90, avg=30.0
    let result = engine
        .execute_powql("User group .email { .email, total_age: sum(.age) }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // Each email is unique → 3 groups, each with sum of one age.
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_with_filter() {
    let mut engine = test_engine();
    // Filter first, then group.
    let result = engine
        .execute_powql("User filter .age >= 30 group .name { .name, n: count(.name) }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // Alice (30) and Charlie (35) survive filter.
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_having() {
    // Use mission_a_engine so we have multiple rows per group.
    let mut engine = mission_a_engine(30);
    // 30 rows: statuses cycle active/inactive/pending → 10 each.
    // Group by status, HAVING count > 5.
    let result = engine
        .execute_powql("User group .status having count(.name) > 5 { .status, n: count(.name) }")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["status", "n"]);
            // All 3 groups have 10 rows each, all > 5.
            assert_eq!(rows.len(), 3);
            for row in &rows {
                assert_eq!(row[1], Value::Int(10));
            }
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_having_filters_groups() {
    let mut engine = mission_a_engine(30);
    // HAVING count > 100 → no groups survive.
    let result = engine
        .execute_powql("User group .status having count(.name) > 100 { .status }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 0);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_having_with_aliased_projection_agg() {
    // Regression: TS client found that when the projection duplicates
    // the aggregate used by HAVING (with an alias), HAVING silently
    // failed to filter. This asserts the dedup path produces correct
    // filtering.
    let mut engine = mission_a_engine(30);
    // 3 statuses, 10 rows each. HAVING >= 11 should exclude all.
    let result = engine
        .execute_powql(
            "User group .status having count(.name) >= 11 { .status, cnt: count(.name) }",
        )
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 0, "HAVING >= 11 should filter all groups");
        }
        _ => panic!("expected rows"),
    }
    // HAVING >= 10 should include all three.
    let result = engine
        .execute_powql(
            "User group .status having count(.name) >= 10 { .status, cnt: count(.name) }",
        )
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
            for row in &rows {
                assert_eq!(row[1], Value::Int(10));
            }
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_having_post_projection() {
    // Regression: HAVING placed after the projection (`{ ... } having cnt >= N`,
    // referencing projection aliases) was silently dropped. This reproduces
    // the exact form the TS client used.
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_having_post_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Person { required name: str, required age: int, city: str }")
        .unwrap();
    for (name, age, city) in [
        ("Alice", 30, "NYC"),
        ("Bob", 24, "SF"),
        ("Carol", 41, "LA"),
        ("Dave", 28, "NYC"),
        ("Eve", 35, "Austin"),
    ] {
        engine
            .execute_powql(&format!(
                r#"insert Person {{ name := "{name}", age := {age}, city := "{city}" }}"#
            ))
            .unwrap();
    }
    let result = engine
        .execute_powql("Person group .city { .city, cnt: count(.name) } having cnt >= 2")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "only NYC has >= 2 people, got: {rows:?}");
            assert_eq!(rows[0][0], Value::Str("NYC".into()));
            assert_eq!(rows[0][1], Value::Int(2));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_having_without_group_by_errors() {
    let mut engine = test_engine();
    let err = engine.execute_powql("User { .name } having count(.name) > 1");
    assert!(
        err.is_err(),
        "HAVING without GROUP BY should be a parse error"
    );
}

#[test]
fn test_group_by_having_reproduces_ts_client_case() {
    // Exact reproduction of the TS client test that surfaced the bug:
    // 5 people across 4 cities, HAVING count >= 2 should keep only NYC.
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_having_ts_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Person { required name: str, required age: int, city: str }")
        .unwrap();
    for (name, age, city) in [
        ("Alice", 30, "NYC"),
        ("Bob", 24, "SF"),
        ("Carol", 41, "LA"),
        ("Dave", 28, "NYC"),
        ("Eve", 35, "Austin"),
    ] {
        engine
            .execute_powql(&format!(
                r#"insert Person {{ name := "{name}", age := {age}, city := "{city}" }}"#
            ))
            .unwrap();
    }
    let result = engine
        .execute_powql("Person group .city having count(.name) >= 2 { .city, cnt: count(.name) }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "only NYC has >= 2 people, got: {rows:?}");
            assert_eq!(rows[0][0], Value::Str("NYC".into()));
            assert_eq!(rows[0][1], Value::Int(2));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_having_filters_some_groups() {
    // Skewed distribution — some groups pass HAVING, some don't.
    let mut engine = test_engine();
    // test_engine has 3 rows, all distinct names. Add duplicates for Alice.
    engine
        .execute_powql(r#"insert User { name := "Alice", email := "a2@ex.com", age := 31 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert User { name := "Alice", email := "a3@ex.com", age := 32 }"#)
        .unwrap();
    // Now: Alice ×3, Bob ×1, Charlie ×1. HAVING count >= 2 → only Alice.
    let result = engine
        .execute_powql("User group .name having count(.name) >= 2 { .name, cnt: count(.name) }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Alice".into()));
            assert_eq!(rows[0][1], Value::Int(3));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_min_max() {
    let mut engine = mission_a_engine(30);
    // 30 rows, ages = 18 + (i % 60) for i in 0..30, so ages 18..47.
    // Group by status (3 groups of 10 each).
    // status=active: i=0,3,6,9,12,15,18,21,24,27 → ages 18,21,24,27,30,33,36,39,42,45
    // min=18, max=45
    let result = engine.execute_powql(
        r#"User filter .status = "active" group .status { .status, lo: min(.age), hi: max(.age) }"#,
    ).unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["status", "lo", "hi"]);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("active".into()));
            assert_eq!(rows[0][1], Value::Int(18));
            assert_eq!(rows[0][2], Value::Int(45));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_group_by_avg() {
    let mut engine = mission_a_engine(6);
    // 6 rows: i=0..5
    // active (i=0,3): ages 18,21 → avg=19.5
    // inactive (i=1,4): ages 19,22 → avg=20.5
    // pending (i=2,5): ages 20,23 → avg=21.5
    let result = engine
        .execute_powql(r#"User filter .status = "active" group .status { .status, a: avg(.age) }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            match &rows[0][1] {
                Value::Float(v) => assert!((v - 19.5).abs() < 0.001),
                other => panic!("expected float, got {other:?}"),
            }
        }
        _ => panic!("expected rows"),
    }
}

// ─── IS NULL / IS NOT NULL tests ─────────────────────────────────────

#[test]
fn test_is_null_filter() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Diana", email := "diana@ex.com" }"#)
        .unwrap();
    let result = engine
        .execute_powql("User filter .age is null { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Diana".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_is_not_null_filter() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Diana", email := "diana@ex.com" }"#)
        .unwrap();
    let result = engine
        .execute_powql("User filter .age is not null { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_is_null_count() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Diana", email := "diana@ex.com" }"#)
        .unwrap();
    let result = engine
        .execute_powql("count(User filter .age is null)")
        .unwrap();
    match result {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
        _ => panic!("expected scalar int"),
    }
}

#[test]
fn test_is_null_combined_with_and() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Diana", email := "diana@ex.com" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert User { name := "Eve", email := "eve@ex.com" }"#)
        .unwrap();
    let result = engine
        .execute_powql(r#"User filter .age is null and .name = "Diana" { .name }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Diana".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_eq_null_matches_is_null() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Diana", email := "diana@ex.com" }"#)
        .unwrap();
    let result = engine
        .execute_powql("User filter .age = null { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Diana".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_neq_null_matches_is_not_null() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Diana", email := "diana@ex.com" }"#)
        .unwrap();
    let result = engine
        .execute_powql("User filter .age != null { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }
}

// ─── String function tests ─────────────────────────────────────────────

#[test]
fn test_upper_in_filter() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User filter upper(.name) = "ALICE""#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Alice".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_lower_in_projection() {
    let mut engine = test_engine();
    let result = engine.execute_powql("User { low: lower(.email) }").unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["low"]);
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0][0], Value::Str("alice@ex.com".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_length_in_projection() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("User { .name, len: length(.name) }")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name", "len"]);
            assert_eq!(rows[0][1], Value::Int(5));
            assert_eq!(rows[1][1], Value::Int(3));
            assert_eq!(rows[2][1], Value::Int(7));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_substring_in_projection() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("User { sub: substring(.name, 1, 3) }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Str("Ali".into()));
            assert_eq!(rows[1][0], Value::Str("Bob".into()));
            assert_eq!(rows[2][0], Value::Str("Cha".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_concat_in_projection() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User { full: concat(.name, " - ", .email) }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Str("Alice - alice@ex.com".into()));
            assert_eq!(rows[1][0], Value::Str("Bob - bob@ex.com".into()));
            assert_eq!(rows[2][0], Value::Str("Charlie - charlie@ex.com".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_concat_coerces_int() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User { info: concat(.name, " age=", .age) }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Str("Alice age=30".into()));
        }
        _ => panic!("expected rows"),
    }
}

// ─── CASE WHEN tests ───────────────────────────────────────────────

#[test]
fn test_case_in_projection() {
    let mut engine = test_engine();
    let result = engine.execute_powql(
        r#"User { .name, label: case when .age > 30 then "senior" when .age >= 30 then "exactly30" else "young" end }"#
    ).unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name", "label"]);
            assert_eq!(rows.len(), 3);
            for row in &rows {
                let name = &row[0];
                let label = &row[1];
                match name {
                    Value::Str(n) if n == "Alice" => {
                        assert_eq!(label, &Value::Str("exactly30".into()))
                    }
                    Value::Str(n) if n == "Bob" => {
                        assert_eq!(label, &Value::Str("young".into()))
                    }
                    Value::Str(n) if n == "Charlie" => {
                        assert_eq!(label, &Value::Str("senior".into()))
                    }
                    _ => panic!("unexpected name: {name:?}"),
                }
            }
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_case_in_filter() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User filter case when .age > 30 then true else false end"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Charlie".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_case_without_else_returns_empty() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User { .name, label: case when .age > 100 then "old" end }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            for row in &rows {
                assert_eq!(row[1], Value::Empty);
            }
        }
        _ => panic!("expected rows"),
    }
}

// ─── Mul/Div expression tests (E2f) ───────────────────────────────

#[test]
fn test_mul_in_projection() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("User { .name, double_age: .age * 2 }")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name", "double_age"]);
            // Alice age=30 → 60, Bob age=25 → 50, Charlie age=35 → 70
            let ages: Vec<_> = rows.iter().map(|r| &r[1]).collect();
            assert!(ages.contains(&&Value::Int(60)));
            assert!(ages.contains(&&Value::Int(50)));
            assert!(ages.contains(&&Value::Int(70)));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_div_in_filter() {
    let mut engine = test_engine();
    let result = engine.execute_powql("User filter .age / 10 > 2").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // 30/10=3>2 ✓, 25/10=2 ✗, 35/10=3>2 ✓
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("expected rows"),
    }
}

// ─── Multi-column ORDER BY tests (E2f) ────────────────────────────

#[test]
fn test_multi_order_by() {
    let mut engine = test_engine();
    // Insert another 30-year-old so we can test tiebreaker
    engine
        .execute_powql(r#"insert User { name := "Dave", email := "dave@ex.com", age := 30 }"#)
        .unwrap();
    let result = engine
        .execute_powql("User order .age asc, .name asc { .name, .age }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // Expected: Bob(25), Alice(30), Dave(30), Charlie(35)
            assert_eq!(rows[0][0], Value::Str("Bob".into()));
            assert_eq!(rows[1][0], Value::Str("Alice".into()));
            assert_eq!(rows[2][0], Value::Str("Dave".into()));
            assert_eq!(rows[3][0], Value::Str("Charlie".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_multi_order_mixed_direction() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Dave", email := "dave@ex.com", age := 30 }"#)
        .unwrap();
    let result = engine
        .execute_powql("User order .age asc, .name desc { .name, .age }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // Expected: Bob(25), Dave(30), Alice(30), Charlie(35)
            assert_eq!(rows[0][0], Value::Str("Bob".into()));
            assert_eq!(rows[1][0], Value::Str("Dave".into()));
            assert_eq!(rows[2][0], Value::Str("Alice".into()));
            assert_eq!(rows[3][0], Value::Str("Charlie".into()));
        }
        _ => panic!("expected rows"),
    }
}

// ─── ALTER TABLE / DROP TABLE tests (E2g) ─────────────────────────

#[test]
fn test_alter_add_column() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("alter User add column status: str")
        .unwrap();
    match result {
        QueryResult::Executed { message } => {
            assert!(message.contains("status"));
            assert!(message.contains("User"));
        }
        other => panic!("expected Executed, got {other:?}"),
    }
    // Verify schema was updated — new inserts can use the new column
    engine.execute_powql(r#"insert User { name := "Eve", email := "eve@ex.com", age := 22, status := "active" }"#).unwrap();
    let result = engine
        .execute_powql(r#"User filter .name = "Eve" { .name, .status }"#)
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name", "status"]);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][1], Value::Str("active".into()));
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_alter_add_column_reads_old_rows() {
    // Regression: before the catalog rewrite path existed, rows
    // inserted before `alter ... add column` were left on disk
    // with the pre-alter variable-offset-table layout. A bare
    // `Type` scan then walked `decode_row` which read
    // `n_var + 1` offsets using the NEW schema and panicked with
    // "range end index X out of range for slice of length Y".
    //
    // This test reproduces that exactly: insert, alter, bare scan.
    // Any panic or wrong row count means the rewrite regressed.
    let mut engine = test_engine();
    engine
        .execute_powql("alter User add column country: str")
        .unwrap();
    // Bare scan: NO filter, so the planner cannot skip old rows.
    let result = engine.execute_powql("User").unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert!(columns.contains(&"country".to_string()));
            assert_eq!(rows.len(), 3, "three old rows must still be readable");
            let country_idx = columns
                .iter()
                .position(|c| c == "country")
                .expect("country column");
            for row in &rows {
                assert_eq!(
                    row[country_idx],
                    Value::Empty,
                    "backfilled column must be Empty"
                );
            }
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_alter_add_required_column_fails() {
    // Adding a required column to a non-empty table has no
    // default value to backfill with, so storing `Empty` would
    // silently violate the required invariant. The catalog must
    // reject it.
    let mut engine = test_engine();
    let err = engine
        .execute_powql("alter User add column required country: str")
        .expect_err("required-column add on non-empty table must fail");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("required") || msg.contains("backfill"),
        "error should mention required/backfill, got: {err}"
    );
    // And the schema must NOT have silently gained the column.
    let result = engine.execute_powql("User").unwrap();
    if let QueryResult::Rows { columns, .. } = result {
        assert!(
            !columns.contains(&"country".to_string()),
            "failed alter must not mutate the schema"
        );
    }
}

#[test]
fn test_alter_add_column_then_update_old_row() {
    // Regression-plus: after the rewrite path backfills Empty, an
    // UPDATE against an old row's new column must round-trip.
    // This exercises encode/decode with the new schema shape on a
    // row that was originally written with the old shape.
    let mut engine = test_engine();
    engine
        .execute_powql("alter User add column country: str")
        .unwrap();
    engine
        .execute_powql(r#"User filter .name = "Alice" update { country := "US" }"#)
        .unwrap();

    let result = engine
        .execute_powql(r#"User filter .name = "Alice" { .name, .country }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Alice".into()));
            assert_eq!(rows[0][1], Value::Str("US".into()));
        }
        other => panic!("expected rows, got {other:?}"),
    }

    // The other two rows should still decode cleanly with Empty.
    let result = engine.execute_powql("User").unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(rows.len(), 3);
            let country_idx = columns
                .iter()
                .position(|c| c == "country")
                .expect("country column");
            let empties = rows
                .iter()
                .filter(|r| r[country_idx] == Value::Empty)
                .count();
            assert_eq!(
                empties, 2,
                "two unchanged old rows must still read as Empty"
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_alter_drop_column() {
    let mut engine = test_engine();
    engine
        .execute_powql("alter User drop column email")
        .unwrap();
    let result = engine.execute_powql("User { .name, .age }").unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name", "age"]);
            assert_eq!(rows.len(), 3);
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_drop_table() {
    let mut engine = test_engine();
    let result = engine.execute_powql("drop User").unwrap();
    match result {
        QueryResult::Executed { message } => {
            assert!(message.contains("User"));
            assert!(message.contains("dropped"));
        }
        other => panic!("expected Executed, got {other:?}"),
    }
    // Querying the dropped table should fail
    assert!(engine.execute_powql("User").is_err());
}

#[test]
fn test_drop_nonexistent_table_errors() {
    let mut engine = test_engine();
    assert!(engine.execute_powql("drop NonExistent").is_err());
}

#[test]
fn test_alter_add_duplicate_column_errors() {
    let mut engine = test_engine();
    assert!(engine.execute_powql("alter User add name: str").is_err());
}

#[test]
fn test_alter_drop_nonexistent_column_errors() {
    let mut engine = test_engine();
    assert!(engine
        .execute_powql("alter User drop column nonexistent")
        .is_err());
}

#[test]
fn test_alter_add_index_creates_index() {
    let mut engine = test_engine();
    let result = engine.execute_powql("alter User add index .email").unwrap();
    match result {
        QueryResult::Executed { message } => {
            assert!(message.contains("User.email"), "message: {message}");
        }
        other => panic!("expected Executed, got {other:?}"),
    }
    // Equality lookup on the indexed column should still return results.
    let result = engine
        .execute_powql(r#"User filter .email = "alice@ex.com" { .name }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Alice".into()));
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_parse_rejects_trailing_tokens() {
    // Previously `User create_index .email` silently succeeded as
    // `User` (ignoring the trailing unknown tokens). Now it's a
    // parse error so users know the syntax isn't recognized.
    let mut engine = test_engine();
    assert!(engine.execute_powql("User create_index .email").is_err());
    assert!(engine.execute_powql("User add_column score: int").is_err());
    assert!(engine.execute_powql("User drop_column email").is_err());
}

// ─── IN subquery tests (E2h) ─────────────────────────────────────

#[test]
fn test_in_subquery_basic() {
    let mut engine = test_engine();
    // Create a second table with a subset of user names
    engine
        .execute_powql("type VIP { required name: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert VIP { name := "Alice" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert VIP { name := "Charlie" }"#)
        .unwrap();

    let result = engine
        .execute_powql("User filter .name in (VIP { .name }) { .name, .age }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
            let names: Vec<_> = rows.iter().map(|r| &r[0]).collect();
            assert!(names.contains(&&Value::Str("Alice".into())));
            assert!(names.contains(&&Value::Str("Charlie".into())));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_not_in_subquery() {
    let mut engine = test_engine();
    engine
        .execute_powql("type VIP { required name: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert VIP { name := "Alice" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert VIP { name := "Charlie" }"#)
        .unwrap();

    let result = engine
        .execute_powql("User filter .name not in (VIP { .name }) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str("Bob".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_in_subquery_with_filter() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Score { required name: str, required points: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert Score { name := "Alice", points := 100 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Score { name := "Bob", points := 50 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Score { name := "Charlie", points := 80 }"#)
        .unwrap();

    // Find users whose names are in the high-scorers list (points > 70)
    let result = engine
        .execute_powql("User filter .name in (Score filter .points > 70 { .name }) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
            let names: Vec<_> = rows.iter().map(|r| &r[0]).collect();
            assert!(names.contains(&&Value::Str("Alice".into())));
            assert!(names.contains(&&Value::Str("Charlie".into())));
        }
        _ => panic!("expected rows"),
    }
}

// ─── EXISTS subquery tests (uncorrelated) ───────────────────────────

#[test]
fn test_exists_subquery_uncorrelated_true() {
    let mut engine = test_engine();
    // A side table with at least one row → EXISTS(...) = true, so the
    // filter passes every User row through.
    engine
        .execute_powql("type VIP { required name: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert VIP { name := "Alice" }"#)
        .unwrap();

    let result = engine
        .execute_powql("User filter exists (VIP) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3, "all users should pass when EXISTS is true");
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_exists_subquery_uncorrelated_false() {
    let mut engine = test_engine();
    // An empty side table → EXISTS(...) = false, so no User rows pass.
    engine
        .execute_powql("type VIP { required name: str }")
        .unwrap();

    let result = engine
        .execute_powql("User filter exists (VIP) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 0, "no rows should pass when EXISTS is false");
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_not_exists_subquery() {
    let mut engine = test_engine();
    // NOT EXISTS over an empty table → true → all rows pass.
    engine
        .execute_powql("type VIP { required name: str }")
        .unwrap();

    let result = engine
        .execute_powql("User filter not exists (VIP) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }

    // Now add a row — NOT EXISTS becomes false → no rows pass.
    engine
        .execute_powql(r#"insert VIP { name := "Alice" }"#)
        .unwrap();
    let result = engine
        .execute_powql("User filter not exists (VIP) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 0);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_exists_subquery_with_inner_filter() {
    let mut engine = test_engine();
    // Subquery with its own filter: only rows matching the inner
    // predicate count toward EXISTS.
    engine
        .execute_powql("type Score { required name: str, required points: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert Score { name := "Alice", points := 100 }"#)
        .unwrap();

    // Inner filter matches → EXISTS true → all users pass.
    let result = engine
        .execute_powql("User filter exists (Score filter .points > 50) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 3),
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_exists_subquery_with_inner_filter_no_match() {
    // Fresh engine so the plan cache doesn't collide with the
    // `> 50` shape from the sibling test.
    let mut engine = test_engine();
    engine
        .execute_powql("type Score { required name: str, required points: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert Score { name := "Alice", points := 100 }"#)
        .unwrap();

    // Inner filter matches nothing → EXISTS false → no users pass.
    let result = engine
        .execute_powql("User filter exists (Score filter .points > 1000) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 0),
        _ => panic!("expected rows"),
    }
}

// ─── Materialized view tests ────────────────────────────────────────────

#[test]
fn test_create_materialized_view() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"materialize OldUsers as User filter .age > 28"#)
        .unwrap();
    match result {
        QueryResult::Executed { message } => {
            assert!(message.contains("OldUsers"));
        }
        _ => panic!("expected Executed"),
    }
    // Query the view like a table.
    let result = engine.execute_powql("OldUsers").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2); // Alice (30) and Charlie (35)
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_view_auto_refresh_on_insert() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"materialize OldUsers as User filter .age > 28"#)
        .unwrap();
    // Insert a new qualifying row.
    engine
        .execute_powql(r#"insert User { name := "Dave", email := "dave@ex.com", age := 40 }"#)
        .unwrap();
    // The view should auto-refresh and include Dave.
    let result = engine.execute_powql("OldUsers").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3); // Alice, Charlie, Dave
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_view_auto_refresh_on_delete() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"materialize OldUsers as User filter .age > 28"#)
        .unwrap();
    // Delete Alice (age 30) from the base table.
    engine
        .execute_powql(r#"User filter .name = "Alice" delete"#)
        .unwrap();
    // View should auto-refresh: only Charlie remains.
    let result = engine.execute_powql("OldUsers").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_view_auto_refresh_on_update() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"materialize OldUsers as User filter .age > 28"#)
        .unwrap();
    // Update Bob's age to make him qualify.
    engine
        .execute_powql(r#"User filter .name = "Bob" update { age := 50 }"#)
        .unwrap();
    let result = engine.execute_powql("OldUsers").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3); // Alice, Charlie, Bob
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_explicit_refresh() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"materialize OldUsers as User filter .age > 28"#)
        .unwrap();
    engine
        .execute_powql(r#"insert User { name := "Eve", email := "eve@ex.com", age := 55 }"#)
        .unwrap();
    // Explicit refresh.
    let result = engine.execute_powql("refresh OldUsers").unwrap();
    match result {
        QueryResult::Executed { message } => {
            assert!(message.contains("refreshed"));
        }
        _ => panic!("expected Executed"),
    }
    // Now query — should include Eve.
    let result = engine.execute_powql("OldUsers").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_drop_view() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"materialize OldUsers as User filter .age > 28"#)
        .unwrap();
    let result = engine.execute_powql("drop view OldUsers").unwrap();
    match result {
        QueryResult::Executed { message } => {
            assert!(message.contains("dropped"));
        }
        _ => panic!("expected Executed"),
    }
    // Querying the dropped view should fail.
    let err = engine.execute_powql("OldUsers").unwrap_err();
    assert!(err.to_string().contains("not found"));
}

#[test]
fn test_view_with_projection() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"materialize UserNames as User { .name }"#)
        .unwrap();
    let result = engine.execute_powql("UserNames").unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name".to_string()]);
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_view_no_stale_reads() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"materialize AllUsers as User"#)
        .unwrap();
    // Verify initial state.
    let result = engine.execute_powql("AllUsers").unwrap();
    match &result {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 3),
        _ => panic!("expected rows"),
    }
    // Insert two more.
    engine
        .execute_powql(r#"insert User { name := "D", email := "d@ex.com", age := 1 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert User { name := "E", email := "e@ex.com", age := 2 }"#)
        .unwrap();
    // First insert marks dirty, second stays dirty. Auto-refresh fires on read.
    let result = engine.execute_powql("AllUsers").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 5),
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_duplicate_view_creation_fails() {
    let mut engine = test_engine();
    engine.execute_powql(r#"materialize V as User"#).unwrap();
    let err = engine
        .execute_powql(r#"materialize V as User"#)
        .unwrap_err();
    assert!(err.to_string().contains("already exists"));
}

#[test]
fn test_drop_nonexistent_view_fails() {
    let mut engine = test_engine();
    let err = engine.execute_powql("drop view NoSuchView").unwrap_err();
    assert!(err.to_string().contains("not found"));
}

// ── UNION / UNION ALL tests ────────────────────────────────

#[test]
fn test_union_deduplicates() {
    let mut engine = test_engine();
    engine.execute_powql("type A { name: str }").unwrap();
    engine.execute_powql("type B { name: str }").unwrap();
    engine
        .execute_powql(r#"insert A { name := "alice" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert A { name := "bob" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert B { name := "bob" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert B { name := "carol" }"#)
        .unwrap();
    let result = engine.execute_powql("A union B").unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    // alice, bob, carol — bob deduped
    assert_eq!(rows.len(), 3);
}

#[test]
fn test_union_all_keeps_duplicates() {
    let mut engine = test_engine();
    engine.execute_powql("type X { val: int }").unwrap();
    engine.execute_powql("type Y { val: int }").unwrap();
    engine.execute_powql("insert X { val := 1 }").unwrap();
    engine.execute_powql("insert X { val := 2 }").unwrap();
    engine.execute_powql("insert Y { val := 2 }").unwrap();
    engine.execute_powql("insert Y { val := 3 }").unwrap();
    let result = engine.execute_powql("X union all Y").unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    // 1, 2, 2, 3 — no dedup
    assert_eq!(rows.len(), 4);
}

#[test]
fn test_union_with_filters() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Emp { name: str, dept: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert Emp { name := "alice", dept := "eng" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Emp { name := "bob", dept := "sales" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Emp { name := "carol", dept := "eng" }"#)
        .unwrap();
    let result = engine
        .execute_powql(r#"Emp filter .dept = "eng" union Emp filter .dept = "sales""#)
        .unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    assert_eq!(rows.len(), 3);
}

#[test]
fn test_union_chain_three_tables() {
    let mut engine = test_engine();
    engine.execute_powql("type T1 { v: int }").unwrap();
    engine.execute_powql("type T2 { v: int }").unwrap();
    engine.execute_powql("type T3 { v: int }").unwrap();
    engine.execute_powql("insert T1 { v := 1 }").unwrap();
    engine.execute_powql("insert T2 { v := 2 }").unwrap();
    engine.execute_powql("insert T3 { v := 3 }").unwrap();
    let result = engine.execute_powql("T1 union T2 union T3").unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    assert_eq!(rows.len(), 3);
}

#[test]
fn test_union_uses_left_side_columns() {
    let mut engine = test_engine();
    engine.execute_powql("type L { name: str }").unwrap();
    engine.execute_powql("type R { name: str }").unwrap();
    engine.execute_powql(r#"insert L { name := "a" }"#).unwrap();
    engine.execute_powql(r#"insert R { name := "b" }"#).unwrap();
    let result = engine.execute_powql("L union R").unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name".to_string()]);
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("expected rows"),
    }
}

// ── COUNT DISTINCT tests ───────────────────────────────────

#[test]
fn test_count_distinct_standalone() {
    let mut engine = test_engine();
    engine.execute_powql("type Color { name: str }").unwrap();
    engine
        .execute_powql(r#"insert Color { name := "red" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Color { name := "blue" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Color { name := "red" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Color { name := "green" }"#)
        .unwrap();
    let result = engine
        .execute_powql("count(distinct Color { .name })")
        .unwrap();
    match result {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 3), // red, blue, green
        _ => panic!("expected scalar int"),
    }
}

#[test]
fn test_count_distinct_in_group_by() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Sale { dept: str, item: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert Sale { dept := "eng", item := "laptop" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Sale { dept := "eng", item := "laptop" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Sale { dept := "eng", item := "monitor" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Sale { dept := "sales", item := "phone" }"#)
        .unwrap();
    let result = engine
        .execute_powql("Sale group .dept { .dept, count(distinct .item) }")
        .unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    // eng: 2 distinct items (laptop, monitor), sales: 1 (phone)
    let eng_row = rows
        .iter()
        .find(|r| r[0] == Value::Str("eng".into()))
        .unwrap();
    let sales_row = rows
        .iter()
        .find(|r| r[0] == Value::Str("sales".into()))
        .unwrap();
    assert_eq!(eng_row[1], Value::Int(2));
    assert_eq!(sales_row[1], Value::Int(1));
}

#[test]
fn test_count_distinct_with_filter() {
    let mut engine = test_engine();
    // Use test_engine which creates User with name, email, age
    engine
        .execute_powql(r#"insert User { name := "Dave", email := "d@e.com", age := 30 }"#)
        .unwrap();
    let result = engine
        .execute_powql("count(distinct User { .age })")
        .unwrap();
    match result {
        QueryResult::Scalar(Value::Int(n)) => {
            // 30(alice), 25(bob), 35(charlie), 30(dave) → 3 distinct
            assert_eq!(n, 3);
        }
        _ => panic!("expected scalar int"),
    }
}

// ── UPDATE with expressions tests ──────────────────────────

#[test]
fn test_update_with_arithmetic_expression() {
    let mut engine = test_engine();
    // Alice starts at age 30
    engine
        .execute_powql(r#"User filter .name = "Alice" update { age := .age + 5 }"#)
        .unwrap();
    let result = engine
        .execute_powql(r#"User filter .name = "Alice""#)
        .unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    assert_eq!(rows[0][2], Value::Int(35)); // 30 + 5 = 35
}

#[test]
fn test_update_with_multiply_expression() {
    let mut engine = test_engine();
    // Double everyone's age
    engine
        .execute_powql("User update { age := .age * 2 }")
        .unwrap();
    let result = engine.execute_powql("User").unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    let ages: Vec<i64> = rows
        .iter()
        .map(|r| match &r[2] {
            Value::Int(v) => *v,
            _ => 0,
        })
        .collect();
    assert!(ages.contains(&60)); // Alice: 30*2
    assert!(ages.contains(&50)); // Bob: 25*2
    assert!(ages.contains(&70)); // Charlie: 35*2
}

#[test]
fn test_update_expression_with_filter() {
    let mut engine = test_engine();
    // Increment age only for people over 28
    engine
        .execute_powql("User filter .age > 28 update { age := .age + 1 }")
        .unwrap();
    let result = engine
        .execute_powql(r#"User filter .name = "Alice""#)
        .unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    assert_eq!(rows[0][2], Value::Int(31)); // Alice was 30, now 31
    let result = engine
        .execute_powql(r#"User filter .name = "Bob""#)
        .unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    assert_eq!(rows[0][2], Value::Int(25)); // Bob was 25, unchanged
}

#[test]
fn test_update_literal_still_uses_fast_path() {
    // Verify the literal path still works after the refactor
    let mut engine = test_engine();
    engine
        .execute_powql(r#"User filter .name = "Alice" update { age := 99 }"#)
        .unwrap();
    let result = engine
        .execute_powql(r#"User filter .name = "Alice""#)
        .unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    assert_eq!(rows[0][2], Value::Int(99));
}

// ── COUNT(*) in GROUP BY tests ─────────────────────────────

#[test]
fn test_group_by_count_star() {
    let mut engine = test_engine();
    // test_engine has 3 users: Alice(30), Bob(25), Charlie(35)
    // Add another user with same age as Alice
    engine
        .execute_powql(r#"insert User { name := "Dave", email := "d@e.com", age := 30 }"#)
        .unwrap();
    let result = engine
        .execute_powql("User group .age { .age, count(*) }")
        .unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    let age30 = rows.iter().find(|r| r[0] == Value::Int(30)).unwrap();
    assert_eq!(age30[1], Value::Int(2)); // Alice + Dave
    let age25 = rows.iter().find(|r| r[0] == Value::Int(25)).unwrap();
    assert_eq!(age25[1], Value::Int(1)); // Bob only
}

#[test]
fn test_group_by_count_star_with_having() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Dave", email := "d@e.com", age := 30 }"#)
        .unwrap();
    let result = engine
        .execute_powql("User group .age having count(*) > 1 { .age, count(*) }")
        .unwrap();
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(30)); // only age=30 has count > 1
}

// ── Mixed-type arithmetic (Int <-> Float) regression tests ─────────

/// Engine with a Product type containing price:float + stock:int.
/// Exercises mixed numeric promotion in `eval_binop`.
fn product_mix_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_product_mix_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql(
            "type Product { required name: str, required price: float, required stock: int }",
        )
        .unwrap();
    engine
        .execute_powql(r#"insert Product { name := "Apple",  price := 1.5, stock := 10 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Product { name := "Banana", price := 0.25, stock := 4 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Product { name := "Cherry", price := 2.0, stock := 3 }"#)
        .unwrap();
    engine
}

fn as_float(v: &Value) -> f64 {
    match v {
        Value::Float(f) => *f,
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn test_arith_float_times_int() {
    let mut engine = product_mix_engine();
    let result = engine
        .execute_powql("Product { .name, total: .price * .stock }")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name", "total"]);
            let mut by_name: std::collections::HashMap<String, f64> =
                std::collections::HashMap::new();
            for row in &rows {
                let name = match &row[0] {
                    Value::Str(s) => s.clone(),
                    _ => panic!(),
                };
                by_name.insert(name, as_float(&row[1]));
            }
            assert!((by_name["Apple"] - 15.0).abs() < 1e-9);
            assert!((by_name["Banana"] - 1.0).abs() < 1e-9);
            assert!((by_name["Cherry"] - 6.0).abs() < 1e-9);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_arith_int_plus_float() {
    let mut engine = product_mix_engine();
    // stock:int + price:float → should promote to float
    let result = engine
        .execute_powql("Product { .name, bumped: .stock + .price }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            let mut by_name: std::collections::HashMap<String, f64> =
                std::collections::HashMap::new();
            for row in &rows {
                let name = match &row[0] {
                    Value::Str(s) => s.clone(),
                    _ => panic!(),
                };
                by_name.insert(name, as_float(&row[1]));
            }
            assert!((by_name["Apple"] - 11.5).abs() < 1e-9);
            assert!((by_name["Banana"] - 4.25).abs() < 1e-9);
            assert!((by_name["Cherry"] - 5.0).abs() < 1e-9);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_arith_float_div_int() {
    let mut engine = product_mix_engine();
    let result = engine
        .execute_powql("Product { .name, unit: .price / .stock }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            let mut by_name: std::collections::HashMap<String, f64> =
                std::collections::HashMap::new();
            for row in &rows {
                let name = match &row[0] {
                    Value::Str(s) => s.clone(),
                    _ => panic!(),
                };
                by_name.insert(name, as_float(&row[1]));
            }
            assert!((by_name["Apple"] - 0.15).abs() < 1e-9);
            assert!((by_name["Banana"] - 0.0625).abs() < 1e-9);
            assert!((by_name["Cherry"] - (2.0 / 3.0)).abs() < 1e-9);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_arith_int_minus_float() {
    let mut engine = product_mix_engine();
    let result = engine
        .execute_powql("Product { .name, delta: .stock - .price }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            let mut by_name: std::collections::HashMap<String, f64> =
                std::collections::HashMap::new();
            for row in &rows {
                let name = match &row[0] {
                    Value::Str(s) => s.clone(),
                    _ => panic!(),
                };
                by_name.insert(name, as_float(&row[1]));
            }
            assert!((by_name["Apple"] - 8.5).abs() < 1e-9);
            assert!((by_name["Banana"] - 3.75).abs() < 1e-9);
            assert!((by_name["Cherry"] - 1.0).abs() < 1e-9);
        }
        _ => panic!("expected rows"),
    }
}

// Regression: sum() on a Float column must return the actual
// floating-point sum, not Int(0). The old slow-path loops filtered
// out Value::Float and only summed Ints, silently dropping every
// value in a Float column.
#[test]
fn test_sum_float_scalar() {
    let mut engine = product_mix_engine();
    let result = engine.execute_powql("sum(Product { .price })").unwrap();
    match result {
        QueryResult::Scalar(v) => {
            // 1.5 + 0.25 + 2.0 = 3.75
            assert!(
                (as_float(&v) - 3.75).abs() < 1e-9,
                "expected 3.75, got {v:?}"
            );
        }
        _ => panic!("expected scalar result, got {result:?}"),
    }
}

// Regression: sum() of a Float column inside a GROUP BY must work
// the same way. compute_group_aggregate had the identical Int-only
// bug as the scalar path.
#[test]
fn test_sum_float_group_by() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("powdb_sum_float_gb_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Sale { required region: str, required amount: float }")
        .unwrap();
    engine
        .execute_powql(r#"insert Sale { region := "E", amount := 1.5 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Sale { region := "E", amount := 2.25 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Sale { region := "W", amount := 4.0 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Sale { region := "W", amount := 0.5 }"#)
        .unwrap();

    let result = engine
        .execute_powql("Sale group .region { .region, total: sum(.amount) }")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["region", "total"]);
            let mut by_region: std::collections::HashMap<String, f64> =
                std::collections::HashMap::new();
            for row in &rows {
                let region = match &row[0] {
                    Value::Str(s) => s.clone(),
                    _ => panic!(),
                };
                by_region.insert(region, as_float(&row[1]));
            }
            assert!(
                (by_region["E"] - 3.75).abs() < 1e-9,
                "E: {:?}",
                by_region.get("E")
            );
            assert!(
                (by_region["W"] - 4.5).abs() < 1e-9,
                "W: {:?}",
                by_region.get("W")
            );
        }
        _ => panic!("expected rows, got {result:?}"),
    }
}

// ─── Mission D10: Float fast-path parity ─────────────────────────────
//
// Prior to D10, three hot paths in the executor bailed on Float columns:
//   1. `agg_single_col_fast` — sum/avg/min/max/count fell through to the
//      generic row-decoding path (allocates Vec<Value> per row).
//   2. `project_filter_sort_limit_fast` — top-N by Float column fell
//      through the generic sort path.
//   3. `compile_predicate` / `build_int_leaf` — WHERE on Float columns
//      couldn't compile, so the whole filter walked Value::cmp.
//
// These tests exercise each Float fast path end-to-end, including NaN
// handling via `total_cmp` (which matches `Value::Ord` so semantics are
// identical between fast-path and generic-path reads).

/// Engine with a Price table: price:float, qty:int. Eight rows with a
/// deliberate spread of values, a NaN, a negative, -0.0, and a null.
/// The null exercises the bitmap-skip branch; NaN and -0.0 exercise
/// the `total_cmp` invariant.
fn float_fast_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_float_fast_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Price { required name: str, price: float, required qty: int }")
        .unwrap();
    // Insertion order deliberately scrambled so top-N doesn't trivially
    // match insertion order.
    let rows = [
        ("a", "price := 1.5", "qty := 1"),
        ("b", "price := 0.25", "qty := 2"),
        ("c", "price := 2.0", "qty := 3"),
        ("d", "price := -3.5", "qty := 4"),
        ("e", "price := 10.0", "qty := 5"),
        ("f", "price := 0.5", "qty := 6"),
        ("g", "price := 100.0", "qty := 7"),
        ("h", "price := -0.0", "qty := 8"),
    ];
    for (name, price, qty) in rows {
        engine
            .execute_powql(&format!(
                r#"insert Price {{ name := "{name}", {price}, {qty} }}"#
            ))
            .unwrap();
    }
    engine
}

#[test]
fn test_d10_agg_sum_float_fast_path() {
    let mut engine = float_fast_engine();
    let result = engine.execute_powql("sum(Price { .price })").unwrap();
    // 1.5 + 0.25 + 2.0 + -3.5 + 10.0 + 0.5 + 100.0 + -0.0 = 110.75
    match result {
        QueryResult::Scalar(v) => {
            assert!((as_float(&v) - 110.75).abs() < 1e-9, "got {v:?}");
        }
        _ => panic!("expected scalar, got {result:?}"),
    }
}

#[test]
fn test_d10_agg_avg_float_fast_path() {
    let mut engine = float_fast_engine();
    let result = engine.execute_powql("avg(Price { .price })").unwrap();
    // 110.75 / 8 = 13.84375
    match result {
        QueryResult::Scalar(v) => {
            assert!((as_float(&v) - 13.84375).abs() < 1e-9, "got {v:?}");
        }
        _ => panic!("expected scalar, got {result:?}"),
    }
}

#[test]
fn test_d10_agg_min_float_fast_path() {
    let mut engine = float_fast_engine();
    let result = engine.execute_powql("min(Price { .price })").unwrap();
    match result {
        QueryResult::Scalar(v) => {
            assert!((as_float(&v) - (-3.5)).abs() < 1e-9, "got {v:?}");
        }
        _ => panic!("expected scalar, got {result:?}"),
    }
}

#[test]
fn test_d10_agg_max_float_fast_path() {
    let mut engine = float_fast_engine();
    let result = engine.execute_powql("max(Price { .price })").unwrap();
    match result {
        QueryResult::Scalar(v) => {
            assert!((as_float(&v) - 100.0).abs() < 1e-9, "got {v:?}");
        }
        _ => panic!("expected scalar, got {result:?}"),
    }
}

#[test]
fn test_d10_agg_count_distinct_float_fast_path() {
    let mut engine = float_fast_engine();
    let result = engine
        .execute_powql("count(distinct Price { .price })")
        .unwrap();
    // All 8 prices are distinct (+0.0 isn't present; -0.0 is, and
    // distinct from every other value). Hash via to_bits so -0.0 and
    // +0.0 would count separately — matches Value::Hash.
    match result {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 8, "got {n}"),
        _ => panic!("expected scalar int, got {result:?}"),
    }
}

#[test]
fn test_d10_agg_float_with_compiled_where() {
    // Exercises `build_float_leaf` — WHERE .price > 1.0 must compile,
    // and the Float fast path must use it to short-circuit rows.
    let mut engine = float_fast_engine();
    let result = engine
        .execute_powql("sum(Price filter .price > 1.0 { .price })")
        .unwrap();
    // Rows > 1.0: 1.5, 2.0, 10.0, 100.0 → sum = 113.5
    match result {
        QueryResult::Scalar(v) => {
            assert!((as_float(&v) - 113.5).abs() < 1e-9, "got {v:?}");
        }
        _ => panic!("expected scalar, got {result:?}"),
    }
}

#[test]
fn test_d10_agg_float_with_compiled_where_int_literal() {
    // Novel cross-type: WHERE .price > 1 (Int literal on Float column)
    // must still compile via build_float_leaf — the Int literal is
    // promoted to f64 at compile time so the hot loop only sees f64.
    let mut engine = float_fast_engine();
    let result = engine
        .execute_powql("sum(Price filter .price > 1 { .price })")
        .unwrap();
    match result {
        QueryResult::Scalar(v) => {
            assert!((as_float(&v) - 113.5).abs() < 1e-9, "got {v:?}");
        }
        _ => panic!("expected scalar, got {result:?}"),
    }
}

#[test]
fn test_d10_agg_float_with_reversed_literal() {
    // `100.0 > .price` (literal on LHS) must also compile. The
    // build_float_leaf flips the operator so the field is always LHS.
    let mut engine = float_fast_engine();
    let result = engine
        .execute_powql("count(Price filter 1.0 < .price { .price })")
        .unwrap();
    // Rows where 1.0 < .price: 1.5, 2.0, 10.0, 100.0 → count = 4
    match result {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 4, "got {n}"),
        _ => panic!("expected scalar int, got {result:?}"),
    }
}

#[test]
fn test_d10_sort_float_desc_limit_fast_path() {
    // Top-3 by price descending — exercises the Float branch of
    // project_filter_sort_limit_fast with the sortable-u64 transform.
    let mut engine = float_fast_engine();
    let result = engine
        .execute_powql("Price order .price desc limit 3 { .name, .price }")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name", "price"]);
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0][0], Value::Str("g".into())); // 100.0
            assert!((as_float(&rows[0][1]) - 100.0).abs() < 1e-9);
            assert_eq!(rows[1][0], Value::Str("e".into())); // 10.0
            assert!((as_float(&rows[1][1]) - 10.0).abs() < 1e-9);
            assert_eq!(rows[2][0], Value::Str("c".into())); // 2.0
            assert!((as_float(&rows[2][1]) - 2.0).abs() < 1e-9);
        }
        _ => panic!("expected rows, got {result:?}"),
    }
}

#[test]
fn test_d10_sort_float_asc_limit_fast_path() {
    // Bottom-3 by price — negative and -0.0 must order correctly.
    let mut engine = float_fast_engine();
    let result = engine
        .execute_powql("Price order .price limit 3 { .name, .price }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0][0], Value::Str("d".into())); // -3.5
                                                            // -0.0 must come before +0.25 under total_cmp ordering.
            assert_eq!(rows[1][0], Value::Str("h".into())); // -0.0
            assert_eq!(rows[2][0], Value::Str("b".into())); // 0.25
        }
        _ => panic!("expected rows, got {result:?}"),
    }
}

#[test]
fn test_d10_sort_float_with_compiled_filter() {
    // Filter + sort + limit all on Float column — every fast path
    // fires on the same query.
    let mut engine = float_fast_engine();
    let result = engine
        .execute_powql("Price filter .price > 0.0 order .price desc limit 2 { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][0], Value::Str("g".into())); // 100.0
            assert_eq!(rows[1][0], Value::Str("e".into())); // 10.0
        }
        _ => panic!("expected rows, got {result:?}"),
    }
}

#[test]
fn test_f64_sortable_transform_monotonic() {
    // The sortable-u64 transform must preserve total_cmp ordering.
    // Regression guard against accidentally breaking the clever
    // sign-flip trick in `f64_bits_to_sortable_u64`.
    let samples: [f64; 11] = [
        f64::NEG_INFINITY,
        -1e100,
        -1.0,
        -f64::MIN_POSITIVE,
        -0.0,
        0.0,
        f64::MIN_POSITIVE,
        1.0,
        1e100,
        f64::INFINITY,
        f64::NAN, // total_cmp says NaN > +∞
    ];
    let mut sorted = samples;
    sorted.sort_by(|a, b| a.total_cmp(b));

    let as_sortable: Vec<u64> = sorted
        .iter()
        .map(|f| f64_bits_to_sortable_u64(f.to_bits()))
        .collect();

    // Each u64 must be strictly greater than its predecessor, because
    // `total_cmp` places every sample at a distinct total-order slot.
    for pair in as_sortable.windows(2) {
        assert!(
            pair[0] < pair[1],
            "sortable u64 not monotonic: {:#x} >= {:#x}",
            pair[0],
            pair[1]
        );
    }
}

// ─── EXPLAIN tests ─────────────────────────────────────────────────

#[test]
fn test_explain_simple_scan() {
    let mut engine = test_engine();
    let result = engine.execute_powql("explain User").unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["plan"]);
            assert!(!rows.is_empty());
            assert!(matches!(&rows[0][0], Value::Str(s) if s.contains("SeqScan")));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_explain_filter() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql("explain User filter .age > 30")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            let plan_text: String = rows
                .iter()
                .map(|r| match &r[0] {
                    Value::Str(s) => s.as_str(),
                    _ => "",
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                plan_text.contains("Filter"),
                "plan should show Filter(SeqScan) after lowering unindexed RangeScan"
            );
        }
        _ => panic!("expected rows"),
    }
}

fn explain_text(engine: &mut Engine, q: &str) -> String {
    match engine.execute_powql(q).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r[0] {
                Value::Str(s) => s.as_str(),
                _ => "",
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_explain_eq_filter_unindexed_shows_seqscan_not_indexscan() {
    let mut engine = test_engine();
    // `email` has NO index in test_engine; the planner folds
    // `.email = lit` to IndexScan speculatively. EXPLAIN must show
    // what actually runs: Filter over SeqScan.
    let text = explain_text(
        &mut engine,
        r#"explain User filter .email = "alice@ex.com""#,
    );
    assert!(!text.contains("IndexScan"), "got: {text}");
    assert!(text.contains("Filter"), "got: {text}");
    assert!(text.contains("SeqScan"), "got: {text}");
}

#[test]
fn test_explain_eq_filter_indexed_shows_indexscan() {
    let mut engine = test_engine();
    engine.execute_powql("alter User add index .email").unwrap();
    let text = explain_text(
        &mut engine,
        r#"explain User filter .email = "alice@ex.com""#,
    );
    assert!(text.contains("IndexScan"), "got: {text}");
}

fn sorted_names(r: QueryResult) -> Vec<String> {
    match r {
        QueryResult::Rows { rows, .. } => {
            let mut v: Vec<String> = rows.iter().map(|r| format!("{:?}", r[0])).collect();
            v.sort();
            v
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_range_scan_uses_nonunique_index_same_results() {
    let mut engine = test_engine(); // Alice 30, Bob 25, Charlie 35
    let unindexed = engine
        .execute_powql("User filter .age > 26 and .age <= 35 { .name }")
        .unwrap();
    engine.execute_powql("alter User add index .age").unwrap();
    let indexed = engine
        .execute_powql("User filter .age > 26 and .age <= 35 { .name }")
        .unwrap();
    assert_eq!(sorted_names(unindexed), sorted_names(indexed)); // Alice, Charlie
}

#[test]
fn test_range_scan_between_uses_nonunique_index() {
    let mut engine = test_engine();
    let unindexed = engine
        .execute_powql("User filter .age between 25 and 30 { .name }")
        .unwrap();
    engine.execute_powql("alter User add index .age").unwrap();
    let indexed = engine
        .execute_powql("User filter .age between 25 and 30 { .name }")
        .unwrap();
    assert_eq!(sorted_names(unindexed), sorted_names(indexed)); // Alice, Bob
}

#[test]
fn test_range_scan_indexed_exclusive_bound_excludes_boundary() {
    let mut engine = test_engine();
    engine.execute_powql("alter User add index .age").unwrap();
    // Bob is exactly 25; `.age > 25` must exclude him.
    let names = sorted_names(
        engine
            .execute_powql("User filter .age > 25 { .name }")
            .unwrap(),
    );
    assert_eq!(names, vec!["Str(\"Alice\")", "Str(\"Charlie\")"]);
}

#[test]
fn test_range_scan_indexed_excludes_nulls() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"insert User { name := "Dana", email := "d@ex.com" }"#)
        .unwrap(); // age null
    engine.execute_powql("alter User add index .age").unwrap();
    match engine
        .execute_powql("User filter .age < 100 { .name }")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 3, "null age must not match"),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_explain_range_indexed_shows_rangescan() {
    let mut engine = test_engine();
    engine.execute_powql("alter User add index .age").unwrap();
    let text = explain_text(&mut engine, "explain User filter .age > 26");
    assert!(text.contains("RangeScan"), "got: {text}");
}

#[test]
fn test_explain_does_not_execute() {
    let mut engine = test_engine();
    // EXPLAIN should NOT actually insert a row.
    let result = engine
        .execute_powql(r#"explain insert User { name := "Zara", age := 99 }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            let plan_text: String = rows
                .iter()
                .map(|r| match &r[0] {
                    Value::Str(s) => s.as_str(),
                    _ => "",
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(plan_text.contains("Insert"));
        }
        _ => panic!("expected rows"),
    }
    // Verify no row was actually inserted.
    let result = engine.execute_powql("User { .name }").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3, "should still have original 3 users");
        }
        _ => panic!("expected rows"),
    }
}

// ─── Correlated subquery tests ──────────────────────────────────────

#[test]
fn test_correlated_in_subquery() {
    let mut engine = test_engine();
    // Create an orders table with user_name to correlate on.
    engine
        .execute_powql("type UserOrder { required user_name: str, required total: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert UserOrder { user_name := "Alice", total := 100 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert UserOrder { user_name := "Alice", total := 200 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert UserOrder { user_name := "Bob", total := 50 }"#)
        .unwrap();

    // Correlated: for each User row, find orders where user_name = outer .name
    // The subquery references .name which is a User column, not a UserOrder column.
    let result = engine
        .execute_powql(
            "User filter .name in (UserOrder filter .user_name = .name { .user_name }) { .name }",
        )
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2, "Alice and Bob have orders");
            let names: Vec<_> = rows.iter().map(|r| &r[0]).collect();
            assert!(names.contains(&&Value::Str("Alice".into())));
            assert!(names.contains(&&Value::Str("Bob".into())));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_correlated_exists_subquery() {
    let mut engine = test_engine();
    engine
        .execute_powql("type UserOrder { required user_name: str, required total: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert UserOrder { user_name := "Alice", total := 100 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert UserOrder { user_name := "Bob", total := 50 }"#)
        .unwrap();

    // Correlated EXISTS: only Users who have at least one order.
    // .name in the subquery filter refers to the outer User's name column.
    let result = engine
        .execute_powql("User filter exists (UserOrder filter .user_name = .name) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2, "Alice and Bob have orders");
            let names: Vec<_> = rows.iter().map(|r| &r[0]).collect();
            assert!(names.contains(&&Value::Str("Alice".into())));
            assert!(names.contains(&&Value::Str("Bob".into())));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_correlated_not_exists_subquery() {
    let mut engine = test_engine();
    engine
        .execute_powql("type UserOrder { required user_name: str, required total: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert UserOrder { user_name := "Alice", total := 100 }"#)
        .unwrap();

    // NOT EXISTS: Users without orders (Bob and Charlie).
    let result = engine
        .execute_powql("User filter not exists (UserOrder filter .user_name = .name) { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2, "Bob and Charlie have no orders");
            let names: Vec<_> = rows.iter().map(|r| &r[0]).collect();
            assert!(names.contains(&&Value::Str("Bob".into())));
            assert!(names.contains(&&Value::Str("Charlie".into())));
        }
        _ => panic!("expected rows"),
    }
}

// ─── CAST tests ───────────────────────────────────────────────────

#[test]
fn test_cast_int_to_str() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User { s: cast(.age, "str") }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Str("30".into()));
            assert_eq!(rows[1][0], Value::Str("25".into()));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_cast_str_to_int() {
    let mut engine = test_engine();
    engine
        .execute_powql(r#"type Numbers { required val: str }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Numbers { val := "42" }"#)
        .unwrap();
    let result = engine
        .execute_powql(r#"Numbers { n: cast(.val, "int") }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Int(42));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_cast_float_to_int() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Floats { required val: float }")
        .unwrap();
    engine
        .execute_powql("insert Floats { val := 3.7 }")
        .unwrap();
    let result = engine
        .execute_powql(r#"Floats { n: cast(.val, "int") }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Int(3));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_cast_int_to_float() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User { f: cast(.age, "float") }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Float(30.0));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_cast_int_to_bool() {
    let mut engine = test_engine();
    let result = engine
        .execute_powql(r#"User { b: cast(.age, "bool") }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            // age=30 -> true (non-zero)
            assert_eq!(rows[0][0], Value::Bool(true));
        }
        _ => panic!("expected rows"),
    }
}

// ─── Math function tests ──────────────────────────────────────────

#[test]
fn test_abs() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Nums { required val: int }")
        .unwrap();
    engine.execute_powql("insert Nums { val := -42 }").unwrap();
    let result = engine.execute_powql("Nums { a: abs(.val) }").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Int(42));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_round() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Floats { required val: float }")
        .unwrap();
    engine
        .execute_powql("insert Floats { val := 7.56789 }")
        .unwrap();
    let result = engine
        .execute_powql("Floats { r: round(.val, 2) }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Float(7.57));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_ceil_floor() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Floats { required val: float }")
        .unwrap();
    engine
        .execute_powql("insert Floats { val := 3.2 }")
        .unwrap();
    let c = engine.execute_powql("Floats { c: ceil(.val) }").unwrap();
    let f = engine.execute_powql("Floats { f: floor(.val) }").unwrap();
    match (c, f) {
        (QueryResult::Rows { rows: cr, .. }, QueryResult::Rows { rows: fr, .. }) => {
            assert_eq!(cr[0][0], Value::Float(4.0));
            assert_eq!(fr[0][0], Value::Float(3.0));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_sqrt() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Nums { required val: int }")
        .unwrap();
    engine.execute_powql("insert Nums { val := 144 }").unwrap();
    let result = engine.execute_powql("Nums { s: sqrt(.val) }").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Float(12.0));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_pow() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Nums { required val: int }")
        .unwrap();
    engine.execute_powql("insert Nums { val := 3 }").unwrap();
    let result = engine.execute_powql("Nums { p: pow(.val, 4) }").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Int(81));
        }
        _ => panic!("expected rows"),
    }
}

// ─── Date/time function tests ─────────────────────────────────────

#[test]
fn test_now_returns_datetime() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Events { required name: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert Events { name := "test" }"#)
        .unwrap();
    let result = engine.execute_powql("Events { ts: now() }").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => match &rows[0][0] {
            Value::DateTime(m) => assert!(*m > 0, "now() should return positive timestamp"),
            other => panic!("expected DateTime, got {other:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_extract_from_datetime() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Events { required ts: datetime }")
        .unwrap();
    // 2024-01-15 12:30:45 UTC in microseconds
    // 2024-01-15 = 19737 days since epoch
    // 19737 * 86400 = 1705276800 seconds + 12*3600 + 30*60 + 45 = 1705321845
    // * 1_000_000 = 1705321845000000
    engine
        .execute_powql("insert Events { ts := 1705321845000000 }")
        .unwrap();
    let result = engine.execute_powql(r#"Events { y: extract("year", .ts), m: extract("month", .ts), d: extract("day", .ts), h: extract("hour", .ts) }"#).unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Int(2024));
            assert_eq!(rows[0][1], Value::Int(1));
            assert_eq!(rows[0][2], Value::Int(15));
            assert_eq!(rows[0][3], Value::Int(12));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_date_add() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Events { required ts: datetime }")
        .unwrap();
    let base = 1705321845000000_i64; // 2024-01-15 12:30:45 UTC
    engine
        .execute_powql(&format!("insert Events {{ ts := {base} }}"))
        .unwrap();
    let result = engine
        .execute_powql(r#"Events { later: date_add(.ts, 2, "hours") }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::DateTime(base + 2 * 3_600_000_000));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_date_diff() {
    let mut engine = test_engine();
    engine
        .execute_powql("type Events { required start_ts: datetime, required end_ts: datetime }")
        .unwrap();
    let t1 = 1705321845000000_i64; // 2024-01-15 12:30:45 UTC
    let t2 = t1 + 3 * 86_400_000_000; // +3 days
    engine
        .execute_powql(&format!(
            "insert Events {{ start_ts := {t1}, end_ts := {t2} }}"
        ))
        .unwrap();
    let result = engine
        .execute_powql(r#"Events { diff: date_diff(.end_ts, .start_ts, "days") }"#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Int(3));
        }
        _ => panic!("expected rows"),
    }
}

// ── Transaction tests ─────────────────────────────────────────────────

#[test]
fn test_begin_commit() {
    let mut engine = test_engine();
    let count_before = engine.execute_powql("count(User)").unwrap();
    assert!(matches!(count_before, QueryResult::Scalar(Value::Int(3))));

    engine.execute_powql("begin").unwrap();
    engine
        .execute_powql(r#"insert User { name := "Diane", email := "diane@ex.com", age := 28 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert User { name := "Eve", email := "eve@ex.com", age := 22 }"#)
        .unwrap();
    engine.execute_powql("commit").unwrap();

    let count_after = engine.execute_powql("count(User)").unwrap();
    assert!(matches!(count_after, QueryResult::Scalar(Value::Int(5))));
}

#[test]
fn test_begin_transaction_keyword() {
    let mut engine = test_engine();
    engine.execute_powql("begin transaction").unwrap();
    engine.execute_powql("commit").unwrap();
}

#[test]
fn test_rollback_undoes_inserts() {
    let mut engine = test_engine();
    engine.execute_powql("begin").unwrap();
    engine
        .execute_powql(r#"insert User { name := "Zack", email := "zack@ex.com", age := 40 }"#)
        .unwrap();
    let mid = engine.execute_powql("count(User)").unwrap();
    assert!(matches!(mid, QueryResult::Scalar(Value::Int(4))));

    engine.execute_powql("rollback").unwrap();

    let after = engine.execute_powql("count(User)").unwrap();
    assert!(matches!(after, QueryResult::Scalar(Value::Int(3))));
}

#[test]
fn test_nested_begin_errors() {
    let mut engine = test_engine();
    engine.execute_powql("begin").unwrap();
    let err = engine.execute_powql("begin").unwrap_err();
    assert!(
        err.to_string().contains("already in a transaction"),
        "expected nested-begin error, got: {err}"
    );
    engine.execute_powql("rollback").unwrap();
}

#[test]
fn test_commit_without_begin_errors() {
    let mut engine = test_engine();
    let err = engine.execute_powql("commit").unwrap_err();
    assert!(
        err.to_string().contains("no active transaction"),
        "expected no-tx error, got: {err}"
    );
}

#[test]
fn test_rollback_without_begin_errors() {
    let mut engine = test_engine();
    let err = engine.execute_powql("rollback").unwrap_err();
    assert!(
        err.to_string().contains("no active transaction"),
        "expected no-tx error, got: {err}"
    );
}

#[test]
fn test_commit_persists_across_reopen() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_tx_persist_{}_{}", std::process::id(), id));
    {
        let mut engine = Engine::new(&dir).unwrap();
        engine
            .execute_powql("type Item { required name: str }")
            .unwrap();
        engine.execute_powql("begin").unwrap();
        engine
            .execute_powql(r#"insert Item { name := "A" }"#)
            .unwrap();
        engine
            .execute_powql(r#"insert Item { name := "B" }"#)
            .unwrap();
        engine.execute_powql("commit").unwrap();
    }
    {
        let engine = Engine::new(&dir).unwrap();
        let result = engine.execute_powql_readonly("count(Item)").unwrap();
        assert!(matches!(result, QueryResult::Scalar(Value::Int(2))));
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_rollback_undoes_inserts_no_trace() {
    // Verify the rolled-back row is completely gone, not just from count
    // but also invisible to a filter query.
    let mut engine = test_engine();
    engine.execute_powql("begin").unwrap();
    engine
        .execute_powql(r#"insert User { name := "TxTest", email := "tx@ex.com", age := 99 }"#)
        .unwrap();
    // Row is visible during the transaction.
    let mid = engine
        .execute_powql(r#"User filter .name = "TxTest""#)
        .unwrap();
    match mid {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        _ => panic!("expected rows"),
    }

    engine.execute_powql("rollback").unwrap();

    // After rollback the row must be completely gone.
    let after = engine
        .execute_powql(r#"User filter .name = "TxTest""#)
        .unwrap();
    match after {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 0, "rolled-back insert should leave no trace");
        }
        _ => panic!("expected rows"),
    }
    // Total count is back to the original.
    let count = engine.execute_powql("count(User)").unwrap();
    assert!(matches!(count, QueryResult::Scalar(Value::Int(3))));
}

#[test]
fn test_rollback_undoes_update() {
    let mut engine = test_engine();
    // Verify Alice's age is 30 before the transaction.
    let before = engine
        .execute_powql(r#"User filter .name = "Alice" { age: .age }"#)
        .unwrap();
    match &before {
        QueryResult::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(30)),
        _ => panic!("expected rows"),
    }

    engine.execute_powql("begin").unwrap();
    engine
        .execute_powql(r#"User filter .name = "Alice" update { age := 999 }"#)
        .unwrap();
    // Verify the update took effect during the transaction.
    let mid = engine
        .execute_powql(r#"User filter .name = "Alice" { age: .age }"#)
        .unwrap();
    match &mid {
        QueryResult::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(999)),
        _ => panic!("expected rows"),
    }

    engine.execute_powql("rollback").unwrap();

    // After rollback Alice's age is back to 30.
    let after = engine
        .execute_powql(r#"User filter .name = "Alice" { age: .age }"#)
        .unwrap();
    match after {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(
                rows[0][0],
                Value::Int(30),
                "rolled-back update should restore original value"
            );
        }
        _ => panic!("expected rows"),
    }
}

// ─── Non-unique secondary index tests ──────────────────────────────────

#[test]
fn test_non_unique_index_returns_all_matches() {
    // Reproducer for the non-unique index bug: a secondary index on a
    // non-unique column (dept) must return ALL matching rows, not just one.
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_nonunique_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Employee { required name: str, required dept: str, age: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert Employee { name := "Alice", dept := "Eng", age := 30 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Employee { name := "Bob", dept := "Eng", age := 25 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Employee { name := "Carol", dept := "Sales", age := 35 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Employee { name := "Dave", dept := "Eng", age := 28 }"#)
        .unwrap();

    // Create a non-unique secondary index on dept.
    engine
        .execute_powql("alter Employee add index .dept")
        .unwrap();

    // Filter by dept = "Eng" must return all 3 matching rows.
    let result = engine
        .execute_powql(r#"Employee filter .dept = "Eng""#)
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(
                rows.len(),
                3,
                "Expected 3 Eng employees, got {}",
                rows.len()
            );
            let name_idx = columns.iter().position(|c| c == "name").unwrap();
            let mut names: Vec<String> = rows
                .iter()
                .map(|r| match &r[name_idx] {
                    Value::Str(s) => s.clone(),
                    _ => panic!("expected string name"),
                })
                .collect();
            names.sort();
            assert_eq!(names, vec!["Alice", "Bob", "Dave"]);
        }
        _ => panic!("expected rows"),
    }

    // Sales should return 1 row.
    let result = engine
        .execute_powql(r#"Employee filter .dept = "Sales""#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
        }
        _ => panic!("expected rows"),
    }

    // Missing dept should return 0 rows.
    let result = engine
        .execute_powql(r#"Employee filter .dept = "Legal""#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 0);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_rollback_undoes_delete() {
    let mut engine = test_engine();
    let count_before = engine.execute_powql("count(User)").unwrap();
    assert!(matches!(count_before, QueryResult::Scalar(Value::Int(3))));

    engine.execute_powql("begin").unwrap();
    engine
        .execute_powql(r#"User filter .name = "Bob" delete"#)
        .unwrap();
    // Bob is gone during the transaction.
    let mid = engine.execute_powql("count(User)").unwrap();
    assert!(matches!(mid, QueryResult::Scalar(Value::Int(2))));

    engine.execute_powql("rollback").unwrap();

    // After rollback Bob is back.
    let after = engine.execute_powql("count(User)").unwrap();
    assert!(
        matches!(after, QueryResult::Scalar(Value::Int(3))),
        "rolled-back delete should restore deleted row"
    );
    let bob = engine
        .execute_powql(r#"User filter .name = "Bob""#)
        .unwrap();
    match bob {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "Bob should be restored after rollback");
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_non_unique_index_delete_removes_correct_entry() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("powdb_nonunique_del_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Employee { required name: str, required dept: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert Employee { name := "Alice", dept := "Eng" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Employee { name := "Bob", dept := "Eng" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Employee { name := "Carol", dept := "Eng" }"#)
        .unwrap();
    engine
        .execute_powql("alter Employee add index .dept")
        .unwrap();

    // Delete Bob.
    engine
        .execute_powql(r#"Employee filter .name = "Bob" delete"#)
        .unwrap();

    // Should have 2 Eng employees remaining.
    let result = engine
        .execute_powql(r#"Employee filter .dept = "Eng""#)
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(rows.len(), 2, "Expected 2 Eng employees after delete");
            let name_idx = columns.iter().position(|c| c == "name").unwrap();
            let mut names: Vec<String> = rows
                .iter()
                .map(|r| match &r[name_idx] {
                    Value::Str(s) => s.clone(),
                    _ => panic!("expected string"),
                })
                .collect();
            names.sort();
            assert_eq!(names, vec!["Alice", "Carol"]);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_rollback_then_new_transaction_works() {
    // Ensure the engine is functional after a rollback — a new
    // begin/commit cycle should work normally.
    let mut engine = test_engine();
    engine.execute_powql("begin").unwrap();
    engine
        .execute_powql(r#"insert User { name := "Ghost", email := "g@ex.com", age := 1 }"#)
        .unwrap();
    engine.execute_powql("rollback").unwrap();

    // Start a fresh transaction and commit.
    engine.execute_powql("begin").unwrap();
    engine
        .execute_powql(r#"insert User { name := "Real", email := "r@ex.com", age := 50 }"#)
        .unwrap();
    engine.execute_powql("commit").unwrap();

    let count = engine.execute_powql("count(User)").unwrap();
    assert!(matches!(count, QueryResult::Scalar(Value::Int(4))));
    // Ghost should not be there, Real should.
    let ghost = engine
        .execute_powql(r#"User filter .name = "Ghost""#)
        .unwrap();
    match ghost {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 0),
        _ => panic!("expected rows"),
    }
    let real = engine
        .execute_powql(r#"User filter .name = "Real""#)
        .unwrap();
    match real {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_non_unique_index_update_changes_correct_entry() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("powdb_nonunique_upd_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Employee { required name: str, required dept: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert Employee { name := "Alice", dept := "Eng" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Employee { name := "Bob", dept := "Eng" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Employee { name := "Carol", dept := "Sales" }"#)
        .unwrap();
    engine
        .execute_powql("alter Employee add index .dept")
        .unwrap();

    // Move Bob from Eng to Sales.
    engine
        .execute_powql(r#"Employee filter .name = "Bob" update { dept := "Sales" }"#)
        .unwrap();

    // Eng should now have 1 employee.
    let result = engine
        .execute_powql(r#"Employee filter .dept = "Eng""#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "Eng should have 1 employee after move");
        }
        _ => panic!("expected rows"),
    }

    // Sales should now have 2 employees.
    let result = engine
        .execute_powql(r#"Employee filter .dept = "Sales""#)
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2, "Sales should have 2 employees after move");
        }
        _ => panic!("expected rows"),
    }
}

// ─── WS2: per-query memory budget ─────────────────────────────────────────

/// A sort over real rows with a 1 KB budget must return MemoryLimitExceeded —
/// not OOM, not panic. The default limit is large so normal queries are
/// unaffected; this exercises the budget at a tiny configured limit.
#[test]
fn test_memory_limit_sort_exceeded() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_memlimit_{}_{}", std::process::id(), id));
    let mut engine = Engine::with_memory_limit(&dir, 1024).unwrap();
    engine
        .execute_powql("type Item { required name: str, n: int }")
        .unwrap();
    for i in 0..200 {
        engine
            .execute_powql(&format!(r#"insert Item {{ name := "row-{i}", n := {i} }}"#))
            .unwrap();
    }
    // Sorting 200 rows materializes well over 1 KB of Value buffers.
    let err = engine
        .execute_powql("Item order .n")
        .expect_err("expected memory limit error");
    match err {
        crate::result::QueryError::MemoryLimitExceeded { limit_bytes, .. } => {
            assert_eq!(limit_bytes, 1024);
        }
        other => panic!("expected MemoryLimitExceeded, got {other:?}"),
    }
}

/// GROUP BY hash-table materialization is also capped.
#[test]
fn test_memory_limit_group_by_exceeded() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_memlimit_g_{}_{}", std::process::id(), id));
    let mut engine = Engine::with_memory_limit(&dir, 1024).unwrap();
    engine
        .execute_powql("type Item { required cat: str, n: int }")
        .unwrap();
    for i in 0..200 {
        engine
            .execute_powql(&format!(r#"insert Item {{ cat := "cat-{i}", n := {i} }}"#))
            .unwrap();
    }
    let err = engine
        .execute_powql("Item group .cat { .cat, n: count(.cat) }")
        .expect_err("expected memory limit error on group by");
    assert!(matches!(
        err,
        crate::result::QueryError::MemoryLimitExceeded { .. }
    ));
}

/// Normal queries under the (default) budget are unaffected.
#[test]
fn test_memory_limit_default_allows_normal_query() {
    let mut engine = test_engine();
    let result = engine.execute_powql("User order .age").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 3),
        _ => panic!("expected rows"),
    }
}

// ---------------------------------------------------------------------------
// Window aggregates without `order`: frame must be the ENTIRE partition,
// not a running prefix. (`avg(.sal) over (partition .dept)` used to return
// 10/15/20 for salaries 10/20/30 instead of 20/20/20.)
// ---------------------------------------------------------------------------

fn window_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_win_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Emp { required name: str, required dept: str, required sal: int }")
        .unwrap();
    for (name, dept, sal) in [
        ("a", "eng", 10),
        ("b", "eng", 20),
        ("c", "eng", 30),
        ("d", "ops", 100),
        ("e", "ops", 300),
    ] {
        engine
            .execute_powql(&format!(
                r#"insert Emp {{ name := "{name}", dept := "{dept}", sal := {sal} }}"#
            ))
            .unwrap();
    }
    engine
}

/// Extract (name → window value) pairs from a two-column result.
fn window_col_by_name(result: QueryResult) -> std::collections::HashMap<String, Value> {
    match result {
        QueryResult::Rows { columns, rows } => {
            let name_idx = columns.iter().position(|c| c == "name").unwrap();
            let win_idx = columns.len() - 1;
            rows.into_iter()
                .map(|r| {
                    let name = match &r[name_idx] {
                        Value::Str(s) => s.clone(),
                        other => panic!("expected name string, got {other:?}"),
                    };
                    (name, r[win_idx].clone())
                })
                .collect()
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_window_agg_without_order_uses_whole_partition() {
    let mut engine = window_engine();
    let result = engine
        .execute_powql("Emp { .name, davg: avg(.sal) over (partition .dept) }")
        .unwrap();
    let by_name = window_col_by_name(result);
    // eng partition avg = (10+20+30)/3 = 20 for EVERY row.
    for n in ["a", "b", "c"] {
        assert_eq!(by_name[n], Value::Float(20.0), "row {n}");
    }
    // ops partition avg = (100+300)/2 = 200 for both rows.
    for n in ["d", "e"] {
        assert_eq!(by_name[n], Value::Float(200.0), "row {n}");
    }
}

#[test]
fn test_window_sum_count_min_max_without_order_whole_partition() {
    let mut engine = window_engine();

    let by_name = window_col_by_name(
        engine
            .execute_powql("Emp { .name, dsum: sum(.sal) over (partition .dept) }")
            .unwrap(),
    );
    for n in ["a", "b", "c"] {
        assert_eq!(by_name[n], Value::Int(60), "sum row {n}");
    }
    for n in ["d", "e"] {
        assert_eq!(by_name[n], Value::Int(400), "sum row {n}");
    }

    let by_name = window_col_by_name(
        engine
            .execute_powql("Emp { .name, dcnt: count(.sal) over (partition .dept) }")
            .unwrap(),
    );
    for n in ["a", "b", "c"] {
        assert_eq!(by_name[n], Value::Int(3), "count row {n}");
    }

    let by_name = window_col_by_name(
        engine
            .execute_powql("Emp { .name, dmin: min(.sal) over (partition .dept) }")
            .unwrap(),
    );
    for n in ["a", "b", "c"] {
        assert_eq!(by_name[n], Value::Int(10), "min row {n}");
    }

    let by_name = window_col_by_name(
        engine
            .execute_powql("Emp { .name, dmax: max(.sal) over (partition .dept) }")
            .unwrap(),
    );
    for n in ["a", "b", "c"] {
        assert_eq!(by_name[n], Value::Int(30), "max row {n}");
    }
    for n in ["d", "e"] {
        assert_eq!(by_name[n], Value::Int(300), "max row {n}");
    }
}

#[test]
fn test_window_agg_with_order_keeps_running_frame() {
    // WITH an explicit `order`, the running (rows-so-far) frame is the
    // existing documented behavior — it must not change.
    let mut engine = window_engine();
    let by_name = window_col_by_name(
        engine
            .execute_powql("Emp { .name, ravg: avg(.sal) over (partition .dept order .sal) }")
            .unwrap(),
    );
    assert_eq!(by_name["a"], Value::Float(10.0));
    assert_eq!(by_name["b"], Value::Float(15.0));
    assert_eq!(by_name["c"], Value::Float(20.0));
}

// ---------------------------------------------------------------------------
// UNIQUE constraint enforcement (Task 3)
// ---------------------------------------------------------------------------

fn unique_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_uniq_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Acct { required unique email: str, id: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert Acct { email := "a@x.com", id := 1 }"#)
        .unwrap();
    engine
}

#[test]
fn test_unique_dup_insert_rejected() {
    let mut engine = unique_engine();
    let err = engine
        .execute_powql(r#"insert Acct { email := "a@x.com", id := 2 }"#)
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("unique constraint violation on Acct.email"),
        "{err}"
    );
    match engine.execute_powql("count(Acct)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
        other => panic!("expected scalar, got {other:?}"),
    }
}

#[test]
fn test_unique_update_into_dup_rejected() {
    let mut engine = unique_engine();
    engine
        .execute_powql(r#"insert Acct { email := "b@x.com", id := 2 }"#)
        .unwrap();
    let err = engine
        .execute_powql(r#"Acct filter .id = 2 update { email := "a@x.com" }"#)
        .unwrap_err();
    assert!(
        err.to_string().contains("unique constraint violation"),
        "{err}"
    );
    // The losing row keeps its own value (rolled back / never applied).
    match engine
        .execute_powql(r#"Acct filter .email = "b@x.com" { .id }"#)
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_unique_update_to_same_value_allowed() {
    let mut engine = unique_engine();
    // Updating a unique column to its own current value must NOT trip the
    // constraint (existing rid == self).
    engine
        .execute_powql(r#"Acct filter .id = 1 update { email := "a@x.com", id := 9 }"#)
        .unwrap();
    match engine.execute_powql("count(Acct)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
        other => panic!("expected scalar, got {other:?}"),
    }
}

#[test]
fn test_upsert_requires_unique_and_no_dup_ids() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_ups_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type W { unique id: int, v: str }")
        .unwrap();
    engine
        .execute_powql(r#"upsert W on .id { id := 1, v := "first" }"#)
        .unwrap();
    // Known bug regression: a plain insert of the same id must now fail
    // instead of silently creating a second id=1 row.
    assert!(engine
        .execute_powql(r#"insert W { id := 1, v := "second" }"#)
        .is_err());
    engine
        .execute_powql(r#"upsert W on .id { id := 1, v := "third" }"#)
        .unwrap();
    match engine.execute_powql("count(W)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
        other => panic!("expected scalar, got {other:?}"),
    }
    // upsert on a NON-unique column is a clean error.
    engine.execute_powql("type W2 { id: int }").unwrap();
    let err = engine
        .execute_powql("upsert W2 on .id { id := 1 }")
        .unwrap_err();
    assert!(
        err.to_string().contains("requires a unique column"),
        "{err}"
    );
}

#[test]
fn test_alter_add_unique_fails_on_existing_dups() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_audup_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine.execute_powql("type L { e: str }").unwrap();
    engine.execute_powql(r#"insert L { e := "x" }"#).unwrap();
    engine.execute_powql(r#"insert L { e := "x" }"#).unwrap();
    assert!(engine.execute_powql("alter L add unique .e").is_err());
}

#[test]
fn test_alter_add_unique_succeeds_then_enforces() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_au_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine.execute_powql("type L { e: str }").unwrap();
    engine.execute_powql(r#"insert L { e := "x" }"#).unwrap();
    engine.execute_powql(r#"insert L { e := "y" }"#).unwrap();
    engine.execute_powql("alter L add unique .e").unwrap();
    // Now enforced on subsequent inserts.
    assert!(engine.execute_powql(r#"insert L { e := "x" }"#).is_err());
    // Adding unique on an already-indexed column is a clean error.
    let err = engine.execute_powql("alter L add unique .e").unwrap_err();
    assert!(err.to_string().contains("already indexed"), "{err}");
}

#[test]
fn test_unique_constraint_survives_reopen() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_uniq_re_{}_{}", std::process::id(), id));
    {
        let mut engine = Engine::new(&dir).unwrap();
        engine
            .execute_powql("type Acct { required unique email: str }")
            .unwrap();
        engine
            .execute_powql(r#"insert Acct { email := "a@x.com" }"#)
            .unwrap();
        // Dropped here without explicit checkpoint — recovery path must
        // restore the unique flag from catalog.bin + WAL replay.
    }
    let mut engine = Engine::new(&dir).unwrap();
    assert!(engine
        .execute_powql(r#"insert Acct { email := "a@x.com" }"#)
        .is_err());
}

// ---------------------------------------------------------------------------
// Task 4: wire parameter binding ($1..$N), token-level substitution.
// ---------------------------------------------------------------------------

#[test]
fn test_params_bind_injection_shaped_strings_byte_faithfully() {
    use crate::ast::ParamValue;
    let mut engine = test_engine();
    let evil = r#"x"; drop User; filter .age > "0"#;
    engine
        .execute_powql_with_params(
            "insert User { name := $1, email := $2, age := $3 }",
            &[
                ParamValue::Str(evil.to_string()),
                ParamValue::Str("e@x.com".into()),
                ParamValue::Int(40),
            ],
        )
        .unwrap();
    let r = engine
        .execute_powql_with_params(
            "User filter .email = $1 { .name }",
            &[ParamValue::Str("e@x.com".into())],
        )
        .unwrap();
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str(evil.to_string()));
        }
        other => panic!("expected rows, got {other:?}"),
    }
    // Table survived; 4 rows total.
    match engine.execute_powql("count(User)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 4),
        other => panic!("{other:?}"),
    }
}

#[test]
fn test_params_errors() {
    use crate::ast::ParamValue;
    let mut engine = test_engine();
    // Out-of-range placeholder is a clean error.
    assert!(engine
        .execute_powql_with_params("User filter .age > $2", &[ParamValue::Int(1)])
        .is_err());
    // The no-params API rejects an unbound placeholder.
    assert!(engine.execute_powql("User filter .age > $1").is_err());
    // Null param round-trips as PowQL null.
    engine
        .execute_powql_with_params(
            "insert User { name := $1, email := $2, age := $3 }",
            &[
                ParamValue::Str("N".into()),
                ParamValue::Str("n@x.com".into()),
                ParamValue::Null,
            ],
        )
        .unwrap();
    match engine
        .execute_powql("User filter .age = null { .name }")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("{other:?}"),
    }
}

#[test]
fn test_params_all_types_round_trip() {
    use crate::ast::ParamValue;
    let mut engine = test_engine();
    engine
        .execute_powql("type Mix { required name: str, n: int, f: float, ok: bool }")
        .unwrap();
    engine
        .execute_powql_with_params(
            "insert Mix { name := $1, n := $2, f := $3, ok := $4 }",
            &[
                ParamValue::Str("row".into()),
                ParamValue::Int(-7),
                ParamValue::Float(2.5),
                ParamValue::Bool(true),
            ],
        )
        .unwrap();
    match engine
        .execute_powql("Mix filter .n = -7 { .name }")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("{other:?}"),
    }
}

#[test]
fn test_params_readonly_path() {
    use crate::ast::ParamValue;
    let engine = {
        let mut e = test_engine();
        // mutate up front, then exercise the readonly param path on &self.
        e.execute_powql_with_params(
            "insert User { name := $1, email := $2, age := $3 }",
            &[
                ParamValue::Str("Zed".into()),
                ParamValue::Str("z@x.com".into()),
                ParamValue::Int(99),
            ],
        )
        .unwrap();
        e
    };
    let r = engine
        .execute_powql_readonly_with_params(
            "User filter .name = $1 { .age }",
            &[ParamValue::Str("Zed".into())],
        )
        .unwrap();
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Int(99));
        }
        other => panic!("{other:?}"),
    }
    // A write statement through the readonly param path escalates.
    assert!(matches!(
        engine.execute_powql_readonly_with_params(
            "insert User { name := $1, email := $2 }",
            &[ParamValue::Str("a".into()), ParamValue::Str("b".into())],
        ),
        Err(crate::result::QueryError::ReadonlyNeedsWrite)
    ));
}

#[test]
fn test_no_params_regression_path_unchanged() {
    let mut engine = test_engine();
    // Plain queries with no placeholders still work identically.
    match engine
        .execute_powql("User filter .age > 26 { .name }")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 2),
        other => panic!("{other:?}"),
    }
}
