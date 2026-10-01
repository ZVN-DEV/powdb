use powdb_query::executor::Engine;
use powdb_storage::btree::BTree;
use powdb_storage::catalog::Catalog;
use powdb_storage::data_dir::{READERS_DIR, VIEW_REGISTRY_FILE};
use powdb_storage::dir_lock::DirLock;
use powdb_storage::page::{slot_entry_offset_checked, Page, PAGE_SIZE};
use powdb_storage::pj1::parse_json_text;
use powdb_storage::stored_json_path::{StoredJsonPathSegmentV1, StoredJsonPathV1};
use powdb_storage::types::{ColumnDef, RowId, Schema, TypeId, Value};
use std::io::{Seek, SeekFrom, Write};

fn tmp(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let uniq = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "powdb_verify_{tag}_{}_{}_{}",
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

fn exec(engine: &mut Engine, query: &str) {
    engine.execute_powql(query).unwrap();
}

fn make_indexed_db(tag: &str) -> std::path::PathBuf {
    let dir = tmp(tag);
    {
        let mut catalog = Catalog::create(&dir).unwrap();
        catalog
            .create_table(Schema {
                table_name: "User".into(),
                columns: vec![
                    ColumnDef {
                        name: "id".into(),
                        type_id: TypeId::Int,
                        required: true,
                        position: 0,
                    },
                    ColumnDef {
                        name: "name".into(),
                        type_id: TypeId::Str,
                        required: false,
                        position: 1,
                    },
                    ColumnDef {
                        name: "data".into(),
                        type_id: TypeId::Json,
                        required: false,
                        position: 2,
                    },
                ],
            })
            .unwrap();
        catalog.create_index_unique("User", "id", true).unwrap();
        catalog.create_index("User", "name").unwrap();
        catalog
            .insert(
                "User",
                &vec![
                    Value::Int(1),
                    Value::Str("Ada".into()),
                    Value::Json(
                        parse_json_text(r#"{"slug":"ada"}"#)
                            .unwrap()
                            .into_boxed_slice(),
                    ),
                ],
            )
            .unwrap();
        catalog
            .insert(
                "User",
                &vec![
                    Value::Int(2),
                    Value::Str("Grace".into()),
                    Value::Json(
                        parse_json_text(r#"{"slug":"grace"}"#)
                            .unwrap()
                            .into_boxed_slice(),
                    ),
                ],
            )
            .unwrap();
        let path = StoredJsonPathV1::new("data", vec![StoredJsonPathSegmentV1::Key("slug".into())]);
        catalog
            .create_expression_index_metadata("User", 1, path.canonical_text(), path, false)
            .unwrap();
    }
    dir
}

fn full_backup(src: &std::path::Path) -> std::path::PathBuf {
    let backup = tmp("backup");
    let mut catalog = Catalog::open(src).unwrap();
    powdb_backup::full_backup(&mut catalog, &backup).unwrap();
    backup
}

fn dir_fingerprints(dir: &std::path::Path) -> Vec<(String, u64, String)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        out.push((
            entry.file_name().to_string_lossy().into_owned(),
            bytes.len() as u64,
            blake3::hash(&bytes).to_hex().to_string(),
        ));
    }
    out.sort();
    out
}

#[test]
fn verify_database_healthy_checks_rows_indexes_links_and_expression_indexes() {
    let dir = make_indexed_db("healthy");
    {
        let mut catalog = Catalog::open(&dir).unwrap();
        catalog
            .create_link(powdb_storage::catalog::LinkDef {
                owner_type: "User".into(),
                name: "same_name".into(),
                target_type: "User".into(),
                local_key: "name".into(),
                target_key: "name".into(),
                kind: powdb_storage::catalog::LinkKind::ToOne,
            })
            .unwrap();
    }

    let before = dir_fingerprints(&dir);
    let report = powdb_backup::verify_database(&dir);
    let after = dir_fingerprints(&dir);

    assert!(report.ok, "report was not ok:\n{}", report.to_text());
    assert_eq!(before, after, "verification must preserve source bytes");
    assert!(report.checks.iter().any(|c| c.name == "links"));
    assert!(report
        .checks
        .iter()
        .any(|c| c.name == "table:User:indexes" && c.detail.contains("3 index")));
}

#[test]
fn verify_backup_rejects_manifest_damage_before_restore_writes() {
    let src = make_indexed_db("manifest_src");
    let backup = full_backup(&src);
    let dest = tmp("must_not_exist");

    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(backup.join("manifest.json")).unwrap()).unwrap();
    let files = manifest
        .get_mut("files")
        .and_then(|v| v.as_array_mut())
        .unwrap();
    files.push(files[0].clone());
    std::fs::write(
        backup.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let report = powdb_backup::verify_backup(
        &backup,
        &powdb_backup::VerifyOptions {
            restore_drill_dir: Some(dest.clone()),
            compare_source_dir: Some(src),
        },
    );

    assert!(!report.ok);
    assert!(report.errors.iter().any(|e| e.code == "manifest_duplicate"));
    assert!(
        !dest.exists(),
        "restore drill must not write after manifest validation fails"
    );
}

#[test]
fn verify_database_refuses_pending_wal() {
    let dir = tmp("pending_wal");
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type T { id: int }");
    exec(&mut engine, "insert T { id := 1 }");
    std::mem::forget(engine);
    std::fs::remove_file(dir.join("LOCK")).unwrap();

    let report = powdb_backup::verify_database(&dir);
    assert!(!report.ok);
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.code == "catalog_open_failed" && e.message.contains("WAL")),
        "expected pending WAL refusal, got:\n{}",
        report.to_text()
    );
}

