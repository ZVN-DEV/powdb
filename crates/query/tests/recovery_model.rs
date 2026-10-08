//! Bounded, replayable mutation traces checked against an independent model.
//! This tests live rollback and graceful reopen, not power-loss durability.

use std::collections::BTreeMap;

use powdb_query::ast::ParamValue;
use powdb_query::executor::Engine;
use powdb_query::result::{QueryError, QueryResult};
use powdb_storage::types::Value;
use powdb_storage::wal::WalSyncMode;

#[derive(Clone)]
struct ModelRow {
    score: i64,
    payload: String,
}

type Model = BTreeMap<i64, ModelRow>;

const INSERT: &str = "insert Item { id := $1, email := $2, score := $3, payload := $4 }";
const UPSERT: &str = "upsert Item on .id { id := $1, email := $2, score := $3, payload := $4 }";
const OPERATIONS: [&str; 8] = [
    "insert-or-upsert",
    "update",
    "delete",
    "late-insert-failure",
    "late-update-failure",
    "transaction-commit",
    "transaction-rollback",
    "transaction-abort",
];

fn expected_row(id: i64, row: &ModelRow) -> Vec<Value> {
    vec![
        Value::Int(id),
        Value::Str(format!("email-{id}")),
        Value::Int(row.score),
        Value::Str(row.payload.clone()),
    ]
}

fn exec(engine: &mut Engine, query: &str, context: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|error| panic!("{context}: {query}: {error}"))
}

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn affected(result: QueryResult) -> u64 {
    match result {
        QueryResult::Modified(count) => count,
        other => panic!("expected mutation result, got {other:?}"),
    }
}

fn write_row(engine: &mut Engine, query: &str, id: i64, row: &ModelRow, context: &str) {
    let result = engine
        .execute_powql_with_params(
            query,
            &[
                ParamValue::Int(id),
                ParamValue::Str(format!("email-{id}")),
                ParamValue::Int(row.score),
                ParamValue::Str(row.payload.clone()),
            ],
        )
        .unwrap_or_else(|error| panic!("{context}: {query}, id={id}: {error}"));
    assert_eq!(affected(result), 1, "{context}");
}

fn assert_state(engine: &mut Engine, model: &Model, context: &str) {
    let expected: Vec<_> = model
        .iter()
        .map(|(&id, row)| expected_row(id, row))
        .collect();
    assert_eq!(
        rows(exec(
            engine,
            "Item order .id { .id, .email, .score, .payload }",
            context
        )),
        expected,
        "{context}: complete rows"
    );
    match exec(engine, "count(Item)", context) {
        QueryResult::Scalar(value) => assert_eq!(
            value,
            Value::Int(model.len() as i64),
            "{context}: live row count"
        ),
        other => panic!("{context}: expected count scalar, got {other:?}"),
    }

    // Inspect the real B-trees rather than letting a query silently fall back
    // to a scan. Include absent keys used by failed inserts and transactions.
    let table = engine.catalog().get_table("Item").expect("fixture table");
    for (column, keys) in [
        ("id", (0..=11).map(Value::Int).collect::<Vec<_>>()),
        (
            "email",
            (0..=11)
                .map(|id| Value::Str(format!("email-{id}")))
                .chain([Value::Str("collision".into())])
                .collect(),
        ),
        ("score", (0..=3).map(Value::Int).collect()),
    ] {
        assert!(table.has_index(column), "{context}: missing {column} index");
        let index = match column {
            "id" => 0,
            "email" => 1,
            "score" => 2,
            _ => unreachable!(),
        };
        for key in keys {
            let mut indexed: Vec<_> = table
                .index_lookup_all(column, &key)
                .into_iter()
                .map(|rid| {
                    table
                        .get(rid)
                        .expect("index row decodes")
                        .unwrap_or_else(|| {
                            panic!("{context}: dangling {column} index entry for {key:?}")
                        })
                })
                .collect();
            indexed.sort_by(|left, right| left[0].cmp(&right[0]));
            let expected: Vec<_> = expected
                .iter()
                .filter(|row| row[index] == key)
                .cloned()
                .collect();
            assert_eq!(indexed, expected, "{context}: {column} index key {key:?}");
        }
    }

    let expected_view: Vec<_> = model
        .iter()
        .filter(|(_, row)| row.score >= 2)
        .map(|(&id, row)| vec![Value::Int(id), Value::Int(row.score)])
        .collect();
    assert_eq!(
        rows(exec(engine, "HighScore order .id { .id, .score }", context)),
        expected_view,
        "{context}: materialized view"
    );
}

fn late_insert_failure(engine: &mut Engine, payload: &str, context: &str) {
    // First row can allocate overflow pages and index entries. The second
    // conflicts with sentinel 0, so *none* of the statement may survive.
    let error = engine
        .execute_powql_with_params(
            "insert Item { id := 10, email := $1, score := 3, payload := $2 }, \
         { id := 11, email := $3, score := 2, payload := $2 }",
            &[
                ParamValue::Str("email-10".into()),
                ParamValue::Str(payload.into()),
                ParamValue::Str("email-0".into()),
            ],
        )
        .expect_err("late insert conflict");
    assert!(
        error.to_string().contains("unique constraint"),
        "{context}: {error}"
    );
}

