//! Track A (catalog v7): persisted relationship links.
//!
//! Covers the storage-layer contract for `create_link` / `link` / `links` /
//! `link_kind` / `derive_link_kind` / `drop_link`: v7 round-trip, the version
//! staircase (v5/v6 stay put until the first link, then activate v7), CRC
//! coverage of the new section, declare-time validation, the drop guards, and
//! the rule that cardinality is derived from current index uniqueness while
//! the persisted `LinkDef::kind` byte is advisory and never refreshed.
//! The old-binary refusal and the activation-rollback failpoint tests live in
//! the in-crate unit module (they touch private seams).

use powdb_storage::catalog::{
    Catalog, LinkDef, LinkKind, CATALOG_VERSION, EXPRESSION_INDEX_CATALOG_VERSION,
    LEGACY_CATALOG_VERSION,
};
use powdb_storage::stored_json_path::{StoredJsonPathSegmentV1, StoredJsonPathV1};
use powdb_storage::types::{ColumnDef, Schema, TypeId};

/// Two tables: `Order(user_id int, category str)` and `User(id int, name str)`.
/// `User.id` gets a UNIQUE index so a link onto it derives `ToOne`; `User.name`
/// stays non-unique so a link onto it derives `ToMany`.
fn setup() -> (tempfile::TempDir, Catalog) {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::create(dir.path()).unwrap();
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
    (dir, catalog)
}

fn user_link() -> LinkDef {
    LinkDef {
        owner_type: "Order".into(),
        name: "user".into(),
        target_type: "User".into(),
        local_key: "user_id".into(),
        target_key: "id".into(),
        kind: LinkKind::ToMany, // ignored on create; derived from uniqueness
    }
}

#[test]
fn v7_round_trip_preserves_links_across_reopen() {
    let (dir, mut catalog) = setup();
    catalog.create_link(user_link()).unwrap();
    // A second link onto a non-unique target key derives ToMany.
    catalog
        .create_link(LinkDef {
            owner_type: "Order".into(),
            name: "same_category_users".into(),
            target_type: "User".into(),
            local_key: "category".into(),
            target_key: "name".into(),
            kind: LinkKind::ToOne, // ignored; derived as ToMany
        })
        .unwrap();
    assert_eq!(catalog.active_catalog_version(), CATALOG_VERSION);

    let before: Vec<LinkDef> = catalog.links().cloned().collect();
    assert_eq!(before.len(), 2);
    assert_eq!(catalog.link("Order", "user").unwrap().kind, LinkKind::ToOne);
    assert_eq!(
        catalog.link("Order", "same_category_users").unwrap().kind,
        LinkKind::ToMany
    );

    drop(catalog);

    let reopened = Catalog::open(dir.path()).unwrap();
    assert_eq!(reopened.active_catalog_version(), CATALOG_VERSION);
    let after: Vec<LinkDef> = reopened.links().cloned().collect();
    assert_eq!(before, after);
    assert_eq!(reopened.link("Order", "user").unwrap(), &user_link_stored());
    assert!(reopened.link("Order", "missing").is_none());
}

fn user_link_stored() -> LinkDef {
    LinkDef {
        kind: LinkKind::ToOne,
        ..user_link()
    }
}

#[test]
fn staircase_fresh_db_stays_pre_v7_until_first_link() {
    let (dir, catalog) = setup();
    // A brand-new catalog that declared neither an expression index nor a link
    // is still at the legacy version and has no links.
    assert_eq!(catalog.active_catalog_version(), LEGACY_CATALOG_VERSION);
    assert_eq!(catalog.links().count(), 0);

    drop(catalog);
    let reopened = Catalog::open(dir.path()).unwrap();
    assert_eq!(reopened.active_catalog_version(), LEGACY_CATALOG_VERSION);
    assert_eq!(reopened.links().count(), 0);
    drop(reopened);

    let mut catalog = Catalog::open(dir.path()).unwrap();
    catalog.create_link(user_link()).unwrap();
    assert_eq!(catalog.active_catalog_version(), CATALOG_VERSION);
    drop(catalog);

    let reopened = Catalog::open(dir.path()).unwrap();
    assert_eq!(reopened.active_catalog_version(), CATALOG_VERSION);
    assert!(reopened.link("Order", "user").is_some());
}

