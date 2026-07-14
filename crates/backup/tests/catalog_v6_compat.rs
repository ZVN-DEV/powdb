use powdb_backup::{ChangedFile, IncrementManifest};
use powdb_storage::catalog::{Catalog, CATALOG_VERSION, LEGACY_CATALOG_VERSION};
use powdb_storage::page::PAGE_SIZE;
use powdb_storage::stored_json_path::{StoredJsonPathSegmentV1, StoredJsonPathV1};
use powdb_storage::types::{ColumnDef, RowId, Schema, TypeId, Value};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "powdb_backup_catalog_v6_{label}_{}_{}",
            std::process::id(),
            nonce
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn document_schema() -> Schema {
    Schema {
        table_name: "Document".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            },
            ColumnDef {
                name: "data".into(),
                type_id: TypeId::Json,
                required: false,
                position: 1,
            },
        ],
    }
}

fn status_path() -> StoredJsonPathV1 {
    StoredJsonPathV1::new("data", vec![StoredJsonPathSegmentV1::Key("status".into())])
}

fn page_aligned_index_key() -> String {
    // A one-leaf BIDX v2 file has 39 bytes of fixed framing around one string
    // key and RID, so this produces an exactly PAGE_SIZE-byte valid file.
    "x".repeat(PAGE_SIZE - 39)
}

fn activate_expression_index(catalog: &mut Catalog) -> u64 {
    let path = status_path();
    let index_id = catalog
        .create_expression_index_metadata("Document", 1, path.canonical_text(), path, false)
        .unwrap();
    catalog
        .expression_index_btree_mut("Document", index_id)
        .unwrap()
        .insert_duplicate(
            Value::Str(page_aligned_index_key()),
            RowId {
                page_id: 7,
                slot_index: 3,
            },
        );
    index_id
}

#[test]
fn untouched_and_reopened_catalogs_remain_v5_in_full_manifests_and_restore() {
    let source = TempDir::new("v5_source");
    let backup = TempDir::new("v5_backup");
    let restored = TempDir::new("v5_restored");
    let mut catalog = Catalog::create(source.path()).unwrap();
    catalog.create_table(document_schema()).unwrap();
    assert_eq!(catalog.active_catalog_version(), LEGACY_CATALOG_VERSION);

    let manifest = powdb_backup::full_backup(&mut catalog, backup.path()).unwrap();
    assert_eq!(manifest.catalog_version, LEGACY_CATALOG_VERSION);
    drop(catalog);

    let mut reopened = Catalog::open(source.path()).unwrap();
    assert_eq!(reopened.active_catalog_version(), LEGACY_CATALOG_VERSION);
    let second_backup = TempDir::new("v5_reopened_backup");
    let reopened_manifest = powdb_backup::full_backup(&mut reopened, second_backup.path()).unwrap();
    assert_eq!(reopened_manifest.catalog_version, LEGACY_CATALOG_VERSION);

    powdb_backup::restore(backup.path(), restored.path()).unwrap();
    let restored_catalog = Catalog::open(restored.path()).unwrap();
    assert_eq!(
        restored_catalog.active_catalog_version(),
        LEGACY_CATALOG_VERSION
    );
}

#[test]
fn v6_full_restore_preserves_id_named_expression_index_as_opaque_file() {
    let source = TempDir::new("v6_source");
    let backup = TempDir::new("v6_backup");
    let restored = TempDir::new("v6_restored");
    let mut catalog = Catalog::create(source.path()).unwrap();
    catalog.create_table(document_schema()).unwrap();
    let index_id = activate_expression_index(&mut catalog);
    let index_name = format!("Document_idx_{index_id}.idx");
    catalog.checkpoint().unwrap();
    let index_len = std::fs::metadata(source.path().join(&index_name))
        .unwrap()
        .len() as usize;
    assert_eq!(
        index_len % PAGE_SIZE,
        0,
        "the regression fixture must cover a page-aligned BIDX file"
    );

    let manifest = powdb_backup::full_backup(&mut catalog, backup.path()).unwrap();
    assert_eq!(manifest.catalog_version, CATALOG_VERSION);
    assert!(manifest.files.iter().any(|file| file.name == index_name));
    drop(catalog);

    powdb_backup::restore(backup.path(), restored.path()).unwrap();
    let restored_catalog = Catalog::open(restored.path()).unwrap();
    assert_eq!(restored_catalog.active_catalog_version(), CATALOG_VERSION);
    let metadata = restored_catalog
        .expression_index_metadata("Document")
        .unwrap();
    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0].index_id, index_id);
    assert_eq!(
        restored_catalog
            .expression_index_btree("Document", index_id)
            .unwrap()
            .lookup_all(&Value::Str(page_aligned_index_key())),
        vec![RowId {
            page_id: 7,
            slot_index: 3
        }]
    );
}