#[test]
fn verify_database_missing_directory_does_not_create_path() {
    let dir = tmp("missing_dir");
    assert!(!dir.exists());

    let report = powdb_backup::verify_database(&dir);

    assert!(!report.ok);
    assert!(report
        .errors
        .iter()
        .any(|e| e.code == "directory_unavailable"));
    assert!(
        !dir.exists(),
        "verify_database must not create missing data directories"
    );
}

#[test]
fn verify_database_refuses_same_process_writer_lock_even_when_wal_clean() {
    let dir = make_indexed_db("same_pid_writer");
    drop(Catalog::open_read_only(&dir).unwrap());
    let _writer = DirLock::acquire(&dir).unwrap();

    let report = powdb_backup::verify_database(&dir);

    assert!(!report.ok);
    assert!(report
        .errors
        .iter()
        .any(|e| e.code == "live_writer" && e.message.contains("current process")));
}

#[test]
fn verify_database_rejects_corrupt_heap_crc() {
    let dir = make_indexed_db("corrupt_crc");
    let heap = dir.join("User.heap");
    let mut bytes = std::fs::read(&heap).unwrap();
    let pos = bytes.len().saturating_sub(1);
    bytes[pos] ^= 0x55;
    std::fs::write(&heap, bytes).unwrap();

    let report = powdb_backup::verify_database(&dir);
    assert!(!report.ok);
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.code == "catalog_open_failed")
            || report.errors.iter().any(|e| e.code == "heap_crc_failed"),
        "expected heap corruption failure, got:\n{}",
        report.to_text()
    );
}

#[test]
fn verify_database_detects_stale_column_index() {
    let dir = make_indexed_db("stale_index");
    let idx = dir.join("User_name.idx");
    let mut stale = BTree::create_non_unique(&idx).unwrap();
    stale.insert_non_unique(
        Value::Str("Ada".into()),
        RowId {
            page_id: 1,
            slot_index: 0,
        },
    );
    stale.save().unwrap();

    let report = powdb_backup::verify_database(&dir);
    assert!(!report.ok);
    assert!(report.errors.iter().any(|e| e.code == "index_mismatch"));
}

#[test]
fn verify_database_reports_dirty_view_as_warning_not_error() {
    let dir = tmp("dirty_view");
    {
        let mut engine = Engine::new(&dir).unwrap();
        exec(&mut engine, "type E { required id: int }");
        exec(&mut engine, "insert E { id := 1 }");
        exec(&mut engine, "materialize V as E { .id }");
        exec(&mut engine, "insert E { id := 2 }");
    }

    let report = powdb_backup::verify_database(&dir);
    assert!(
        report.ok,
        "dirty view should not be corruption:\n{}",
        report.to_text()
    );
    assert!(report.warnings.iter().any(|w| w.code == "dirty_view"));
}

