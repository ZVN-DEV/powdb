//! Cross-version compat: a v0.7.2 database predates the durable `catalog.lsn`
//! sidecar added in 0.8.0. Opening such an on-disk directory under this branch
//! must succeed — `read_durable_lsn` falls back to 0 when the sidecar is
//! absent, recovery rebuilds the high-water mark from page LSNs, and the
//! database stays readable and writable. This guards the core-engine change
//! called out in CHANGELOG 0.8.0 against regressing existing databases.

use powdb_storage::catalog::{Catalog, CATALOG_LSN_FILE};
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};

fn tmp(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let uniq = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "powdb_sidecar_compat_{tag}_{}_{}",
        std::process::id(),
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

#[test]
fn opens_pre_0_8_0_database_without_catalog_lsn_sidecar() {
    let dir = tmp("open");

    // Build a database, then simulate a v0.7.2 on-disk layout by deleting the
    // `catalog.lsn` sidecar — that file did not exist before 0.8.0.
    {
        let mut cat = Catalog::create(&dir).unwrap();
        cat.create_table(schema_t()).unwrap();
        for i in 0..25 {
            cat.insert("T", &vec![Value::Int(i)]).unwrap();
        }
        cat.sync_wal().unwrap();
    }
    let sidecar = dir.join(CATALOG_LSN_FILE);
    assert!(
        sidecar.exists(),
        "0.8.0 must write the durable catalog.lsn sidecar"
    );
    std::fs::remove_file(&sidecar).unwrap();

    // Reopen: a missing sidecar must not error, and every row must survive.
    let cat = Catalog::open(&dir).unwrap();
    assert_eq!(
        cat.scan("T").unwrap().count(),
        25,
        "a pre-sidecar (v0.7.2) database must open with all rows intact"
    );
    // durable_lsn falls back to 0, but recovery rebuilds the high-water mark
    // from stamped page LSNs, so LSN monotonicity is preserved.
    assert!(
        cat.max_lsn() > 0,
        "high-water mark must be recovered from page LSNs when the sidecar is absent"
    );
    drop(cat);

    // The database must remain writable, and the sidecar is re-created on the
    // next durable mutation.
    {
        let mut cat = Catalog::open(&dir).unwrap();
        cat.insert("T", &vec![Value::Int(999)]).unwrap();
        cat.sync_wal().unwrap();
    }
    assert!(
        dir.join(CATALOG_LSN_FILE).exists(),
        "the sidecar must be re-created on the first durable write after upgrade"
    );
    let cat = Catalog::open(&dir).unwrap();
    assert_eq!(
        cat.scan("T").unwrap().count(),
        26,
        "rows written after the upgrade must persist alongside the legacy rows"
    );

    std::fs::remove_dir_all(&dir).ok();
}