#[test]
fn v5_base_to_v6_increment_copies_aligned_bidx_whole_and_restores_activation() {
    let source = TempDir::new("inc_source");
    let full = TempDir::new("inc_full");
    let increment = TempDir::new("inc_delta");
    let restored = TempDir::new("inc_restored");
    let mut catalog = Catalog::create(source.path()).unwrap();
    catalog.create_table(document_schema()).unwrap();
    let base = powdb_backup::full_backup(&mut catalog, full.path()).unwrap();
    assert_eq!(base.catalog_version, LEGACY_CATALOG_VERSION);

    let index_id = activate_expression_index(&mut catalog);
    let index_name = format!("Document_idx_{index_id}.idx");
    catalog.checkpoint().unwrap();
    assert_eq!(
        std::fs::metadata(source.path().join(&index_name))
            .unwrap()
            .len() as usize
            % PAGE_SIZE,
        0
    );

    let inc = powdb_backup::incremental_backup(&mut catalog, &base, increment.path()).unwrap();
    assert_eq!(inc.catalog_version, CATALOG_VERSION);
    assert!(inc.changed.iter().any(|changed| matches!(
        changed,
        ChangedFile::Whole { name, .. } if name == &index_name
    )));
    assert!(!inc.changed.iter().any(|changed| matches!(
        changed,
        ChangedFile::Pages { name, .. } if name.ends_with(".idx")
    )));
    drop(catalog);

    powdb_backup::restore_chain(full.path(), &[increment.path()], restored.path()).unwrap();
    let restored_catalog = Catalog::open(restored.path()).unwrap();
    assert_eq!(restored_catalog.active_catalog_version(), CATALOG_VERSION);
    assert_eq!(
        restored_catalog
            .expression_index_metadata("Document")
            .unwrap()[0]
            .index_id,
        index_id
    );
}

#[test]
fn orphan_expression_index_ids_are_not_snapshot_metadata_or_files() {
    let source = TempDir::new("orphan_source");
    let backup = TempDir::new("orphan_backup");
    let mut catalog = Catalog::create(source.path()).unwrap();
    catalog.create_table(document_schema()).unwrap();
    let active_id = activate_expression_index(&mut catalog);
    catalog.checkpoint().unwrap();

    let active_name = format!("Document_idx_{active_id}.idx");
    let orphan_name = "Document_idx_999.idx";
    std::fs::copy(
        source.path().join(&active_name),
        source.path().join(orphan_name),
    )
    .unwrap();

    let manifest = powdb_backup::full_backup(&mut catalog, backup.path()).unwrap();
    assert!(manifest.files.iter().any(|file| file.name == active_name));
    assert!(!manifest.files.iter().any(|file| file.name == orphan_name));
    assert!(!backup.path().join(orphan_name).exists());
    assert_eq!(
        catalog
            .expression_index_metadata("Document")
            .unwrap()
            .iter()
            .map(|metadata| metadata.index_id)
            .collect::<Vec<_>>(),
        vec![active_id]
    );
}

#[test]
fn dropped_and_recreated_ids_from_a_base_are_not_reactivated_by_chain_restore() {
    let source = TempDir::new("recreate_source");
    let full = TempDir::new("recreate_full");
    let increment = TempDir::new("recreate_increment");
    let restored = TempDir::new("recreate_restored");
    let mut catalog = Catalog::create(source.path()).unwrap();
    catalog.create_table(document_schema()).unwrap();
    let dropped_id = activate_expression_index(&mut catalog);
    let base = powdb_backup::full_backup(&mut catalog, full.path()).unwrap();

    catalog.alter_table_drop_column("Document", "data").unwrap();
    catalog
        .alter_table_add_column(
            "Document",
            ColumnDef {
                name: "data".into(),
                type_id: TypeId::Json,
                required: false,
                position: 1,
            },
        )
        .unwrap();
    let recreated_id = activate_expression_index(&mut catalog);
    assert!(recreated_id > dropped_id, "index IDs must not be reused");
    powdb_backup::incremental_backup(&mut catalog, &base, increment.path()).unwrap();
    drop(catalog);

    powdb_backup::restore_chain(full.path(), &[increment.path()], restored.path()).unwrap();
    let restored_catalog = Catalog::open(restored.path()).unwrap();
    let metadata = restored_catalog
        .expression_index_metadata("Document")
        .unwrap();
    assert_eq!(
        metadata
            .iter()
            .map(|metadata| metadata.index_id)
            .collect::<Vec<_>>(),
        vec![recreated_id]
    );
    assert!(
        restored_catalog
            .expression_index_btree("Document", dropped_id)
            .is_none(),
        "a stale base artifact must not become active without catalog metadata"
    );
}

#[test]
fn sync_snapshot_metadata_uses_the_active_v6_catalog_version() {
    let source = TempDir::new("sync_v6_source");
    let backup = TempDir::new("sync_v6_backup");
    let mut catalog = Catalog::create(source.path()).unwrap();
    catalog.create_table(document_schema()).unwrap();
    activate_expression_index(&mut catalog);
    powdb_sync::open_or_create_identity(source.path()).unwrap();

    let manifest = powdb_backup::full_backup(&mut catalog, backup.path()).unwrap();
    assert_eq!(manifest.catalog_version, CATALOG_VERSION);
    assert_eq!(manifest.sync.unwrap().catalog_version, CATALOG_VERSION);
}

#[test]
fn legacy_increment_json_without_catalog_version_defaults_to_v5() {
    let raw = r#"{
        "format_version": 1,
        "created_unix_secs": 1,
        "base_source_lsn": 1,
        "source_lsn": 2,
        "changed": []
    }"#;
    let manifest: IncrementManifest = serde_json::from_str(raw).unwrap();
    assert_eq!(manifest.catalog_version, LEGACY_CATALOG_VERSION);
    manifest.validate_version().unwrap();
}