#[test]
fn restore_drill_requires_fresh_dest_and_can_compare_source() {
    let src = make_indexed_db("drill_src");
    let backup = full_backup(&src);

    let nonempty = tmp("nonempty_dest");
    std::fs::create_dir_all(&nonempty).unwrap();
    std::fs::write(nonempty.join("sentinel"), b"do not overwrite").unwrap();
    let report = powdb_backup::verify_restore_drill(&backup, &nonempty, Some(&src));
    assert!(!report.ok);
    assert!(report
        .errors
        .iter()
        .any(|e| e.code == "restore_destination_not_empty"));
    assert_eq!(
        std::fs::read(nonempty.join("sentinel")).unwrap(),
        b"do not overwrite"
    );

    let fresh = tmp("fresh_dest");
    let report = powdb_backup::verify_restore_drill(&backup, &fresh, Some(&src));
    assert!(
        report.ok,
        "restore drill should pass:\n{}",
        report.to_text()
    );
    assert!(report
        .checks
        .iter()
        .any(|c| c.name.contains("source_compare:compare:User:rows")));
}

#[test]
fn restore_does_not_publish_bad_file_when_manifest_hash_fails() {
    let src = make_indexed_db("restore_bad_hash_src");
    let backup = full_backup(&src);
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(backup.join("manifest.json")).unwrap()).unwrap();
    let first_name = manifest
        .get_mut("files")
        .and_then(|v| v.as_array_mut())
        .and_then(|files| files.first())
        .and_then(|entry| entry.get("name"))
        .and_then(|name| name.as_str())
        .unwrap()
        .to_owned();
    let target = backup.join(&first_name);
    let mut bytes = std::fs::read(&target).unwrap();
    bytes[0] ^= 0x5a;
    std::fs::write(&target, bytes).unwrap();
    let backup_after_tamper = dir_fingerprints(&backup);

    let dest = tmp("restore_bad_hash_dest");
    let error = powdb_backup::restore(&backup, &dest).unwrap_err();

    assert!(
        error.to_string().contains("blake3 mismatch"),
        "expected hash failure, got {error}"
    );
    assert_eq!(
        backup_after_tamper,
        dir_fingerprints(&backup),
        "restore must not mutate backup source files"
    );
    assert!(
        !dest.join(&first_name).exists(),
        "failed restore must not publish the corrupt final destination file"
    );
    let leftovers: Vec<_> = std::fs::read_dir(&dest)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        leftovers.iter().all(|name| !name.contains(".tmp.")),
        "failed restore must clean temporary destination files, found {leftovers:?}"
    );
}

#[test]
fn verify_database_rejects_live_slot_outside_page_even_with_valid_crc() {
    let dir = make_indexed_db("bad_slot");
    let heap = dir.join("User.heap");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&heap)
        .unwrap();
    let mut raw = [0u8; PAGE_SIZE];
    file.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
    std::io::Read::read_exact(&mut file, &mut raw).unwrap();
    let entry = slot_entry_offset_checked(0).unwrap();
    raw[entry..entry + 2].copy_from_slice(&(PAGE_SIZE as u16 - 1).to_le_bytes());
    raw[entry + 2..entry + 4].copy_from_slice(&10u16.to_le_bytes());
    let mut page = Page::from_bytes(&raw).unwrap();
    page.stamp_checksum();
    file.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
    file.write_all(page.as_bytes()).unwrap();
    file.flush().unwrap();

    let report = powdb_backup::verify_database(&dir);
    assert!(!report.ok);
    assert!(
        report.errors.iter().any(|e| e.code == "heap_crc_failed"),
        "expected strict heap layout failure, got:\n{}",
        report.to_text()
    );
}

#[test]
fn verify_backup_rejects_manifest_omitting_catalog_referenced_index() {
    let src = make_indexed_db("manifest_missing_src");
    let backup = full_backup(&src);
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(backup.join("manifest.json")).unwrap()).unwrap();
    let files = manifest
        .get_mut("files")
        .and_then(|v| v.as_array_mut())
        .unwrap();
    files.retain(|entry| {
        entry
            .get("name")
            .and_then(|v| v.as_str())
            .map(|name| name != "User.heap" && name != "User_name.idx")
            .unwrap_or(true)
    });
    std::fs::write(
        backup.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let report = powdb_backup::verify_backup(&backup, &powdb_backup::VerifyOptions::default());
    assert!(!report.ok);
    assert!(report
        .errors
        .iter()
        .any(|e| e.code == "manifest_missing_required_file"));
}

