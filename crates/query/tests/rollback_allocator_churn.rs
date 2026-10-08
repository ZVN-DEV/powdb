//! Rollback regressions that exercise allocator metadata churn through the
//! public query engine.
//!
//! These tests intentionally avoid timing assertions. They lock observable
//! behavior before optimizing rollback snapshots: rolled-back heap/index/auto
//! state must be exact, and later writes must still be able to reuse storage
//! after failed transactions.

use powdb_query::executor::Engine;
use powdb_query::result::{QueryError, QueryResult};
use powdb_storage::types::Value;
use powdb_storage::wal::WalSyncMode;
use std::fs;
use std::path::Path;

fn wal_off_engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine.set_wal_sync_mode(WalSyncMode::Off);
    (dir, engine)
}

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|error| panic!("failed `{query}`: {error}"))
}

fn expect_failure(engine: &mut Engine, query: &str, needle: &str) {
    let error = engine
        .execute_powql(query)
        .expect_err(&format!("expected `{query}` to fail"));
    assert!(
        error.to_string().contains(needle),
        "expected `{query}` error to contain {needle:?}, got {error}"
    );
}

fn rows(engine: &mut Engine, query: &str) -> Vec<Vec<Value>> {
    match exec(engine, query) {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows for `{query}`, got {other:?}"),
    }
}

fn scalar_int(engine: &mut Engine, query: &str) -> i64 {
    match exec(engine, query) {
        QueryResult::Scalar(Value::Int(n)) => n,
        QueryResult::Rows { rows, .. } if rows.len() == 1 && !rows[0].is_empty() => {
            match rows[0][0] {
                Value::Int(n) => n,
                ref other => panic!("expected int for `{query}`, got {other:?}"),
            }
        }
        other => panic!("expected int scalar for `{query}`, got {other:?}"),
    }
}

fn dir_size(path: &Path) -> u64 {
    fn walk(path: &Path, total: &mut u64) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            if metadata.is_dir() {
                walk(&entry.path(), total);
            } else {
                *total += metadata.len();
            }
        }
    }

    let mut total = 0;
    walk(path, &mut total);
    total
}

fn text(prefix: &str, len: usize) -> String {
    let mut value = prefix.to_owned();
    value.push_str(&"x".repeat(len.saturating_sub(value.len())));
    value
}

fn assert_rows(engine: &mut Engine, query: &str, expected: Vec<Vec<Value>>) {
    assert_eq!(rows(engine, query), expected, "{query}");
}

