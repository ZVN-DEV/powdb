//! Track A (catalog v7): durability of persisted links.
//!
//! The catalog format is a highest-rigor change, so these tests exercise link
//! metadata surviving alongside row data across both a clean shutdown and a
//! simulated crash (skipping the drop-time checkpoint with `mem::forget`, the
//! same seam the expression-index durability tests use). The real-binary
//! `kill -9` smoke is a later track; these guard the storage layer.

use powdb_storage::catalog::{Catalog, LinkDef, LinkKind, CATALOG_VERSION};
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};

fn build(dir: &std::path::Path) -> Catalog {
    let mut catalog = Catalog::create(dir).unwrap();
    catalog
        .create_table(Schema {
            table_name: "Order".into(),
            columns: vec![ColumnDef {
                name: "user_id".into(),
                type_id: TypeId::Int,
                required: false,
                position: 0,
            }],
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
    catalog
}

fn user_link() -> LinkDef {
    LinkDef {
        owner_type: "Order".into(),
        name: "user".into(),
        target_type: "User".into(),
        local_key: "user_id".into(),
        target_key: "id".into(),
        kind: LinkKind::ToMany,
    }
}

fn row_count(catalog: &Catalog, table: &str) -> usize {
    catalog.get_table(table).unwrap().scan().count()
}

#[test]
fn link_and_rows_survive_clean_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = build(dir.path());
    catalog.create_link(user_link()).unwrap();
    catalog
        .insert("User", &vec![Value::Int(1), Value::Str("Ada".into())])
        .unwrap();
    catalog.insert("Order", &vec![Value::Int(1)]).unwrap();
    catalog.insert("Order", &vec![Value::Int(1)]).unwrap();
    // Clean shutdown: Drop checkpoints and truncates the WAL.
    drop(catalog);

    let reopened = Catalog::open(dir.path()).unwrap();
    assert_eq!(reopened.active_catalog_version(), CATALOG_VERSION);
    assert_eq!(
        reopened.link("Order", "user").unwrap().kind,
        LinkKind::ToOne
    );
    assert_eq!(row_count(&reopened, "User"), 1);
    assert_eq!(row_count(&reopened, "Order"), 2);
}

#[test]
fn link_survives_simulated_crash_before_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = build(dir.path());
    // The link is persisted through the activation boundary (its own fsync +
    // rename), independently of any later heap checkpoint — so it must survive a
    // crash even though an uncommitted autocommit insert would not. (Row
    // durability across an *unclean* shutdown is the WAL's concern and is
    // covered by the durability suite; here we only assert the link.)
    catalog.create_link(user_link()).unwrap();
    // Simulate a crash: skip Drop's checkpoint so recovery must run on reopen.
    std::mem::forget(catalog);

    let reopened = Catalog::open(dir.path()).unwrap();
    assert_eq!(reopened.active_catalog_version(), CATALOG_VERSION);
    let link = reopened.link("Order", "user").unwrap();
    assert_eq!(link.kind, LinkKind::ToOne);
    assert_eq!(&link.target_type, "User");
}

#[test]
fn link_and_rows_survive_simulated_crash_before_checkpoint() {
    // Design doc section 6, item 6 (storage-layer half of the real-binary
    // `kill -9` smoke): a link declared AND rows inserted with NO clean
    // checkpoint must both recover on reopen. The sibling
    // `link_survives_simulated_crash_before_checkpoint` deliberately asserts
    // only the link and hands row durability to the WAL suite; this test closes
    // that gap by asserting both halves recover together across one crash.
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = build(dir.path());
    // Declaring the link persists catalog.bin at v7 through its own activation
    // fsync + rename, independent of any heap checkpoint.
    catalog.create_link(user_link()).unwrap();
    // Insert rows after activation so they live only in the WAL + hot-page
    // cache; the crash must replay them onto the freshly reopened heap.
    catalog
        .insert("User", &vec![Value::Int(1), Value::Str("Ada".into())])
        .unwrap();
    catalog.insert("Order", &vec![Value::Int(1)]).unwrap();
    catalog.insert("Order", &vec![Value::Int(1)]).unwrap();
    // Flush the WAL but take NO checkpoint, then simulate a crash by skipping
    // Drop so the hot-page cache holding the rows is lost and recovery runs.
    catalog.sync_wal().unwrap();
    std::mem::forget(catalog);

    let reopened = Catalog::open(dir.path()).unwrap();
    assert_eq!(reopened.active_catalog_version(), CATALOG_VERSION);
    // The link recovers from catalog.bin.
    let link = reopened.link("Order", "user").unwrap();
    assert_eq!(link.kind, LinkKind::ToOne);
    assert_eq!(&link.target_type, "User");
    // The rows recover via WAL replay.
    assert_eq!(row_count(&reopened, "User"), 1);
    assert_eq!(row_count(&reopened, "Order"), 2);
}

#[test]
fn multiple_links_round_trip_in_declaration_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = build(dir.path());
    // Add a second non-unique index so the second link derives ToMany.
    catalog.create_index_unique("User", "name", false).unwrap();
    catalog.create_link(user_link()).unwrap();
    catalog
        .create_link(LinkDef {
            owner_type: "User".into(),
            name: "namesakes".into(),
            target_type: "User".into(),
            local_key: "name".into(),
            target_key: "name".into(),
            kind: LinkKind::ToOne,
        })
        .unwrap();
    let before: Vec<LinkDef> = catalog.links().cloned().collect();
    drop(catalog);

    let reopened = Catalog::open(dir.path()).unwrap();
    let after: Vec<LinkDef> = reopened.links().cloned().collect();
    assert_eq!(before, after);
    assert_eq!(after.len(), 2);
    assert_eq!(after[0].name, "user");
    assert_eq!(after[0].kind, LinkKind::ToOne);
    assert_eq!(after[1].name, "namesakes");
    assert_eq!(after[1].kind, LinkKind::ToMany);
}