#[test]
fn verify_backup_rejects_traversal_manifest_name() {
    let src = make_indexed_db("manifest_traversal_src");
    let backup = full_backup(&src);
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(backup.join("manifest.json")).unwrap()).unwrap();
    let files = manifest
        .get_mut("files")
        .and_then(|v| v.as_array_mut())
        .unwrap();
    files[0]["name"] = serde_json::Value::String("../catalog.bin".into());
    std::fs::write(
        backup.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let report = powdb_backup::verify_backup(&backup, &powdb_backup::VerifyOptions::default());
    assert!(!report.ok);
    assert!(report
        .errors
        .iter()
        .any(|e| e.code == "manifest_name_invalid"));
}

#[cfg(unix)]
#[test]
fn verify_backup_rejects_symlink_manifest_file() {
    let src = make_indexed_db("manifest_symlink_src");
    let backup = full_backup(&src);
    let real = backup.join("User_name.idx.real");
    std::fs::rename(backup.join("User_name.idx"), &real).unwrap();
    std::os::unix::fs::symlink(&real, backup.join("User_name.idx")).unwrap();

    let report = powdb_backup::verify_backup(&backup, &powdb_backup::VerifyOptions::default());
    assert!(!report.ok);
    assert!(report
        .errors
        .iter()
        .any(|e| e.code == "backup_file_type_invalid"));
}

#[cfg(unix)]
#[test]
fn restore_drill_rejects_symlink_destination_without_writing() {
    let src = make_indexed_db("symlink_dest_src");
    let backup = full_backup(&src);
    let real = tmp("symlink_dest_real");
    std::fs::create_dir_all(&real).unwrap();
    let link = tmp("symlink_dest_link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let report = powdb_backup::verify_restore_drill(&backup, &link, Some(&src));
    assert!(!report.ok);
    assert!(report
        .errors
        .iter()
        .any(|e| e.code == "restore_destination_invalid"));
    assert!(std::fs::read_dir(&real).unwrap().next().is_none());
}

#[test]
fn verify_backup_cleans_transient_reader_dir_it_created() {
    let src = make_indexed_db("backup_readers_src");
    let backup = full_backup(&src);
    assert!(!backup.join(READERS_DIR).exists());

    let report = powdb_backup::verify_backup(&backup, &powdb_backup::VerifyOptions::default());
    assert!(report.ok, "report was not ok:\n{}", report.to_text());
    assert!(
        !backup.join(READERS_DIR).exists(),
        "backup verification should clean its transient readers directory"
    );
}

#[test]
fn compare_database_dirs_hashes_bytes_by_content_not_display_placeholder() {
    let left = tmp("bytes_left");
    let right = tmp("bytes_right");
    for (dir, value) in [(&left, vec![1u8, 2, 3]), (&right, vec![9u8, 8, 7])] {
        let mut catalog = Catalog::create(dir).unwrap();
        catalog
            .create_table(Schema {
                table_name: "Blob".into(),
                columns: vec![ColumnDef {
                    name: "payload".into(),
                    type_id: TypeId::Bytes,
                    required: false,
                    position: 0,
                }],
            })
            .unwrap();
        catalog.insert("Blob", &vec![Value::Bytes(value)]).unwrap();
    }

    let report = powdb_backup::compare_database_dirs(&left, &right);
    assert!(!report.ok);
    assert!(report
        .errors
        .iter()
        .any(|e| e.code == "compare_rows_mismatch"));
}

#[test]
fn verify_database_rejects_dangling_view_dependency_metadata() {
    let dir = tmp("dangling_view");
    {
        let mut engine = Engine::new(&dir).unwrap();
        exec(&mut engine, "type E { required id: int }");
        exec(&mut engine, "insert E { id := 1 }");
        exec(&mut engine, "materialize V as E { .id }");
    }
    let path = dir.join(VIEW_REGISTRY_FILE);
    let mut bytes = std::fs::read(&path).unwrap();
    for byte in &mut bytes {
        if *byte == b'E' {
            *byte = b'Z';
        }
    }
    std::fs::write(&path, bytes).unwrap();

    let report = powdb_backup::verify_database(&dir);
    assert!(!report.ok);
    assert!(report
        .errors
        .iter()
        .any(|e| e.code == "view_metadata_invalid"));
}
