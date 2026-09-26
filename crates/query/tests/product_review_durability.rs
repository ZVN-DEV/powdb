//! Product-review durability probes.
//!
//! These tests intentionally stay at the public `Engine` API boundary. The
//! storage crate already has strong low-level WAL/DDL coverage; this file pins
//! the product-level behavior a user would evaluate: failed DDL must not leak
//! catalog/view metadata, failed unique mutations must be statement-atomic, and
//! materialized-view refreshes inside transactions must roll back cleanly.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use powdb_storage::view::ViewRegistry;

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("failed to execute `{query}`: {e}"))
}

fn expect_err_contains(engine: &mut Engine, query: &str, needle: &str) {
    let err = match engine.execute_powql(query) {
        Ok(result) => panic!("`{query}` unexpectedly succeeded with {result:?}"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains(needle),
        "`{query}` error should contain {needle:?}, got {message:?}"
    );
}

fn count(engine: &mut Engine, query: &str) -> i64 {
    match exec(engine, query) {
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

fn projected_ints(engine: &mut Engine, query: &str) -> Vec<i64> {
    let mut out = match exec(engine, query) {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|row| match &row[0] {
                Value::Int(n) => *n,
                other => panic!("expected int projection, got {other:?}"),
            })
            .collect::<Vec<_>>(),
        other => panic!("expected rows, got {other:?}"),
    };
    out.sort_unstable();
    out
}

fn id_email_rows(engine: &mut Engine) -> Vec<(i64, String)> {
    let mut rows = match exec(engine, "User order .id { .id, .email }") {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|row| {
                let id = match &row[0] {
                    Value::Int(n) => *n,
                    other => panic!("expected int id, got {other:?}"),
                };
                let email = match &row[1] {
                    Value::Str(s) => s.clone(),
                    other => panic!("expected str email, got {other:?}"),
                };
                (id, email)
            })
            .collect::<Vec<_>>(),
        other => panic!("expected rows, got {other:?}"),
    };
    rows.sort_unstable_by_key(|(id, _)| *id);
    rows
}

fn registry_state(dir: &std::path::Path, view: &str) -> bool {
    ViewRegistry::open(dir)
        .expect("view registry should decode")
        .is_view(view)
}

fn seed_view_database(dir: &std::path::Path) {
    let mut engine = Engine::new(dir).unwrap();
    exec(
        &mut engine,
        "type Base { required unique id: int, label: str }",
    );
    exec(&mut engine, r#"insert Base { id := 1, label := "one" }"#);
    exec(&mut engine, "materialize V as Base { .id }");
    assert_eq!(count(&mut engine, "count(V)"), 1);
}

#[test]
fn ddl_inside_transaction_is_refused_without_catalog_or_view_leaks() {
    let dir = tempfile::tempdir().unwrap();
    seed_view_database(dir.path());

    let mut observed = Vec::new();
    {
        let mut engine = Engine::new(dir.path()).unwrap();
        for (label, query) in [
            (
                "after refused type, before rollback",
                "type TxCreated { required id: int }",
            ),
            (
                "after refused add column, before rollback",
                "alter Base add column extra: int",
            ),
            (
                "after refused add index, before rollback",
                "alter Base add index .label",
            ),
            (
                "after refused materialize, before rollback",
                "materialize TxView as Base { .id }",
            ),
            ("after refused drop table, before rollback", "drop Base"),
            ("after refused drop view, before rollback", "drop view V"),
        ] {
            exec(&mut engine, "begin");
            expect_err_contains(&mut engine, query, "explicit transaction");
            observed.push((
                label,
                registry_state(dir.path(), "V"),
                registry_state(dir.path(), "TxView"),
            ));
            exec(&mut engine, "rollback");
            observed.push((
                "after rollback, before graceful close",
                registry_state(dir.path(), "V"),
                registry_state(dir.path(), "TxView"),
            ));
        }
    }
    observed.push((
        "after graceful reopen",
        registry_state(dir.path(), "V"),
        registry_state(dir.path(), "TxView"),
    ));
    eprintln!("drop_view_registry_observations={observed:?}");

    let mut engine = Engine::new(dir.path()).unwrap();
    let tx_created_exists = engine.catalog().schema("TxCreated").is_some();
    let base_column_count = engine.catalog().schema("Base").unwrap().columns.len();
    let base_label_indexed = engine.catalog().has_index("Base", "label");
    let base_count = count(&mut engine, "count(Base)");
    let v_rows = projected_ints(&mut engine, "V { .id }");
    eprintln!(
        "ddl_post_reopen_catalog_snapshot={{ tx_created_exists: {tx_created_exists}, \
         base_column_count: {base_column_count}, base_label_indexed: {base_label_indexed}, \
         base_count: {base_count}, v_rows: {v_rows:?} }}"
    );

    assert!(
        observed.iter().all(|(_, v, _)| *v),
        "a refused transactional `drop view V` must not unregister V; observed {observed:?}"
    );
    assert!(
        observed.iter().all(|(_, _, txview)| !*txview),
        "a refused transactional materialize must not register TxView; observed {observed:?}"
    );
    assert!(
        !tx_created_exists,
        "refused transactional type creation leaked into the catalog"
    );
    assert_eq!(
        base_column_count, 2,
        "refused transactional ALTER ADD COLUMN changed Base"
    );
    assert!(
        !base_label_indexed,
        "refused transactional ALTER ADD INDEX changed Base"
    );
    assert_eq!(base_count, 1);
    assert_eq!(
        v_rows,
        vec![1],
        "the original materialized view must remain readable after rollback"
    );
}

#[test]
fn failed_unique_update_is_atomic_and_index_consistent_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut engine = Engine::new(dir.path()).unwrap();
        exec(
            &mut engine,
            "type User { required unique id: int, unique email: str }",
        );
        exec(
            &mut engine,
            r#"insert User { id := 1, email := "a@example.com" }"#,
        );
        exec(
            &mut engine,
            r#"insert User { id := 2, email := "b@example.com" }"#,
        );

        expect_err_contains(
            &mut engine,
            r#"User update { email := "dupe@example.com" }"#,
            "unique constraint violation",
        );
        let immediate_rows = id_email_rows(&mut engine);
        eprintln!("unique_update_rows_immediately_after_error={immediate_rows:?}");
    }

    let mut engine = Engine::new(dir.path()).unwrap();
    let reopened_rows = id_email_rows(&mut engine);
    let insert_dupe_after_reopen = engine
        .execute_powql(r#"insert User { id := 3, email := "dupe@example.com" }"#)
        .map(|result| format!("ok:{result:?}"))
        .unwrap_or_else(|err| format!("err:{err}"));
    let rows_after_insert_attempt = id_email_rows(&mut engine);
    let lookup_after_insert_attempt = projected_ints(
        &mut engine,
        r#"User filter .email = "dupe@example.com" { .id }"#,
    );
    let second_dupe_insert = engine
        .execute_powql(r#"insert User { id := 4, email := "dupe@example.com" }"#)
        .map(|result| format!("ok:{result:?}"))
        .unwrap_or_else(|err| format!("err:{err}"));
    eprintln!(
        "unique_update_persistence_snapshot={{ reopened_rows: {reopened_rows:?}, \
         insert_dupe_after_reopen: {insert_dupe_after_reopen:?}, \
         rows_after_insert_attempt: {rows_after_insert_attempt:?}, \
         lookup_after_insert_attempt: {lookup_after_insert_attempt:?}, \
         second_dupe_insert: {second_dupe_insert:?} }}"
    );

    assert_eq!(
        reopened_rows,
        vec![
            (1, "a@example.com".to_string()),
            (2, "b@example.com".to_string())
        ],
        "failed unique update must stay rolled back after reopen"
    );
    assert!(
        insert_dupe_after_reopen.starts_with("ok:"),
        "unique index must not retain a phantom key from the failed update; got {insert_dupe_after_reopen}"
    );
    assert_eq!(
        lookup_after_insert_attempt,
        vec![3],
        "unique index must not retain a phantom key from the failed update"
    );
    assert!(
        second_dupe_insert.contains("unique constraint violation"),
        "second duplicate insert should be rejected after a clean insert, got {second_dupe_insert}"
    );
}

#[test]
fn materialized_view_refresh_inside_transaction_rolls_back_with_base_rows() {
    let dir = tempfile::tempdir().unwrap();
    seed_view_database(dir.path());

    {
        let mut engine = Engine::new(dir.path()).unwrap();
        exec(&mut engine, "begin");
        exec(&mut engine, r#"insert Base { id := 2, label := "two" }"#);
        assert_eq!(
            projected_ints(&mut engine, "V { .id }"),
            vec![1, 2],
            "reading a dirty view inside the transaction should see tx-local rows"
        );
        exec(&mut engine, "rollback");
        assert_eq!(projected_ints(&mut engine, "Base { .id }"), vec![1]);
        assert_eq!(
            projected_ints(&mut engine, "V { .id }"),
            vec![1],
            "rollback must restore the materialized backing rows too"
        );
    }

    let mut engine = Engine::new(dir.path()).unwrap();
    assert_eq!(projected_ints(&mut engine, "Base { .id }"), vec![1]);
    assert_eq!(
        projected_ints(&mut engine, "V { .id }"),
        vec![1],
        "transactional view-refresh rollback must survive reopen"
    );
}
