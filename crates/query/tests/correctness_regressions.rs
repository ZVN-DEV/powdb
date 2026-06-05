//! Correctness regressions found in the v0.4.3 full production test pass.
//! These are not data-loss bugs but silent wrong-answer / stale-read bugs
//! that a production user hit, so they get permanent guards here.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_correctness_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("failed to execute `{query}`: {e}"))
}

fn scalar(engine: &mut Engine, query: &str) -> i64 {
    match exec(engine, query) {
        QueryResult::Scalar(Value::Int(n)) => n,
        QueryResult::Rows { rows, .. } if rows.len() == 1 && rows[0].len() == 1 => {
            match &rows[0][0] {
                Value::Int(n) => *n,
                other => panic!("non-int scalar {other:?}"),
            }
        }
        other => panic!("expected scalar, got {other:?}"),
    }
}

fn setup(dir: &std::path::Path) -> Engine {
    let mut engine = Engine::new(dir).unwrap();
    exec(&mut engine, "type User { required uid: int, age: int }");
    exec(&mut engine, "type Ord { required oid: int, user_id: int }");
    for i in 1..=20i64 {
        exec(
            &mut engine,
            &format!("insert User {{ uid := {i}, age := {a} }}", a = 20 + i),
        );
    }
    // Orders only for users 1..=10 (so 10 users have orders, 10 don't).
    for i in 1..=10i64 {
        exec(
            &mut engine,
            &format!("insert Ord {{ oid := {i}, user_id := {i} }}"),
        );
    }
    engine
}

/// F1: `count(T filter .x in (<subquery>))` silently returned 0 because the
/// count fast path evaluated the predicate without resolving the subquery.
#[test]
fn test_count_with_in_subquery() {
    let dir = temp_dir("count_subquery");
    std::fs::create_dir_all(&dir).unwrap();
    let mut engine = setup(&dir);

    // Users who placed an order: 10. The buggy fast path returned 0.
    assert_eq!(
        scalar(&mut engine, "count(User filter .uid in (Ord { .user_id }))"),
        10,
        "count() with an IN-subquery filter must resolve the subquery"
    );
    // Users who did NOT order: 10.
    assert_eq!(
        scalar(
            &mut engine,
            "count(User filter .uid not in (Ord { .user_id }))"
        ),
        10,
    );
    // EXISTS form.
    assert_eq!(
        scalar(
            &mut engine,
            "count(User filter exists (Ord filter .user_id = 5))"
        ),
        20,
    );
    // The projection form always worked — cross-check it agrees.
    assert_eq!(
        scalar(
            &mut engine,
            "count(User filter .uid in (Ord { .user_id }) { .uid })"
        ),
        10,
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// F3: materialized-view auto-refresh did not fire for `count(View)` or
/// `View filter ...` — those access shapes returned stale data after an
/// underlying insert, contradicting the documented "no stale reads".
#[test]
fn test_materialized_view_autorefresh() {
    let dir = temp_dir("matview_refresh");
    std::fs::create_dir_all(&dir).unwrap();
    let mut engine = setup(&dir);

    exec(
        &mut engine,
        "materialize Adults as User filter .age > 30 { .uid, .age }",
    );
    let before = scalar(&mut engine, "count(Adults)");

    // Insert a new matching row; the view must auto-refresh on next read.
    exec(&mut engine, "insert User { uid := 100, age := 99 }");

    assert_eq!(
        scalar(&mut engine, "count(Adults)"),
        before + 1,
        "count(View) must auto-refresh after an underlying insert (no stale reads)"
    );
    assert_eq!(
        scalar(&mut engine, "count(Adults filter .uid = 100 { .uid })"),
        1,
        "View filter must see the freshly-inserted row"
    );

    std::fs::remove_dir_all(&dir).ok();
}