#[test]
fn staircase_v6_db_activates_v7_on_first_link() {
    // A database already at v6 (it declared an expression index) must upgrade
    // cleanly to v7 on the first link, not stay stuck or skip a version.
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::create(dir.path()).unwrap();
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
            table_name: "Doc2".into(),
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
    assert_eq!(catalog.links().count(), 0);

    catalog
        .create_link(LinkDef {
            owner_type: "Doc2".into(),
            name: "author".into(),
            target_type: "Author".into(),
            local_key: "author_id".into(),
            target_key: "id".into(),
            kind: LinkKind::ToMany,
        })
        .unwrap();
    assert_eq!(catalog.active_catalog_version(), CATALOG_VERSION);
    drop(catalog);

    // Reopen: both the expression index and the link survive.
    let reopened = Catalog::open(dir.path()).unwrap();
    assert_eq!(reopened.active_catalog_version(), CATALOG_VERSION);
    assert_eq!(
        reopened.link("Doc2", "author").unwrap().kind,
        LinkKind::ToOne
    );
    assert!(!reopened
        .expression_index_metadata("Doc")
        .unwrap()
        .is_empty());
}

#[test]
fn crc_covers_links_section() {
    let (dir, mut catalog) = setup();
    catalog.create_link(user_link()).unwrap();
    drop(catalog);

    // Flip a byte inside the links section. It lives near the end of the file,
    // just before the trailing 4-byte CRC, so mutating a byte a few positions
    // in from the end lands in a link string / kind and must fail the CRC.
    let cat_path = dir.path().join("catalog.bin");
    let mut bytes = std::fs::read(&cat_path).unwrap();
    let n = bytes.len();
    // Target a byte inside the payload but outside the CRC suffix.
    let target = n - 6;
    bytes[target] ^= 0xff;
    std::fs::write(&cat_path, &bytes).unwrap();

    let err = match Catalog::open(dir.path()) {
        Ok(_) => panic!("open must reject a tampered links section"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("CRC32 mismatch"),
        "expected CRC mismatch, got: {err}"
    );
}

#[test]
fn create_link_rejects_unknown_target_type() {
    let (_dir, mut catalog) = setup();
    let err = catalog
        .create_link(LinkDef {
            target_type: "Ghost".into(),
            ..user_link()
        })
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(err.to_string().contains("Ghost"));
}

#[test]
fn create_link_rejects_unknown_owner_type() {
    let (_dir, mut catalog) = setup();
    let err = catalog
        .create_link(LinkDef {
            owner_type: "Ghost".into(),
            ..user_link()
        })
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn create_link_rejects_unknown_local_key() {
    let (_dir, mut catalog) = setup();
    let err = catalog
        .create_link(LinkDef {
            local_key: "nope".into(),
            ..user_link()
        })
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(err.to_string().contains("local key"));
}

#[test]
fn create_link_rejects_unknown_target_key() {
    let (_dir, mut catalog) = setup();
    let err = catalog
        .create_link(LinkDef {
            target_key: "nope".into(),
            ..user_link()
        })
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(err.to_string().contains("target key"));
}

#[test]
fn create_link_rejects_duplicate_name() {
    let (_dir, mut catalog) = setup();
    catalog.create_link(user_link()).unwrap();
    let err = catalog.create_link(user_link()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(err.to_string().contains("already exists"));
}

#[test]
fn create_link_rejects_name_colliding_with_column() {
    let (_dir, mut catalog) = setup();
    // "category" is a column on Order.
    let err = catalog
        .create_link(LinkDef {
            name: "category".into(),
            ..user_link()
        })
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(err.to_string().contains("collides with a column"));
}

#[test]
fn kind_is_to_one_for_unique_target_key_and_to_many_otherwise() {
    let (_dir, mut catalog) = setup();
    // Unique target key -> ToOne.
    catalog.create_link(user_link()).unwrap();
    assert_eq!(catalog.link("Order", "user").unwrap().kind, LinkKind::ToOne);
    // Non-unique target key -> ToMany.
    catalog
        .create_link(LinkDef {
            name: "roommates".into(),
            local_key: "category".into(),
            target_key: "name".into(),
            ..user_link()
        })
        .unwrap();
    assert_eq!(
        catalog.link("Order", "roommates").unwrap().kind,
        LinkKind::ToMany
    );
}

#[test]
fn dropping_a_linked_table_is_refused() {
    let (_dir, mut catalog) = setup();
    catalog.create_link(user_link()).unwrap();
    // User is a link target; Order is a link owner. Both are pinned.
    let target_err = catalog.drop_table("User").unwrap_err();
    assert_eq!(target_err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(target_err.to_string().contains("references it"));
    let owner_err = catalog.drop_table("Order").unwrap_err();
    assert_eq!(owner_err.kind(), std::io::ErrorKind::InvalidInput);

    // After dropping the link both tables become droppable again.
    catalog.drop_link("Order", "user").unwrap();
    catalog.drop_table("User").unwrap();
}

#[test]
fn dropping_a_linked_column_is_refused() {
    let (_dir, mut catalog) = setup();
    catalog.create_link(user_link()).unwrap();
    // Order.user_id is the local key; User.id is the target key. Both pinned.
    let local_err = catalog
        .alter_table_drop_column("Order", "user_id")
        .unwrap_err();
    assert_eq!(local_err.kind(), std::io::ErrorKind::InvalidInput);
    let target_err = catalog.alter_table_drop_column("User", "id").unwrap_err();
    assert_eq!(target_err.kind(), std::io::ErrorKind::InvalidInput);

    // An unrelated column stays droppable.
    catalog
        .alter_table_drop_column("Order", "category")
        .unwrap();

    // After dropping the link the pinned column becomes droppable.
    catalog.drop_link("Order", "user").unwrap();
    catalog.alter_table_drop_column("User", "id").unwrap();
}

#[test]
fn drop_link_removes_only_the_named_link_and_persists() {
    let (dir, mut catalog) = setup();
    catalog.create_link(user_link()).unwrap();
    catalog
        .create_link(LinkDef {
            name: "roommates".into(),
            local_key: "category".into(),
            target_key: "name".into(),
            ..user_link()
        })
        .unwrap();
    catalog.drop_link("Order", "user").unwrap();
    assert!(catalog.link("Order", "user").is_none());
    assert!(catalog.link("Order", "roommates").is_some());
    drop(catalog);

    let reopened = Catalog::open(dir.path()).unwrap();
    assert!(reopened.link("Order", "user").is_none());
    assert!(reopened.link("Order", "roommates").is_some());
}

#[test]
fn drop_link_unknown_is_not_found() {
    let (_dir, mut catalog) = setup();
    let err = catalog.drop_link("Order", "ghost").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

// ---------------------------------------------------------------------------
// The stored kind byte is advisory: derived is the only source of truth
// ---------------------------------------------------------------------------

/// Storage-side half of RULE L. The engine-wide parity suite for link
/// cardinality lives in `crates/query/tests/link_cardinality_parity.rs`, so
/// `cargo test -p powdb-storage` on its own would not notice the catalog
/// growing a cache again. These two pin the contract at this layer.
///
/// `Catalog::link_kind` derives from the target key's uniqueness at the moment
/// of the call, so DDL that lands after the link is visible immediately, with
/// no re-declaration and no reopen.
#[test]
fn link_kind_is_derived_from_current_uniqueness_not_declaration_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::create(dir.path()).unwrap();
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
            columns: vec![ColumnDef {
                name: "id".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            }],
        })
        .unwrap();
    // Declare the link BEFORE the unique index: the order that used to freeze
    // a link as to-many for the life of the database.
    catalog.create_link(user_link()).unwrap();
    assert_eq!(catalog.link_kind("Order", "user"), Some(LinkKind::ToMany));
    assert_eq!(catalog.derive_link_kind("User", "id"), LinkKind::ToMany);

    catalog.create_index_unique("User", "id", true).unwrap();
    assert_eq!(
        catalog.link_kind("Order", "user"),
        Some(LinkKind::ToOne),
        "uniqueness added after the link must be visible to the next read"
    );
    assert_eq!(catalog.link_kind("Order", "ghost"), None);

    // The persisted byte is NOT brought along, in memory or across a reopen.
    // That is the contract, not an oversight: a mirror that is kept in step
    // answers before the live derivation does and leaves it untested.
    assert_eq!(
        catalog.link("Order", "user").unwrap().kind,
        LinkKind::ToMany
    );
    drop(catalog);
    let reopened = Catalog::open(dir.path()).unwrap();
    assert_eq!(
        reopened.link("Order", "user").unwrap().kind,
        LinkKind::ToMany,
        "opening a catalog must not repair the advisory byte"
    );
    assert_eq!(reopened.link_kind("Order", "user"), Some(LinkKind::ToOne));
}

/// The drop guards pin a referenced table or column in place. PowQL has no
/// statement that removes a link, so the refusal names the API that does
/// rather than a `drop link` statement the parser would reject.
#[test]
fn drop_guards_name_a_remedy_that_exists() {
    let (_dir, mut catalog) = setup();
    catalog.create_link(user_link()).unwrap();

    for err in [
        catalog.drop_table("User").unwrap_err().to_string(),
        catalog.drop_table("Order").unwrap_err().to_string(),
        catalog
            .alter_table_drop_column("User", "id")
            .unwrap_err()
            .to_string(),
        catalog
            .alter_table_drop_column("Order", "user_id")
            .unwrap_err()
            .to_string(),
    ] {
        assert!(
            err.contains("Catalog::drop_link(\"Order\", \"user\")"),
            "must name the API that can actually remove the link: {err}"
        );
        assert!(
            err.contains("PowQL has no statement that removes a link"),
            "must say why the obvious remedy is unavailable: {err}"
        );
        assert!(
            !err.contains("drop the link first"),
            "must not name a statement PowQL does not have: {err}"
        );
    }
}
