//! Multi-row INSERT (`insert T { .. }, { .. }, { .. }`) — correctness,
//! atomicity, transaction batching, plan-cache literal substitution across
//! rows, large batches, and crash recovery.
//!
//! Multi-row insert is a single statement that writes N rows. The WAL fsync
//! happens once at statement end, so it's the durable bulk-load fast path:
//! N rows = N WAL appends + 1 fsync (vs N statements = N fsyncs).

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "powdb_multirow_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn engine(name: &str) -> Engine {
    let mut e = Engine::new(&temp_dir(name)).unwrap();
    exec(
        &mut e,
        "type Item { required sku: str, qty: int, note: str }",
    );
    e
}

fn exec(e: &mut Engine, q: &str) -> QueryResult {
    e.execute_powql(q)
        .unwrap_or_else(|err| panic!("`{q}` failed: {err}"))
}

fn count(e: &mut Engine, q: &str) -> i64 {
    match exec(e, q) {
        QueryResult::Scalar(Value::Int(n)) => n,
        QueryResult::Rows { rows, .. } if rows.len() == 1 && rows[0].len() == 1 => {
            match &rows[0][0] {
                Value::Int(n) => *n,
                other => panic!("count returned non-int {other:?}"),
            }
        }
        other => panic!("expected scalar count, got {other:?}"),
    }
}

fn affected(r: QueryResult) -> u64 {
    match r {
        QueryResult::Modified(n) => n,
        other => panic!("expected Modified, got {other:?}"),
    }
}

#[test]
fn multi_row_basic_count_and_affected() {
    let mut e = engine("basic");
    let r = exec(
        &mut e,
        r#"insert Item { sku := "a", qty := 1 }, { sku := "b", qty := 2 }, { sku := "c", qty := 3 }"#,
    );
    assert_eq!(affected(r), 3, "affected count must equal rows inserted");
    assert_eq!(count(&mut e, "count(Item)"), 3);
}

