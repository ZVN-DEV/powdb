//! Statement boundaries must agree across APIs and WAL modes, including the
//! benchmark-only Off mode's live-process semantics.

use powdb_query::ast::{Literal, ParamValue};
use powdb_query::executor::Engine;
use powdb_query::result::{QueryError, QueryResult};
use powdb_storage::types::Value;
use powdb_storage::wal::WalSyncMode;

const MODES: [WalSyncMode; 3] = [WalSyncMode::Full, WalSyncMode::Normal, WalSyncMode::Off];

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
}

fn seeded(mode: WalSyncMode) -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine.set_wal_sync_mode(mode);
    exec(
        &mut engine,
        "type User { required unique id: int, unique email: str, value: int }",
    );
    exec(
        &mut engine,
        r#"insert User { id := 1, email := "a", value := 10 }"#,
    );
    exec(
        &mut engine,
        r#"insert User { id := 2, email := "b", value := 20 }"#,
    );
    (dir, engine)
}

fn rows(engine: &mut Engine) -> Vec<Vec<Value>> {
    match exec(engine, "User order .id { .id, .email, .value }") {
        QueryResult::Rows { rows, .. } => rows,
        result => panic!("expected rows, got {result:?}"),
    }
}

#[test]
fn failed_unique_update_is_atomic_across_entry_points_and_wal_modes() {
    for mode in MODES {
        for surface in [
            "powql",
            "sql",
            "params",
            "prepared",
            "prepared_take",
            "plan",
        ] {
            let (dir, mut engine) = seeded(mode);
            // Capture before-images from live state, not from a forced
            // checkpoint. Off has earlier successful writes with no WAL.
            let expected = rows(&mut engine);
            exec(&mut engine, "User filter .id = 1 update { value := 11 }");
            let mut expected = expected;
            expected[0][2] = Value::Int(11);
            let query = r#"User update { email := "duplicate" }"#;
            let result = match surface {
                "powql" => engine.execute_powql(query),
                "sql" => engine.execute_sql("UPDATE User SET email = 'duplicate'"),
                "params" => engine.execute_powql_with_params(
                    "User update { email := $1 }",
                    &[ParamValue::Str("duplicate".into())],
                ),
                "prepared" | "prepared_take" => {
                    let prep = engine.prepare(query).unwrap();
                    let mut literals = [Literal::String("duplicate".into())];
                    if surface == "prepared" {
                        engine.execute_prepared(&prep, &literals)
                    } else {
                        engine.execute_prepared_take(&prep, &mut literals)
                    }
                }
                "plan" => engine.execute_plan(&powdb_query::planner::plan(query).unwrap()),
                _ => unreachable!(),
            };
            let error = result.expect_err("second row must violate uniqueness");
            assert!(
                error.to_string().contains("unique constraint"),
                "{surface}: {error}"
            );
            assert_eq!(rows(&mut engine), expected, "{surface}, {mode:?}");
            // A later successful statement must not commit the failed prefix.
            exec(
                &mut engine,
                r#"insert User { id := 3, email := "c", value := 30 }"#,
            );
            expected.push(vec![Value::Int(3), Value::Str("c".into()), Value::Int(30)]);
            assert_eq!(rows(&mut engine), expected);
            drop(engine);
            let mut reopened = Engine::new(dir.path()).unwrap();
            assert_eq!(rows(&mut reopened), expected, "reopen: {surface}, {mode:?}");
            let result = exec(&mut reopened, r#"User filter .email = "duplicate" { .id }"#);
            assert_eq!(result.row_count(), 0, "index retained a failed prefix");
        }
    }
}

#[test]
fn explicit_transaction_errors_require_rollback_and_preserve_prior_commits() {
    for mode in MODES {
        for failure in ["unique", "parse", "bind", "readonly", "readonly_sql"] {
            let (_dir, mut engine) = seeded(mode);
            let expected = rows(&mut engine);
            exec(&mut engine, "begin");
            exec(
                &mut engine,
                r#"insert User { id := 3, email := "c", value := 30 }"#,
            );
            let result = match failure {
                "unique" => engine.execute_powql(r#"User update { email := "same" }"#),
                "parse" => engine.execute_powql("insert User {"),
                "bind" => engine.execute_powql_with_params("User filter .id = $1", &[]),
                "readonly" => engine.execute_powql_readonly("User { .missing }"),
                "readonly_sql" => engine.execute_sql_readonly("SELECT missing FROM User"),
                _ => unreachable!(),
            };
            assert!(result.is_err(), "{failure} must fail");
            assert_eq!(
                engine.execute_powql("User").unwrap_err(),
                QueryError::TransactionAborted
            );
            assert_eq!(
                engine.execute_sql("COMMIT").unwrap_err(),
                QueryError::TransactionAborted
            );
            assert_eq!(
                engine.execute_powql_readonly("User").unwrap_err(),
                QueryError::TransactionAborted
            );
            exec(&mut engine, "rollback");
            assert_eq!(rows(&mut engine), expected, "{failure}, {mode:?}");
            exec(&mut engine, "begin");
            exec(&mut engine, "User filter .id = 1 update { value := 12 }");
            exec(&mut engine, "commit");
            assert_eq!(rows(&mut engine)[0][2], Value::Int(12));
        }
    }
}

#[test]
fn arithmetic_error_after_a_written_prefix_is_rolled_back() {
    for mode in MODES {
        let (_dir, mut engine) = seeded(mode);
        let expected = rows(&mut engine);
        assert!(engine
            .execute_powql("User update { value := 100 / (2 - .id) }")
            .is_err());
        assert_eq!(rows(&mut engine), expected, "{mode:?}");
    }
}

#[test]
fn prepared_insert_failure_does_not_consume_input_or_abandon_prior_off_writes() {
    for mode in MODES {
        let (_dir, mut engine) = seeded(mode);
        let expected = rows(&mut engine);
        let prep = engine
            .prepare(r#"insert User { id := 3, email := "a", value := 30 }"#)
            .unwrap();
        let mut values = [
            Literal::Int(3),
            Literal::String("a".into()),
            Literal::Int(30),
        ];
        assert!(engine.execute_prepared_take(&prep, &mut values).is_err());
        assert!(matches!(&values[1], Literal::String(value) if value == "a"));
        assert_eq!(rows(&mut engine), expected);
    }
}

#[test]
fn ordinary_prepared_and_direct_plan_writes_have_one_durability_boundary() {
    let (_dir, mut engine) = seeded(WalSyncMode::Full);
    let before = engine.wal_fsync_count();
    exec(&mut engine, "User filter .id = 1 update { value := 11 }");
    assert_eq!(engine.wal_fsync_count() - before, 1);
    let prep = engine
        .prepare("User filter .id = 1 update { value := 12 }")
        .unwrap();
    let before = engine.wal_fsync_count();
    engine
        .execute_prepared(&prep, &[Literal::Int(1), Literal::Int(12)])
        .unwrap();
    assert_eq!(engine.wal_fsync_count() - before, 1);
    let plan = powdb_query::planner::plan("User filter .id = 1 update { value := 13 }").unwrap();
    let before = engine.wal_fsync_count();
    engine.execute_plan(&plan).unwrap();
    assert_eq!(engine.wal_fsync_count() - before, 1);
}

#[test]
fn a_failed_view_refresh_never_serves_its_partially_rebuilt_cache() {
    for mode in MODES {
        let (dir, mut engine) = seeded(mode);
        exec(&mut engine, "materialize V as User { .id, .value }");
        exec(&mut engine, "alter V add unique .value");
        exec(&mut engine, "User update { value := 10 }");
        for query in ["V", "V filter .value = 10 { .id }"] {
            let error = engine.execute_powql(query).unwrap_err();
            assert!(error.to_string().contains("unique constraint"), "{error}");
        }
        drop(engine);
        let mut engine = Engine::new(dir.path()).unwrap();
        assert!(engine.execute_powql("V").is_err());
        exec(&mut engine, "User filter .id = 2 update { value := 20 }");
        let result = exec(&mut engine, "V order .id { .id, .value }");
        match result {
            QueryResult::Rows { rows, .. } => assert_eq!(
                rows,
                vec![
                    vec![Value::Int(1), Value::Int(10)],
                    vec![Value::Int(2), Value::Int(20)],
                ]
            ),
            result => panic!("unexpected repaired view: {result:?}"),
        }
    }
}

#[test]
fn pre_cancelled_prepared_writes_preserve_state_and_input() {
    use powdb_query::cancel::{install, CancelReason, ExecCancel};
    use std::sync::Arc;

    for mode in MODES {
        let (_dir, mut engine) = seeded(mode);
        let expected = rows(&mut engine);
        let prep = engine
            .prepare(r#"insert User { id := 3, email := "c", value := 30 }"#)
            .unwrap();
        let mut values = [
            Literal::Int(3),
            Literal::String("c".into()),
            Literal::Int(30),
        ];
        let cancel = Arc::new(ExecCancel::new());
        cancel.cancel(CancelReason::Disconnect);
        {
            let _guard = install(cancel);
            assert_eq!(
                engine
                    .execute_prepared_take(&prep, &mut values)
                    .unwrap_err(),
                QueryError::Cancelled
            );
            assert!(matches!(&values[1], Literal::String(value) if value == "c"));
        }
        assert_eq!(rows(&mut engine), expected);
    }
}

#[cfg(feature = "testing")]
#[test]
fn prepared_take_generic_and_fast_paths_share_commit_and_rollback_semantics() {
    for mode in MODES {
        let mut outcomes = Vec::new();
        for generic in [false, true] {
            let (_dir, mut engine) = seeded(mode);
            engine.set_force_generic_path(generic);
            let prep = engine
                .prepare(r#"insert User { id := 3, email := "c", value := 30 }"#)
                .unwrap();
            let mut literals = [
                Literal::Int(3),
                Literal::String("c".into()),
                Literal::Int(30),
            ];
            engine.execute_prepared_take(&prep, &mut literals).unwrap();
            assert!(matches!(&literals[1], Literal::String(value) if value.is_empty()));
            let expected = rows(&mut engine);
            literals[0] = Literal::Int(4);
            literals[1] = Literal::String("a".into());
            assert!(engine.execute_prepared_take(&prep, &mut literals).is_err());
            assert!(matches!(&literals[1], Literal::String(value) if value == "a"));
            assert_eq!(rows(&mut engine), expected);
            if generic {
                assert!(engine
                    .forced_generic_sites()
                    .contains(&"prepared-insert-take"));
            }
            outcomes.push(expected);
        }
        assert_eq!(outcomes[0], outcomes[1]);
    }
}

#[test]
fn failed_view_registry_reload_after_rollback_poisons_the_handle() {
    let (dir, mut engine) = seeded(WalSyncMode::Full);
    let expected = rows(&mut engine);
    // The live registry is empty. Damaging its on-disk file is observed only
    // when rollback attempts to reload the registry after restoring rows.
    std::fs::write(dir.path().join("views.bin"), b"invalid registry").unwrap();
    assert_eq!(
        engine
            .execute_powql(r#"User update { email := "same" }"#)
            .unwrap_err(),
        QueryError::EnginePoisoned
    );
    assert_eq!(
        engine.execute_powql("User").unwrap_err(),
        QueryError::EnginePoisoned
    );
    drop(engine);
    // Restore the original absence of the registry in this disposable fixture.
    std::fs::remove_file(dir.path().join("views.bin")).unwrap();
    let mut reopened = Engine::new(dir.path()).unwrap();
    assert_eq!(rows(&mut reopened), expected);
}
