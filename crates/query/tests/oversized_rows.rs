//! Large-row behavior tests.
//!
//! Before v0.11 an encoded row larger than one 4KB slotted page was a clean
//! `row too large` query error (the remote-DoS fix for `panic = "abort"`).
//! v0.11 (P-2, overflow pages) changes that: values that no longer fit inline
//! spill to out-of-line overflow chains and the insert/update SUCCEEDS. The
//! DoS boundary moves to `MAX_VALUE_SIZE` (64MB), above which a value is still
//! a clean typed error, never a panic. These tests pin the new behavior.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;

fn temp_engine(name: &str) -> (Engine, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "powdb_oversized_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(&dir).unwrap();
    (engine, dir)
}

fn big_string(n: usize) -> String {
    "x".repeat(n)
}

#[test]
fn large_value_spills_and_is_readable() {
    let (mut engine, dir) = temp_engine("insert");
    engine
        .execute_powql("type T { required id: int, b: str }")
        .unwrap();

    // A 5000-byte string no longer fits inline — it now spills out of line and
    // the insert succeeds (P-2), instead of the old "row too large" error.
    let q = format!(r#"insert T {{ id := 1, b := "{}" }}"#, big_string(5000));
    engine
        .execute_powql(&q)
        .expect("large value must spill and insert cleanly");

    // The spilled value reads back at full length.
    let res = engine.execute_powql("T filter .id = 1").unwrap();
    match res {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            let joined = format!("{:?}", rows[0]);
            assert!(
                joined.matches('x').count() >= 5000,
                "spilled value must read back at full length"
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }

    // Engine keeps working.
    engine
        .execute_powql(r#"insert T { id := 2, b := "small" }"#)
        .unwrap();
    let res = engine.execute_powql("count(T)").unwrap();
    match res {
        QueryResult::Scalar(v) => assert_eq!(format!("{v:?}"), "Int(2)"),
        other => panic!("expected scalar count, got {other:?}"),
    }
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn very_large_literal_is_clean_error_and_engine_survives() {
    // The storage-level 64MB `MAX_VALUE_SIZE` cap is covered by the storage
    // crate's overflow tests. At the query layer the relevant DoS boundary is
    // the lexer's 16MB string-literal cap: an over-cap literal is a clean
    // error, never a panic, and the engine stays usable.
    let (mut engine, dir) = temp_engine("cap");
    engine
        .execute_powql("type T { required id: int, b: str }")
        .unwrap();

    let q = format!(
        r#"insert T {{ id := 1, b := "{}" }}"#,
        big_string(20 * 1024 * 1024)
    );
    let err = engine
        .execute_powql(&q)
        .expect_err("a >16MB string literal must fail");
    assert!(
        err.to_string().contains("exceeds maximum size") || err.to_string().contains("too large"),
        "unexpected error: {err}"
    );

    // Engine remains fully usable.
    engine
        .execute_powql(r#"insert T { id := 2, b := "small" }"#)
        .unwrap();
    let res = engine.execute_powql("count(T)").unwrap();
    match res {
        QueryResult::Scalar(v) => assert_eq!(format!("{v:?}"), "Int(1)"),
        other => panic!("expected scalar count, got {other:?}"),
    }
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn update_to_large_value_spills_and_reads_back() {
    let (mut engine, dir) = temp_engine("update");
    engine
        .execute_powql("type T { required id: int, b: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert T { id := 1, b := "original" }"#)
        .unwrap();

    // Updating to a value that no longer fits inline spills (inline->spilled
    // transition) and succeeds.
    let q = format!(
        r#"T filter .id = 1 update {{ b := "{}" }}"#,
        big_string(5000)
    );
    engine
        .execute_powql(&q)
        .expect("update to large value must spill cleanly");

    let res = engine.execute_powql("T filter .id = 1").unwrap();
    match res {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            let joined = format!("{:?}", rows[0]);
            assert!(
                joined.matches('x').count() >= 5000,
                "updated spilled value must read back at full length"
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn compiled_predicate_scan_over_mixed_v1_v2_rows() {
    // A table holding both inline (v1) and spilled (v2) rows: a compiled
    // predicate scan must return correct results across both. The v2 rows are
    // reassembled at the scan boundary before the predicate/projection see
    // them, so the filter matches on the true value.
    let (mut engine, dir) = temp_engine("mixed");
    engine
        .execute_powql("type T { required id: int, tag: str, b: str }")
        .unwrap();
    // Interleave small (inline) and big (spilled) rows, all tagged "keep" for
    // odd ids and "drop" for even ids.
    for id in 1..=6 {
        let tag = if id % 2 == 1 { "keep" } else { "drop" };
        let body = if id % 2 == 1 {
            "small".to_string()
        } else {
            big_string(20_000) // even rows spill
        };
        engine
            .execute_powql(&format!(
                r#"insert T {{ id := {id}, tag := "{tag}", b := "{body}" }}"#
            ))
            .unwrap();
    }

    // Compiled predicate over a fixed column spanning v1 and v2 rows.
    let res = engine.execute_powql("T filter .id > 2 { .id }").unwrap();
    match res {
        QueryResult::Rows { rows, .. } => {
            // ids 3,4,5,6 — includes both inline (3,5) and spilled (4,6) rows.
            assert_eq!(rows.len(), 4, "filter must match across v1 and v2 rows");
        }
        other => panic!("expected rows, got {other:?}"),
    }

    // Predicate on an inline string column, matching rows of both encodings.
    let res = engine
        .execute_powql(r#"T filter .tag = "drop" { .id }"#)
        .unwrap();
    match res {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(
                rows.len(),
                3,
                "the three even (spilled) rows are tagged drop"
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn spilled_value_survives_wal_replay() {
    // A committed spilled value must survive a reopen: its overflow chain is
    // reconstructed from the WAL OverflowWrite records + the Insert stub row.
    let (mut engine, dir) = temp_engine("replay");
    engine
        .execute_powql("type T { required id: int, b: str }")
        .unwrap();
    let q = format!(r#"insert T {{ id := 1, b := "{}" }}"#, big_string(50_000));
    engine.execute_powql(&q).unwrap();
    // A follow-up statement group-commits the buffered WAL bytes.
    engine.execute_powql("count(T)").unwrap();
    drop(engine);

    let mut engine = Engine::new(&dir).expect("reopen after spilled insert");
    let res = engine.execute_powql("T filter .id = 1").unwrap();
    match res {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            let joined = format!("{:?}", rows[0]);
            assert!(
                joined.matches('x').count() >= 50_000,
                "spilled value must survive replay at full length"
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}