#[test]
fn rollback_after_repeated_size_churn_restores_rows_indexes_and_allows_more_writes() {
    let (dir, mut engine) = wal_off_engine();
    exec(
        &mut engine,
        "type Churn { required unique id: int, unique code: str, payload: str }",
    );

    for id in 0..20 {
        exec(
            &mut engine,
            &format!(
                r#"insert Churn {{ id := {id}, code := "c{id}", payload := "{}" }}"#,
                text(&format!("seed-{id}-"), 180)
            ),
        );
    }
    let baseline = rows(&mut engine, "Churn order .id { .id, .code, .payload }");
    let size_after_seed = dir_size(dir.path());

    for cycle in 0..10 {
        exec(&mut engine, "begin");
        for id in 0..20 {
            if id % 3 == 0 {
                exec(&mut engine, &format!("Churn filter .id = {id} delete"));
                exec(
                    &mut engine,
                    &format!(
                        r#"insert Churn {{ id := {id}, code := "c{id}", payload := "{}" }}"#,
                        text(&format!("reinsert-{cycle}-{id}-"), 420 + cycle * 5)
                    ),
                );
            } else {
                exec(
                    &mut engine,
                    &format!(
                        r#"Churn filter .id = {id} update {{ payload := "{}" }}"#,
                        text(
                            &format!("update-{cycle}-{id}-"),
                            40 + ((cycle + id) % 5) * 90
                        )
                    ),
                );
            }
        }
        expect_failure(
            &mut engine,
            r#"insert Churn { id := 9999, code := "c0", payload := "dupe" }"#,
            "unique constraint",
        );
        assert_eq!(
            engine.execute_powql("Churn").unwrap_err(),
            QueryError::TransactionAborted
        );
        exec(&mut engine, "rollback");

        assert_eq!(
            rows(&mut engine, "Churn order .id { .id, .code, .payload }"),
            baseline,
            "cycle {cycle} rollback did not restore exact rows"
        );
        assert_rows(
            &mut engine,
            r#"Churn filter .code = "c0" { .id, .payload }"#,
            vec![vec![Value::Int(0), Value::Str(text("seed-0-", 180))]],
        );

        let committed_id = 100 + cycle as i64;
        exec(
            &mut engine,
            &format!(
                r#"insert Churn {{ id := {committed_id}, code := "after{cycle}", payload := "{}" }}"#,
                text("post-rollback-", 260)
            ),
        );
        assert_rows(
            &mut engine,
            &format!(r#"Churn filter .code = "after{cycle}" {{ .id }}"#),
            vec![vec![Value::Int(committed_id)]],
        );
        exec(
            &mut engine,
            &format!("Churn filter .id = {committed_id} delete"),
        );
    }

    assert_eq!(scalar_int(&mut engine, "count(Churn)"), 20);
    assert!(
        dir_size(dir.path()) <= size_after_seed + 256 * 1024,
        "repeated rolled-back small-row churn should not grow data files without bound"
    );
}

#[test]
fn overflow_rollback_cycles_preserve_committed_payloads_and_unique_indexes() {
    let (dir, mut engine) = wal_off_engine();
    exec(
        &mut engine,
        "type Big { required unique id: int, unique key: str, payload: str }",
    );
    let original_one = text("one-", 12 * 1024);
    let original_two = text("two-", 13 * 1024);
    exec(
        &mut engine,
        &format!(r#"insert Big {{ id := 1, key := "one", payload := "{original_one}" }}"#),
    );
    exec(
        &mut engine,
        &format!(r#"insert Big {{ id := 2, key := "two", payload := "{original_two}" }}"#),
    );
    let baseline = rows(&mut engine, "Big order .id { .id, .key, .payload }");
    let size_after_seed = dir_size(dir.path());

    for cycle in 0..6 {
        exec(&mut engine, "begin");
        exec(
            &mut engine,
            &format!(
                r#"Big filter .key = "one" update {{ payload := "{}" }}"#,
                text(&format!("expanded-{cycle}-"), 24 * 1024 + cycle * 1024)
            ),
        );
        exec(&mut engine, r#"Big filter .key = "two" delete"#);
        exec(
            &mut engine,
            &format!(
                r#"insert Big {{ id := {}, key := "temp{cycle}", payload := "{}" }}"#,
                10 + cycle,
                text(&format!("temp-{cycle}-"), 18 * 1024)
            ),
        );
        expect_failure(
            &mut engine,
            r#"insert Big { id := 900, key := "one", payload := "duplicate" }"#,
            "unique constraint",
        );
        exec(&mut engine, "rollback");

        assert_eq!(
            rows(&mut engine, "Big order .id { .id, .key, .payload }"),
            baseline,
            "cycle {cycle} changed committed overflow rows"
        );
        assert_rows(
            &mut engine,
            r#"Big filter .key = "one" { .id, .payload }"#,
            vec![vec![Value::Int(1), Value::Str(original_one.clone())]],
        );
        assert_rows(
            &mut engine,
            r#"Big filter .key = "two" { .id, .payload }"#,
            vec![vec![Value::Int(2), Value::Str(original_two.clone())]],
        );

        let post_id = 100 + cycle as i64;
        exec(
            &mut engine,
            &format!(
                r#"insert Big {{ id := {post_id}, key := "after{cycle}", payload := "{}" }}"#,
                text("after-", 14 * 1024)
            ),
        );
        assert_rows(
            &mut engine,
            &format!(r#"Big filter .key = "after{cycle}" {{ .id }}"#),
            vec![vec![Value::Int(post_id)]],
        );
        exec(&mut engine, &format!("Big filter .id = {post_id} delete"));
    }

    assert_eq!(
        rows(&mut engine, "Big order .id { .id, .key, .payload }"),
        baseline
    );
    assert!(
        dir_size(dir.path()) <= size_after_seed + 512 * 1024,
        "rolled-back overflow churn should reuse freed space instead of unbounded growth"
    );
}

#[test]
fn rollback_restores_auto_id_sequence_after_failed_transaction() {
    let (_dir, mut engine) = wal_off_engine();
    exec(
        &mut engine,
        "type D { unique auto id: int, required unique label: str, required v: int }",
    );
    exec(&mut engine, r#"insert D { label := "seed", v := 1 }"#);

    for cycle in 0..4 {
        exec(&mut engine, "begin");
        exec(
            &mut engine,
            &format!(r#"insert D {{ label := "rolled-{cycle}", v := {cycle} }}"#),
        );
        expect_failure(
            &mut engine,
            r#"insert D { label := "seed", v := 99 }"#,
            "unique constraint",
        );
        exec(&mut engine, "rollback");

        let assigned = scalar_int(
            &mut engine,
            &format!(r#"insert D {{ label := "committed-{cycle}", v := {cycle} }} returning"#),
        );
        assert_eq!(
            assigned,
            cycle as i64 + 2,
            "rolled-back auto id should be reused by the next committed insert"
        );
    }

    assert_rows(
        &mut engine,
        "D order .id { .id, .label, .v }",
        vec![
            vec![Value::Int(1), Value::Str("seed".into()), Value::Int(1)],
            vec![
                Value::Int(2),
                Value::Str("committed-0".into()),
                Value::Int(0),
            ],
            vec![
                Value::Int(3),
                Value::Str("committed-1".into()),
                Value::Int(1),
            ],
            vec![
                Value::Int(4),
                Value::Str("committed-2".into()),
                Value::Int(2),
            ],
            vec![
                Value::Int(5),
                Value::Str("committed-3".into()),
                Value::Int(3),
            ],
        ],
    );
}

#[test]
fn rollback_restores_multiple_touched_tables_and_preserves_untouched_table() {
    let (_dir, mut engine) = wal_off_engine();
    exec(
        &mut engine,
        "type A { required unique id: int, unique code: str, payload: str }",
    );
    exec(
        &mut engine,
        "type B { required unique id: int, unique code: str, payload: str }",
    );
    exec(
        &mut engine,
        "type C { required unique id: int, unique code: str, payload: str }",
    );
    for (table, code) in [("A", "a"), ("B", "b"), ("C", "c")] {
        exec(
            &mut engine,
            &format!(
                r#"insert {table} {{ id := 1, code := "{code}1", payload := "{}" }}"#,
                text(&format!("{table}-one-"), 512)
            ),
        );
        exec(
            &mut engine,
            &format!(
                r#"insert {table} {{ id := 2, code := "{code}2", payload := "{}" }}"#,
                text(&format!("{table}-two-"), 640)
            ),
        );
    }
    let expected_a = rows(&mut engine, "A order .id { .id, .code, .payload }");
    let expected_b = rows(&mut engine, "B order .id { .id, .code, .payload }");
    let expected_c = rows(&mut engine, "C order .id { .id, .code, .payload }");

    exec(&mut engine, "begin");
    exec(
        &mut engine,
        &format!(
            r#"A filter .id = 1 update {{ payload := "{}" }}"#,
            text("A-expanded-", 14 * 1024)
        ),
    );
    exec(&mut engine, "A filter .id = 2 delete");
    exec(
        &mut engine,
        &format!(
            r#"insert A {{ id := 3, code := "a3", payload := "{}" }}"#,
            text("A-new-", 300)
        ),
    );
    exec(&mut engine, "B filter .id = 1 delete");
    exec(
        &mut engine,
        &format!(
            r#"B filter .id = 2 update {{ payload := "{}" }}"#,
            text("B-expanded-", 15 * 1024)
        ),
    );
    expect_failure(
        &mut engine,
        r#"insert B { id := 9, code := "b2", payload := "duplicate" }"#,
        "unique constraint",
    );
    exec(&mut engine, "rollback");

    assert_eq!(
        rows(&mut engine, "A order .id { .id, .code, .payload }"),
        expected_a
    );
    assert_eq!(
        rows(&mut engine, "B order .id { .id, .code, .payload }"),
        expected_b
    );
    assert_eq!(
        rows(&mut engine, "C order .id { .id, .code, .payload }"),
        expected_c
    );
    assert_rows(
        &mut engine,
        r#"A filter .code = "a2" { .id }"#,
        vec![vec![Value::Int(2)]],
    );
    assert_rows(
        &mut engine,
        r#"B filter .code = "b1" { .id }"#,
        vec![vec![Value::Int(1)]],
    );
}
