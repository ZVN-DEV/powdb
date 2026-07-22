//! Track D (catalog v7): backup/restore carries persisted links.
//!
//! Design doc section 6, item 7: a v7 database (one that declared at least one
//! ToOne and one ToMany link) must round-trip through both a full backup and an
//! incremental chain. After restore the reopened catalog stays at v7, every
//! `LinkDef` is byte-identical, and the on-disk manifest advertises v7. Links
//! ride along because the backup copies `catalog.bin` verbatim; these tests pin
//! that contract so a future backup-file refactor cannot silently drop links.

use powdb_storage::catalog::{Catalog, LinkDef, LinkKind, CATALOG_VERSION, LEGACY_CATALOG_VERSION};
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};
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
            "powdb_backup_catalog_v7_{label}_{}_{}",
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

/// Two tables: `Order(user_id int, category str)` and `User(id int, name str)`.
/// `User.id` gets a UNIQUE index so a link onto it derives `ToOne`; `User.name`
/// stays non-unique so a link onto it derives `ToMany`. This mirrors the
/// storage-layer `catalog_links.rs` fixture so the two suites share a shape.
fn build_linked_catalog(dir: &Path) -> Catalog {
    let mut catalog = Catalog::create(dir).unwrap();
    catalog
        .create_table(Schema {
            table_name: "Order".into(),
            columns: vec![
                ColumnDef {
                    name: "user_id".into(),
                    type_id: TypeId::Int,
                    required: false,
                    position: 0,
                },
                ColumnDef {
                    name: "category".into(),
                    type_id: TypeId::Str,
                    required: false,
                    position: 1,
                },
            ],
        })
        .unwrap();
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
            ],
        })
        .unwrap();
    catalog.create_index_unique("User", "id", true).unwrap();
    catalog.create_index_unique("User", "name", false).unwrap();
    catalog
}

/// Declare one ToOne link (`user` onto the unique `User.id`) and one ToMany link
/// (`same_category_users` onto the non-unique `User.name`).
fn declare_both_links(catalog: &mut Catalog) {
    catalog
        .create_link(LinkDef {
            owner_type: "Order".into(),
            name: "user".into(),
            target_type: "User".into(),
            local_key: "user_id".into(),
            target_key: "id".into(),
            kind: LinkKind::ToMany, // ignored on create; derived as ToOne (unique)
        })
        .unwrap();
    catalog
        .create_link(LinkDef {
            owner_type: "Order".into(),
            name: "same_category_users".into(),
            target_type: "User".into(),
            local_key: "category".into(),
            target_key: "name".into(),
            kind: LinkKind::ToOne, // ignored on create; derived as ToMany (non-unique)
        })
        .unwrap();
}

fn insert_rows(catalog: &mut Catalog) {
    catalog
        .insert("User", &vec![Value::Int(1), Value::Str("Ada".into())])
        .unwrap();
    catalog
        .insert("User", &vec![Value::Int(2), Value::Str("Grace".into())])
        .unwrap();
    catalog
        .insert("Order", &vec![Value::Int(1), Value::Str("books".into())])
        .unwrap();
    catalog
        .insert("Order", &vec![Value::Int(2), Value::Str("books".into())])
        .unwrap();
}

/// Assert the restored catalog carries exactly the two declared links with the
/// derived kinds intact and byte-identical to the source registry.
fn assert_links_present(catalog: &Catalog, expected: &[LinkDef]) {
    let restored: Vec<LinkDef> = catalog.links().cloned().collect();
    assert_eq!(
        restored, expected,
        "every LinkDef must survive backup/restore byte-for-byte"
    );
    let user = catalog.link("Order", "user").unwrap();
    assert_eq!(user.kind, LinkKind::ToOne);
    assert_eq!(user.target_type, "User");
    assert_eq!(user.local_key, "user_id");
    assert_eq!(user.target_key, "id");
    let many = catalog.link("Order", "same_category_users").unwrap();
    assert_eq!(many.kind, LinkKind::ToMany);
    assert_eq!(many.target_key, "name");
}

/// Read the on-disk backup manifest JSON and return its catalog_version field.
fn manifest_catalog_version_on_disk(backup_dir: &Path, manifest_name: &str) -> u64 {
    let raw = std::fs::read_to_string(backup_dir.join(manifest_name)).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    json["catalog_version"]
        .as_u64()
        .expect("manifest must record a catalog_version")
}