// Explicit arithmetic makes the fixture independent of a random crate's
// version. The seed and step printed before each operation reproduce a failure.
fn next(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 32
}

fn run_trace(seed: u64, steps: usize, mode: WalSyncMode, generic: bool) {
    let dir = tempfile::tempdir().expect("fixture directory");
    let mut engine = Engine::new(dir.path()).expect("fixture engine");
    engine.set_wal_sync_mode(mode);
    engine.set_force_generic_path(generic);
    let mut model = Model::new();
    let header = format!("seed={seed} mode={mode:?} generic={generic}");
    exec(&mut engine, "type Item { required unique id: int, required unique email: str, required score: int, required payload: str }", &header);
    exec(&mut engine, "alter Item add index .score", &header);
    for id in 0..4 {
        let row = ModelRow {
            score: id,
            payload: format!("initial-{id}"),
        };
        write_row(&mut engine, INSERT, id, &row, &header);
        model.insert(id, row);
    }
    exec(
        &mut engine,
        "materialize HighScore as Item filter .score >= 2 { .id, .score }",
        &header,
    );
    let mut random = seed;
    for step in 0..steps {
        // Every trace covers all eight operations before varying their order.
        let operation = if step < OPERATIONS.len() {
            step
        } else {
            next(&mut random) as usize % OPERATIONS.len()
        };
        let id = 2 + (next(&mut random) % 6) as i64;
        let row = ModelRow {
            score: (next(&mut random) % 4) as i64,
            payload: format!(
                "seed-{seed}-step-{step}-\"\\雪{}",
                "x".repeat([0, 200, 8_000, 20_000][next(&mut random) as usize % 4])
            ),
        };
        let context = format!(
            "{header} step={step} operation={operation}:{} id={id}",
            OPERATIONS[operation]
        );
        eprintln!(
            "{context} score={} payload_bytes={}",
            row.score,
            row.payload.len()
        );
        match operation {
            0 => {
                let query = if model.contains_key(&id) {
                    UPSERT
                } else {
                    INSERT
                };
                write_row(&mut engine, query, id, &row, &context);
                model.insert(id, row);
            }
            1 => {
                let result = engine
                    .execute_powql_with_params(
                        "Item filter .id = $1 update { score := $2, payload := $3 }",
                        &[
                            ParamValue::Int(id),
                            ParamValue::Int(row.score),
                            ParamValue::Str(row.payload.clone()),
                        ],
                    )
                    .unwrap_or_else(|error| panic!("{context}: update: {error}"));
                let present = model.contains_key(&id);
                assert_eq!(affected(result), u64::from(present), "{context}");
                if present {
                    model.insert(id, row);
                }
            }
            2 => {
                let result = exec(
                    &mut engine,
                    &format!("Item filter .id = {id} delete"),
                    &context,
                );
                assert_eq!(
                    affected(result),
                    u64::from(model.remove(&id).is_some()),
                    "{context}"
                );
            }
            3 => late_insert_failure(&mut engine, &row.payload, &context),
            4 => {
                let error = engine
                    .execute_powql_with_params(
                        "Item filter .id < 2 update { email := $1, payload := $2 }",
                        &[
                            ParamValue::Str("collision".into()),
                            ParamValue::Str(row.payload.clone()),
                        ],
                    )
                    .expect_err("second sentinel conflicts with the first");
                assert!(
                    error.to_string().contains("unique constraint"),
                    "{context}: {error}"
                );
            }
            5..=7 => {
                exec(&mut engine, "begin", &context);
                write_row(&mut engine, UPSERT, id, &row, &context);
                exec(&mut engine, "Item filter .id = 3 delete", &context);
                if operation == 7 {
                    late_insert_failure(&mut engine, &row.payload, &context);
                    for query in ["Item", "commit"] {
                        assert_eq!(
                            engine
                                .execute_powql(query)
                                .expect_err("aborted transaction"),
                            QueryError::TransactionAborted,
                            "{context}"
                        );
                    }
                }
                if operation == 5 {
                    exec(&mut engine, "commit", &context);
                    model.insert(id, row);
                    model.remove(&3);
                } else {
                    exec(&mut engine, "rollback", &context);
                }
            }
            _ => unreachable!(),
        }
        assert_state(&mut engine, &model, &context);
        if step % 8 == 7 || step + 1 == steps {
            drop(engine);
            engine = Engine::new(dir.path()).expect("graceful reopen");
            // Reopen must not silently switch subsequent trace steps to Full
            // or stop exercising the forced-generic executor.
            engine.set_wal_sync_mode(mode);
            engine.set_force_generic_path(generic);
            assert_state(&mut engine, &model, &format!("{context} reopened"));
        }
    }
}

