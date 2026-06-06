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
