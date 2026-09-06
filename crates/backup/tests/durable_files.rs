//! Every durable file in a data dir has to survive backup and restore.
//!
//! `views.bin` (the materialized-view registry) and `auth.json` (the user
//! store) both live beside `catalog.bin` and both were missing from the
//! manifest, so a restored directory lost every view definition and every
//! user: `refresh V` answered "not found", reads of `V` served whatever rows
//! the backing heap happened to hold, and a server started on the restored
//! directory accepted unauthenticated connections.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::catalog::Catalog;
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};

fn tmp(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let uniq = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "powdb_durable_{tag}_{}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        uniq
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn schema_t() -> Schema {
    Schema {
        table_name: "T".into(),
        columns: vec![ColumnDef {
            name: "id".into(),
            type_id: TypeId::Int,
            required: true,
            position: 0,
        }],
    }
}

fn count(engine: &mut Engine, query: &str) -> i64 {
    match engine.execute_powql(query).unwrap() {
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

/// A plausible user store. `powdb-backup` treats it as an opaque blob, so the
/// exact shape does not matter; only that the bytes come back unchanged.
const USER_STORE_JSON: &str = r#"{"users":[{"name":"root","hash":"$argon2id$v=19$m=1,t=1,p=1$c2FsdA$aGFzaA","roles":["admin"]}]}"#;

#[test]
fn full_backup_and_restore_preserve_materialized_views() {
    let src = tmp("mv_src");
    {
        let mut e = Engine::new(&src).unwrap();
        e.execute_powql("type E { required id: int }").unwrap();
        e.execute_powql("insert E { id := 1 }").unwrap();
        e.execute_powql("materialize V as E { .id }").unwrap();
    }
    assert!(
        src.join("views.bin").exists(),
        "the view registry must be on disk before the backup"
    );

    let backup = tmp("mv_bkp");
    let mut cat = Catalog::open(&src).unwrap();
    let manifest = powdb_backup::full_backup(&mut cat, &backup).unwrap();
    drop(cat);

    assert!(
        manifest.files.iter().any(|f| f.name == "views.bin"),
        "the view registry must be in the manifest, got {:?}",
        manifest.files.iter().map(|f| &f.name).collect::<Vec<_>>()
    );
    assert!(backup.join("views.bin").exists());

    let restored = tmp("mv_restored");
    powdb_backup::restore(&backup, &restored).unwrap();
    let mut e2 = Engine::new(&restored).unwrap();
    e2.execute_powql("refresh V")
        .expect("the restored view must still be refreshable");
    assert_eq!(
        count(&mut e2, "count(V)"),
        1,
        "the view must serve its rows"
    );
}

#[test]
fn full_backup_and_restore_preserve_the_user_store() {
    let src = tmp("auth_src");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    cat.insert("T", &vec![Value::Int(1)]).unwrap();
    std::fs::write(src.join("auth.json"), USER_STORE_JSON).unwrap();

    let backup = tmp("auth_bkp");
    let manifest = powdb_backup::full_backup(&mut cat, &backup).unwrap();
    drop(cat);

    assert!(
        manifest.files.iter().any(|f| f.name == "auth.json"),
        "the user store must be in the manifest, got {:?}",
        manifest.files.iter().map(|f| &f.name).collect::<Vec<_>>()
    );

    let restored = tmp("auth_restored");
    powdb_backup::restore(&backup, &restored).unwrap();
    assert_eq!(
        std::fs::read_to_string(restored.join("auth.json")).unwrap(),
        USER_STORE_JSON,
        "a restored server must keep its users"
    );
}

#[test]
fn incremental_backup_carries_views_and_the_user_store() {
    let src = tmp("inc_src");
    {
        let mut e = Engine::new(&src).unwrap();
        e.execute_powql("type E { required id: int }").unwrap();
        e.execute_powql("insert E { id := 1 }").unwrap();
    }

    let full = tmp("inc_full");
    let base = {
        let mut cat = Catalog::open(&src).unwrap();
        powdb_backup::full_backup(&mut cat, &full).unwrap()
    };

    // Both durable files appear only after the full base was taken, so the
    // increment is the only thing that can carry them.
    {
        let mut e = Engine::new(&src).unwrap();
        e.execute_powql("insert E { id := 2 }").unwrap();
        e.execute_powql("materialize V as E { .id }").unwrap();
    }
    std::fs::write(src.join("auth.json"), USER_STORE_JSON).unwrap();

    let inc = tmp("inc_inc");
    {
        let mut cat = Catalog::open(&src).unwrap();
        powdb_backup::incremental_backup(&mut cat, &base, &inc).unwrap();
    }

    let restored = tmp("inc_restored");
    powdb_backup::restore_chain(&full, &[&inc], &restored).unwrap();

    assert_eq!(
        std::fs::read_to_string(restored.join("auth.json")).unwrap(),
        USER_STORE_JSON,
        "an increment must carry a user store created after the base"
    );
    let mut e2 = Engine::new(&restored).unwrap();
    e2.execute_powql("refresh V")
        .expect("the restored view must still be refreshable");
    assert_eq!(count(&mut e2, "count(V)"), 2);
}
