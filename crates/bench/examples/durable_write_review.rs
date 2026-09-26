//! Small durable-write diagnostic, separate from the WAL-off comparison.
//! Run: cargo run --release -p powdb-bench --example durable_write_review
//! Sequential cases make the process-wide fsync deltas attributable to one DB.

use powdb_query::ast::Literal;
use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use powdb_storage::wal::wal_fsync_stats;
use std::time::Instant;

const ROWS: i64 = 256;

fn assert_fixture(engine: &mut Engine) {
    for (query, expected) in [
        ("count(Item)", ROWS),
        ("sum(Item { .id })", ROWS * (ROWS - 1) / 2),
    ] {
        match engine.execute_powql(query).unwrap() {
            QueryResult::Scalar(Value::Int(value)) => assert_eq!(value, expected),
            other => panic!("unexpected fixture result: {other:?}"),
        }
    }
}

fn main() {
    for mode in ["autocommit", "explicit_transaction", "multi_row_statement"] {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type Item { required unique id: int, value: int }")
            .unwrap();
        let prepared = engine
            .prepare("insert Item { id := 0, value := 0 }")
            .unwrap();
        // Text construction and preparation are outside the timed region.
        let batch = format!(
            "insert Item {}",
            (0..ROWS)
                .map(|id| format!("{{ id := {id}, value := {} }}", id * 2))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let before = wal_fsync_stats();
        let start = Instant::now();
        if mode == "multi_row_statement" {
            engine.execute_powql(&batch).unwrap();
        } else {
            if mode == "explicit_transaction" {
                engine.execute_powql("begin").unwrap();
            }
            for id in 0..ROWS {
                engine
                    .execute_prepared(&prepared, &[Literal::Int(id), Literal::Int(id * 2)])
                    .unwrap();
            }
            if mode == "explicit_transaction" {
                engine.execute_powql("commit").unwrap();
            }
        }
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        let after = wal_fsync_stats();
        assert_fixture(&mut engine);
        drop(engine);
        let mut reopened = Engine::new(dir.path()).unwrap();
        assert_fixture(&mut reopened);
        println!(
            "{}",
            serde_json::json!({
                "mode": mode,
                "rows": ROWS,
                "wal_mode": "full",
                "elapsed_ms": elapsed_ms,
                "rows_per_second": ROWS as f64 / (elapsed_ms / 1000.0),
                "wal_fsyncs": after.count - before.count,
                "fsync_failures": after.failures - before.failures,
                "graceful_reopen_verified": true,
                "power_loss_tested": false,
            })
        );
    }
}