#[test]
fn full_backup_restore_carries_to_one_and_to_many_links() {
    let source = TempDir::new("full_source");
    let backup = TempDir::new("full_backup");
    let restored = TempDir::new("full_restored");

    let mut catalog = build_linked_catalog(source.path());
    declare_both_links(&mut catalog);
    insert_rows(&mut catalog);
    catalog.sync_wal().unwrap();
    assert_eq!(catalog.active_catalog_version(), CATALOG_VERSION);
    let expected: Vec<LinkDef> = catalog.links().cloned().collect();
    assert_eq!(expected.len(), 2);

    // (c) the returned and on-disk manifests both advertise v7.
    let manifest = powdb_backup::full_backup(&mut catalog, backup.path()).unwrap();
    assert_eq!(manifest.catalog_version, CATALOG_VERSION);
    assert!(
        manifest.files.iter().any(|f| f.name == "catalog.bin"),
        "the links live in catalog.bin, which must be part of the snapshot"
    );
    assert_eq!(
        manifest_catalog_version_on_disk(backup.path(), powdb_backup::BackupManifest::FILE_NAME),
        CATALOG_VERSION as u64
    );
    drop(catalog);

    powdb_backup::restore(backup.path(), restored.path()).unwrap();

    let reopened = Catalog::open(restored.path()).unwrap();
    // (a) restored catalog is at v7.
    assert_eq!(reopened.active_catalog_version(), CATALOG_VERSION);
    // (b) every LinkDef present and byte-identical.
    assert_links_present(&reopened, &expected);
    // Rows ride along too: the restored DB is fully usable.
    assert_eq!(reopened.scan("User").unwrap().count(), 2);
    assert_eq!(reopened.scan("Order").unwrap().count(), 2);
}

#[test]
fn incremental_backup_restore_chain_carries_links() {
    let source = TempDir::new("inc_source");
    let full = TempDir::new("inc_full");
    let increment = TempDir::new("inc_delta");
    let restored = TempDir::new("inc_restored");

    // Base snapshot is taken BEFORE any link exists, so the base is a pre-v7
    // (legacy) catalog and the v7 activation shows up only in the increment.
    let mut catalog = build_linked_catalog(source.path());
    // A distinct id so the later `insert_rows` (ids 1, 2) cannot collide.
    catalog
        .insert("User", &vec![Value::Int(99), Value::Str("Zoe".into())])
        .unwrap();
    catalog.sync_wal().unwrap();
    assert_eq!(catalog.active_catalog_version(), LEGACY_CATALOG_VERSION);
    let base = powdb_backup::full_backup(&mut catalog, full.path()).unwrap();
    assert_eq!(base.catalog_version, LEGACY_CATALOG_VERSION);

    // Now declare the links (activates v7) and add more rows.
    declare_both_links(&mut catalog);
    insert_rows(&mut catalog);
    catalog.sync_wal().unwrap();
    assert_eq!(catalog.active_catalog_version(), CATALOG_VERSION);
    let expected: Vec<LinkDef> = catalog.links().cloned().collect();
    assert_eq!(expected.len(), 2);

    let inc = powdb_backup::incremental_backup(&mut catalog, &base, increment.path()).unwrap();
    assert_eq!(
        inc.catalog_version, CATALOG_VERSION,
        "the increment must advertise the v7 activation"
    );
    assert_eq!(
        manifest_catalog_version_on_disk(
            increment.path(),
            powdb_backup::IncrementManifest::FILE_NAME
        ),
        CATALOG_VERSION as u64
    );
    drop(catalog);

    powdb_backup::restore_chain(full.path(), &[increment.path()], restored.path()).unwrap();

    let reopened = Catalog::open(restored.path()).unwrap();
    assert_eq!(reopened.active_catalog_version(), CATALOG_VERSION);
    assert_links_present(&reopened, &expected);
    // The base row (id 99) plus the two increment rows all restore.
    assert_eq!(reopened.scan("User").unwrap().count(), 3);
    assert_eq!(reopened.scan("Order").unwrap().count(), 2);
}
