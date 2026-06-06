use powdb_storage::catalog::Catalog;
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "powdb_bk_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
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
fn full_backup_copies_files_and_records_lsn() {
    let src = tmp("src");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    cat.insert("T", &vec![Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();

    let dest = tmp("dest");
    let manifest = powdb_backup::full_backup(&mut cat, &dest).unwrap();

    assert!(
        manifest.source_lsn > 0,
        "snapshot must record a nonzero LSN"
    );
    assert!(manifest.files.iter().any(|f| f.name == "catalog.bin"));
    assert!(manifest.files.iter().any(|f| f.name == "T.heap"));
    assert!(
        !manifest.files.iter().any(|f| f.name == "wal.log"),
        "WAL is truncated by checkpoint; a snapshot must not include it"
    );
    assert!(dest.join("T.heap").exists());
    assert!(dest.join(powdb_backup::BackupManifest::FILE_NAME).exists());
    // Every recorded file must actually exist in dest with a matching blake3.
    for f in &manifest.files {
        let bytes = std::fs::read(dest.join(&f.name)).unwrap();
        assert_eq!(bytes.len() as u64, f.len);
        assert_eq!(blake3::hash(&bytes).to_hex().to_string(), f.blake3_hex);
    }
}

#[test]
fn restore_rebuilds_a_usable_database() {
    let src = tmp("rsrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    for i in 0..50 {
        cat.insert("T", &vec![Value::Int(i)]).unwrap();
    }
    cat.sync_wal().unwrap();

    let backup = tmp("bkp");
    powdb_backup::full_backup(&mut cat, &backup).unwrap();
    drop(cat);

    let restored = tmp("restored");
    powdb_backup::restore(&backup, &restored).unwrap();

    // Reopen the restored dir and confirm every row is present.
    let cat2 = Catalog::open(&restored).unwrap();
    assert_eq!(cat2.scan("T").unwrap().count(), 50);
}

#[test]
fn restore_rejects_a_tampered_backup() {
    let src = tmp("tsrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(schema_t()).unwrap();
    cat.insert("T", &vec![Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();

    let backup = tmp("tbkp");
    powdb_backup::full_backup(&mut cat, &backup).unwrap();

    // Corrupt a backed-up data file (blake3 in the manifest will no longer match).
    std::fs::write(backup.join("T.heap"), b"corrupted").unwrap();

    let restored = tmp("trestored");
    let err = powdb_backup::restore(&backup, &restored).unwrap_err();
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("integrity") || msg.contains("hash") || msg.contains("corrupt"),
        "tampered backup must be rejected with an integrity error, got: {err}"
    );
}