fn bounded_setting(name: &str, default: usize, min: usize, max: usize) -> usize {
    let value = match std::env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .unwrap_or_else(|_| panic!("{name} must be an integer")),
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => panic!("{name}: {error}"),
    };
    assert!(
        (min..=max).contains(&value),
        "{name} must be in {min}..={max}"
    );
    value
}

#[test]
fn mixed_mutations_match_model_in_every_wal_mode() {
    let first_seed = bounded_setting("POWDB_RECOVERY_SEED", 24_301, 0, u32::MAX as usize) as u64;
    let count = bounded_setting("POWDB_RECOVERY_SEEDS", 4, 1, 256);
    let steps = bounded_setting("POWDB_RECOVERY_STEPS", 32, 8, 512);
    assert!(
        first_seed + count as u64 - 1 <= u32::MAX as u64,
        "seed range must stay within the unsigned 32-bit range for replay"
    );
    for seed in first_seed..first_seed + count as u64 {
        for mode in [WalSyncMode::Full, WalSyncMode::Normal, WalSyncMode::Off] {
            for generic in [false, true] {
                run_trace(seed, steps, mode, generic);
            }
        }
    }
}

#[test]
fn rollback_after_spilled_insert_keeps_the_next_wal_usable() {
    for mode in [WalSyncMode::Full, WalSyncMode::Normal] {
        let dir = tempfile::tempdir().expect("fixture directory");
        let mut engine = Engine::new(dir.path()).expect("fixture engine");
        engine.set_wal_sync_mode(mode);
        exec(
            &mut engine,
            "type Entry { required unique id: int, body: str }",
            "setup",
        );
        exec(&mut engine, "begin", "first begin");
        engine
            .execute_powql_with_params(
                "insert Entry { id := 1, body := $1 }",
                &[ParamValue::Str("overflow".repeat(2_500))],
            )
            .expect("insert spilling into multiple pages");
        exec(&mut engine, "rollback", "first rollback");
        // Reopening for rollback sweeps the orphan overflow pages and writes
        // a new WAL record. Retiring the old catalog must not truncate it.
        exec(
            &mut engine,
            r#"insert Entry { id := 2, body := "committed" }"#,
            "next committed write",
        );
        exec(&mut engine, "begin", "second begin");
        exec(
            &mut engine,
            r#"insert Entry { id := 3, body := "discard" }"#,
            "second insert",
        );
        exec(&mut engine, "rollback", "second rollback");
        let expected = vec![vec![Value::Int(2), Value::Str("committed".into())]];
        assert_eq!(
            rows(exec(
                &mut engine,
                "Entry order .id { .id, .body }",
                "after rollback"
            )),
            expected
        );
        drop(engine);
        let mut engine = Engine::new(dir.path()).expect("graceful reopen");
        assert_eq!(
            rows(exec(
                &mut engine,
                "Entry order .id { .id, .body }",
                "reopened"
            )),
            expected
        );
    }
}

#[test]
fn replayed_overflow_reuse_cannot_overwrite_a_committed_payload() {
    for mode in [WalSyncMode::Full, WalSyncMode::Normal] {
        let dir = tempfile::tempdir().expect("fixture directory");
        let mut engine = Engine::new(dir.path()).expect("fixture engine");
        engine.set_wal_sync_mode(mode);
        exec(
            &mut engine,
            "type Entry { required unique id: int, body: str }",
            "setup",
        );
        let body = "committed overflow".repeat(1_000);
        exec(&mut engine, "begin", "first begin");
        engine
            .execute_powql_with_params(
                "insert Entry { id := 1, body := $1 }",
                &[ParamValue::Str(body.clone())],
            )
            .expect("first spilled insert");
        exec(&mut engine, "rollback", "sweep writes OverflowFree");
        engine
            .execute_powql_with_params(
                "insert Entry { id := 2, body := $1 }",
                &[ParamValue::Str(body.clone())],
            )
            .expect("reuse freed pages and commit");
        exec(&mut engine, "begin", "second begin");
        exec(
            &mut engine,
            r#"insert Entry { id := 3, body := "discard" }"#,
            "second insert",
        );
        exec(&mut engine, "rollback", "replay free then reuse");
        // Overflow writes are already on disk, so the replay LSN guard skips
        // their physical writes. It must still remove their pages from the
        // free list before the next allocation.
        engine
            .execute_powql_with_params(
                "insert Entry { id := 4, body := $1 }",
                &[ParamValue::Str("new overflow".repeat(1_000))],
            )
            .expect("new spilled insert");
        assert_eq!(
            rows(exec(
                &mut engine,
                "Entry filter .id = 2 { .body }",
                "preserved payload"
            )),
            vec![vec![Value::Str(body.clone())]]
        );
        drop(engine);
        let mut engine = Engine::new(dir.path()).expect("graceful reopen");
        assert_eq!(
            rows(exec(
                &mut engine,
                "Entry filter .id = 2 { .body }",
                "reopened payload"
            )),
            vec![vec![Value::Str(body)]]
        );
    }
}
