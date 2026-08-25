use super::*;

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
fn test_uncommitted_transaction_wal_records_do_not_replay_after_crash() {
    // More than WAL_BATCH_SIZE row records forces the WAL to auto-flush inside
    // the explicit transaction. Without commit-boundary replay, those flushed
    // but uncommitted records used to become durable after a hard crash.
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "powdb_tx_uncommitted_replay_{}_{}",
        std::process::id(),
        id
    ));

    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Item { required id: int, required name: str }")
        .unwrap();
    engine.execute_powql("begin").unwrap();
    for i in 0..70 {
        engine
            .execute_powql(&format!(
                r#"insert Item {{ id := {i}, name := "pending-{i}" }}"#
            ))
            .unwrap();
    }
    // Simulate process death: skip Engine/Catalog Drop so no checkpoint can
    // cleanly flush pages or truncate the WAL.
    std::mem::forget(engine);

    let engine = Engine::new(&dir).unwrap();
    let count = engine.execute_powql_readonly("count(Item)").unwrap();
    assert!(
        matches!(count, QueryResult::Scalar(Value::Int(0))),
        "uncommitted transaction rows must not replay after crash"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_rollback_discards_auto_flushed_transaction_wal_records() {
    // Rollback must truncate to the BEGIN WAL boundary even if the WAL's batch
    // threshold flushed some transaction records before ROLLBACK.
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "powdb_tx_rollback_autoflush_{}_{}",
        std::process::id(),
        id
    ));

    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Item { required id: int, required name: str }")
        .unwrap();
    engine.execute_powql("begin").unwrap();
    for i in 0..70 {
        engine
            .execute_powql(&format!(
                r#"insert Item {{ id := {i}, name := "rolled-{i}" }}"#
            ))
            .unwrap();
    }
    engine.execute_powql("rollback").unwrap();

    let count = engine.execute_powql("count(Item)").unwrap();
    assert!(
        matches!(count, QueryResult::Scalar(Value::Int(0))),
        "rollback must discard rows even after WAL auto-flush"
    );
    drop(engine);

    let engine = Engine::new(&dir).unwrap();
    let count = engine.execute_powql_readonly("count(Item)").unwrap();
    assert!(
        matches!(count, QueryResult::Scalar(Value::Int(0))),
        "rolled-back rows must stay gone after reopen"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn rollback_wal_archive_failure_keeps_transaction_retryable() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "powdb_tx_rollback_archive_failure_{}_{}",
        std::process::id(),
        id
    ));

    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Item { required id: int, required name: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert Item { id := 1, name := "committed" }"#)
        .unwrap();
    engine.execute_powql("begin").unwrap();
    engine
        .execute_powql(r#"insert Item { id := 2, name := "pending" }"#)
        .unwrap();

    let err = engine
        .rollback_transaction_with_wal_archive(|_, _| Err(std::io::Error::other("archive failed")))
        .unwrap_err();
    assert!(err.to_string().contains("archive failed"));

    engine.execute_powql("rollback").unwrap();
    let count = engine.execute_powql("count(Item)").unwrap();
    assert!(
        matches!(count, QueryResult::Scalar(Value::Int(1))),
        "failed archive rollback must leave the transaction retryable, got {count:?}"
    );

    drop(engine);
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
