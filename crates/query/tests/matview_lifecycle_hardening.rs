//! Materialized-view lifecycle failure windows.
//!
//! Dropping a view touches two independently persisted structures: the backing
//! table in the catalog and the view registry. A refusal before the table is
//! dropped must leave both untouched, including after reopen.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::data_dir::VIEW_REGISTRY_FILE;
use powdb_storage::types::Value;
use powdb_storage::view::ViewRegistry;

fn exec(engine: &mut Engine, query: &str) {
    engine
        .execute_powql(query)
        .unwrap_or_else(|err| panic!("`{query}` failed: {err}"));
}

fn count(engine: &mut Engine, query: &str) -> i64 {
    match engine.execute_powql(query).unwrap() {
        QueryResult::Scalar(Value::Int(n)) => n,
        other => panic!("expected integer scalar from `{query}`, got {other:?}"),
    }
}

fn registry_has_view(dir: &std::path::Path, name: &str) -> bool {
    ViewRegistry::open(dir).unwrap().is_view(name)
}

#[test]
fn refused_drop_view_keeps_registry_and_backing_table_through_reopen() {
    let dir = tempfile::tempdir().unwrap();

    {
        let mut engine = Engine::new(dir.path()).unwrap();
        exec(&mut engine, "type Base { required unique id: int }");
        exec(&mut engine, "insert Base { id := 1 }");
        exec(&mut engine, "materialize V as Base { .id }");

        // Pin the view's backing table so Catalog::drop_table refuses before
        // unlinking anything. This is deterministic and exercises the same
        // outside-transaction ordering window as an early catalog I/O error.
        exec(&mut engine, "link Base.v -> V on id = id");

        let error = engine
            .execute_powql("drop view V")
            .expect_err("the link must pin the view backing table");
        let message = error.to_string();
        assert!(
            message.contains("cannot drop table 'V'") && message.contains("link 'v'"),
            "expected the catalog link guard, got: {message}"
        );
        assert!(
            registry_has_view(dir.path(), "V"),
            "a refused drop must not unregister the view"
        );
        assert_eq!(count(&mut engine, "count(V)"), 1);
    }

    let mut reopened = Engine::new(dir.path()).unwrap();
    assert!(
        registry_has_view(dir.path(), "V"),
        "the registry entry must survive the refused drop and reopen"
    );
    assert_eq!(
        count(&mut reopened, "count(V)"),
        1,
        "the backing table must survive the refused drop and reopen"
    );
}

#[test]
fn failed_unregister_after_backing_drop_reopens_as_actionable_inconsistency() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut engine = Engine::new(dir.path()).unwrap();
        exec(&mut engine, "type Base { required unique id: int }");
        exec(&mut engine, "insert Base { id := 1 }");
        exec(&mut engine, "materialize V as Base { .id }");
        assert!(engine.catalog().schema("V").is_some());

        let tmp_registry_path = dir.path().join(format!("{VIEW_REGISTRY_FILE}.tmp"));
        std::fs::create_dir(&tmp_registry_path)
            .expect("registry temp path fixture should be creatable as a directory");
        let error = engine
            .execute_powql("drop view V")
            .expect_err("the backing drop should succeed, then unregister persistence should fail");
        let message = error.to_string();
        assert!(
            message.contains("views.bin.tmp") || message.contains("Is a directory"),
            "expected registry temp-path failure, got: {message}"
        );
        assert!(
            registry_has_view(dir.path(), "V"),
            "failed unregister must leave the durable registry entry in place"
        );
        assert!(
            engine.catalog().schema("V").is_none(),
            "the backing table drop already succeeded before registry persistence failed"
        );
        assert!(
            tmp_registry_path.is_dir(),
            "recovery must not silently delete the operator-visible failure artifact"
        );
    }

    let error = match Engine::new(dir.path()) {
        Ok(_) => panic!("a registered view without a backing table must fail closed on reopen"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(
        message.contains("views.bin")
            && message.contains("materialized view V")
            && message.contains("no backing table")
            && message.contains("restore")
            && message.contains("recreating the view"),
        "reopen error must explain the inconsistent view registry, got: {message}"
    );
    assert!(
        dir.path()
            .join(format!("{VIEW_REGISTRY_FILE}.tmp"))
            .is_dir(),
        "open-time validation must not auto-delete or auto-repair the temp-path artifact"
    );
}