#[test]
fn single_row_still_works() {
    let mut e = engine("single");
    let r = exec(&mut e, r#"insert Item { sku := "only", qty := 9 }"#);
    assert_eq!(affected(r), 1);
    assert_eq!(count(&mut e, "count(Item)"), 1);
}

#[test]
fn rows_may_omit_optional_columns() {
    let mut e = engine("optional");
    // sku is required; qty/note optional. Each row supplies a different subset.
    exec(
        &mut e,
        r#"insert Item { sku := "a", qty := 1, note := "hi" }, { sku := "b" }, { sku := "c", note := "yo" }"#,
    );
    assert_eq!(count(&mut e, "count(Item)"), 3);
    assert_eq!(count(&mut e, r#"count(Item filter .sku = "b")"#), 1);
}

#[test]
fn values_are_read_back_correctly_per_row() {
    let mut e = engine("readback");
    exec(
        &mut e,
        r#"insert Item { sku := "x", qty := 10 }, { sku := "y", qty := 20 }, { sku := "z", qty := 30 }"#,
    );
    // Each row's values land independently (no cross-row literal bleed).
    assert_eq!(count(&mut e, "count(Item filter .qty > 15)"), 2);
    assert_eq!(count(&mut e, r#"count(Item filter .sku = "x")"#), 1);
    assert_eq!(count(&mut e, "count(Item filter .qty = 30)"), 1);
}

#[test]
fn missing_required_field_aborts_whole_statement() {
    let mut e = engine("required_abort");
    // Second row omits the required `sku`. The whole statement must fail and
    // insert NOTHING (validate-all-then-insert-all atomicity).
    let err = e
        .execute_powql(r#"insert Item { sku := "ok", qty := 1 }, { qty := 2 }"#)
        .unwrap_err();
    assert!(
        format!("{err}").contains("required"),
        "expected a required-field error, got {err}"
    );
    assert_eq!(
        count(&mut e, "count(Item)"),
        0,
        "no rows must be inserted when any row is invalid"
    );
}

#[test]
fn unknown_field_aborts_whole_statement() {
    let mut e = engine("unknown_abort");
    let err = e
        .execute_powql(r#"insert Item { sku := "ok" }, { sku := "x", nope := 5 }"#)
        .unwrap_err();
    assert!(
        format!("{err}").to_lowercase().contains("nope")
            || format!("{err}").to_lowercase().contains("column")
    );
    assert_eq!(count(&mut e, "count(Item)"), 0);
}

#[test]
fn multi_row_inside_transaction() {
    let mut e = engine("txn");
    exec(&mut e, "begin");
    exec(&mut e, r#"insert Item { sku := "a" }, { sku := "b" }"#);
    exec(
        &mut e,
        r#"insert Item { sku := "c" }, { sku := "d" }, { sku := "e" }"#,
    );
    exec(&mut e, "commit");
    assert_eq!(count(&mut e, "count(Item)"), 5);
}

#[test]
fn plan_cache_substitutes_literals_across_rows() {
    // Same canonical shape twice with different literals. The cached plan's
    // per-row literal slots must be substituted in source order — if the
    // multi-row literal walk were wrong, row 2 would get row 1's value.
    let mut e = engine("plancache");
    exec(
        &mut e,
        r#"insert Item { sku := "a", qty := 1 }, { sku := "b", qty := 2 }"#,
    );
    exec(
        &mut e,
        r#"insert Item { sku := "c", qty := 3 }, { sku := "d", qty := 4 }"#,
    );
    assert_eq!(count(&mut e, "count(Item)"), 4);
    // All four distinct qty values must be present.
    for q in [1, 2, 3, 4] {
        assert_eq!(
            count(&mut e, &format!("count(Item filter .qty = {q})")),
            1,
            "qty {q} must be present exactly once"
        );
    }
}

#[test]
fn large_batch_in_one_statement() {
    let mut e = engine("large");
    let blocks: Vec<String> = (0..1000)
        .map(|i| format!(r#"{{ sku := "s{i}", qty := {i} }}"#))
        .collect();
    let q = format!("insert Item {}", blocks.join(", "));
    let r = exec(&mut e, &q);
    assert_eq!(affected(r), 1000);
    assert_eq!(count(&mut e, "count(Item)"), 1000);
    assert_eq!(count(&mut e, "count(Item filter .qty > 499)"), 500);
}

#[test]
fn oversized_batch_is_rejected_by_memory_budget() {
    // A batch large enough to exceed a tiny per-query budget must error
    // cleanly (not OOM) and insert NOTHING — the charge happens before any
    // row is written.
    let mut e = Engine::with_memory_limit(&temp_dir("budget"), 4096).unwrap();
    exec(
        &mut e,
        "type Item { required sku: str, qty: int, note: str }",
    );
    let blocks: Vec<String> = (0..2000)
        .map(|i| {
            format!(r#"{{ sku := "sku-number-{i}", qty := {i}, note := "padding text {i}" }}"#)
        })
        .collect();
    let q = format!("insert Item {}", blocks.join(", "));
    let err = e.execute_powql(&q).unwrap_err();
    assert!(
        format!("{err}").to_lowercase().contains("memory"),
        "expected a memory-limit error, got {err}"
    );
    assert_eq!(
        count(&mut e, "count(Item)"),
        0,
        "an over-budget batch must not write any rows"
    );
}

#[test]
fn multi_row_survives_crash_recovery() {
    // The durable bulk-load path: a multi-row insert, then a hard crash
    // (mem::forget skips checkpoint), must replay all rows on reopen.
    let dir = temp_dir("crash");
    {
        let mut e = Engine::new(&dir).unwrap();
        exec(&mut e, "type Item { required sku: str, qty: int }");
        exec(
            &mut e,
            r#"insert Item { sku := "a", qty := 1 }, { sku := "b", qty := 2 }, { sku := "c", qty := 3 }, { sku := "d", qty := 4 }"#,
        );
        // Hard crash: no checkpoint, WAL holds the records.
        std::mem::forget(e);
    }
    let mut e = Engine::new(&dir).unwrap();
    assert_eq!(
        count(&mut e, "count(Item)"),
        4,
        "all rows from the multi-row insert must replay after a crash"
    );
    assert_eq!(count(&mut e, "count(Item filter .qty > 2)"), 2);
}
