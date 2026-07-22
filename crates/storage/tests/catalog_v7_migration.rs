//! Track D (catalog v7): in-place v6 -> v7 upgrade of an already-populated DB.
//!
//! Design doc section 6, item 6 migration leg, storage-layer half. The engine
//! test `entity_links_persistence.rs` covers a fresh database growing links;
//! `catalog_links.rs::staircase_v6_db_activates_v7_on_first_link` upgrades in
//! the SAME session that created the v6 state. This test instead exercises the
//! genuinely-reopened case: write a v6 catalog (an expression index activates
//! v6), close it, reopen and confirm it is still v6 with zero links, THEN
//! declare the first link and confirm lazy activation to v7 that persists across
//! yet another reopen. This is the "adopt links on an existing v6 database" path.

use powdb_storage::catalog::{
    Catalog, LinkDef, LinkKind, CATALOG_VERSION, EXPRESSION_INDEX_CATALOG_VERSION,
};
use powdb_storage::stored_json_path::{StoredJsonPathSegmentV1, StoredJsonPathV1};
use powdb_storage::types::{ColumnDef, Schema, TypeId};

/// Build a database and drive it to a genuine v6 by activating one expression
/// index, then drop it so the v6 state is fully on disk.
fn write_v6_database(dir: &std::path::Path) {
    let mut catalog = Catalog::create(dir).unwrap();
    catalog
        .create_table(Schema {
            table_name: "Doc".into(),
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
        })
        .unwrap();
    catalog
        .create_table(Schema {
            table_name: "Author".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            }],
        })
        .unwrap();
    catalog.create_index_unique("Author", "id", true).unwrap();
    catalog
        .create_table(Schema {
            table_name: "Note".into(),
            columns: vec![ColumnDef {
                name: "author_id".into(),
                type_id: TypeId::Int,
                required: false,
                position: 0,
            }],
        })
        .unwrap();

    let path = StoredJsonPathV1::new("data", vec![StoredJsonPathSegmentV1::Key("k".into())]);
    catalog
        .create_expression_index_metadata("Doc", 1, path.canonical_text(), path, false)
        .unwrap();
    assert_eq!(
        catalog.active_catalog_version(),
        EXPRESSION_INDEX_CATALOG_VERSION
    );
    // Clean shutdown persists the v6 catalog to disk.
    drop(catalog);
}

#[test]
fn reopened_v6_database_upgrades_to_v7_on_first_link_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    write_v6_database(dir.path());

    // Reopen the existing v6 database: it must come back at v6 with no links.
    let mut catalog = Catalog::open(dir.path()).unwrap();
    assert_eq!(
        catalog.active_catalog_version(),
        EXPRESSION_INDEX_CATALOG_VERSION,
        "a reopened v6 database must not silently advance its format"
    );
    assert_eq!(catalog.links().count(), 0);

    // Declaring the first link on the already-populated v6 DB lazily activates
    // v7 in place — no data rewrite, the expression index is untouched.
    catalog
        .create_link(LinkDef {
            owner_type: "Note".into(),
            name: "author".into(),
            target_type: "Author".into(),
            local_key: "author_id".into(),
            target_key: "id".into(),
            kind: LinkKind::ToMany, // ignored on create; derived ToOne (unique)
        })
        .unwrap();
    assert_eq!(catalog.active_catalog_version(), CATALOG_VERSION);
    drop(catalog);

    // The upgrade survives another reopen: v7, the link, and the pre-existing
    // v6 expression index all persist together.
    let reopened = Catalog::open(dir.path()).unwrap();
    assert_eq!(reopened.active_catalog_version(), CATALOG_VERSION);
    let link = reopened.link("Note", "author").unwrap();
    assert_eq!(link.kind, LinkKind::ToOne);
    assert_eq!(&link.target_type, "Author");
    assert!(
        !reopened
            .expression_index_metadata("Doc")
            .unwrap()
            .is_empty(),
        "the v6 expression index must survive the v7 upgrade"
    );
}
