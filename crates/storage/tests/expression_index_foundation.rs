use powdb_storage::btree::{BTree, BTREE_VERSION, LEGACY_BTREE_VERSION};
use powdb_storage::catalog::{
    expression_index_file_name, Catalog, IndexKeySource, CATALOG_VERSION, LEGACY_CATALOG_VERSION,
};
use powdb_storage::pj1::parse_json_text;
use powdb_storage::stored_json_path::{StoredJsonPathSegmentV1, StoredJsonPathV1};
use powdb_storage::types::{ColumnDef, RowId, Schema, TypeId, Value};

fn schema(name: &str) -> Schema {
    Schema {
        table_name: name.into(),
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

fn author_path() -> StoredJsonPathV1 {
    StoredJsonPathV1::new("data", vec![StoredJsonPathSegmentV1::Key("author".into())])
}

#[test]
fn expression_namespace_cannot_alias_cross_table_column_index_and_rebuilds_safely() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::create(dir.path()).unwrap();
    catalog
        .create_table(Schema {
            table_name: "A".into(),
            columns: vec![ColumnDef {
                name: "B_idx_1".into(),
                type_id: TypeId::Str,
                required: true,
                position: 0,
            }],
        })
        .unwrap();
    catalog.create_table(schema("A_B")).unwrap();

    let column_row = vec![Value::Str("legacy-key".into())];
    catalog.insert("A", &column_row).unwrap();
    catalog.create_index("A", "B_idx_1").unwrap();
    let column_file = dir.path().join("A_B_idx_1.idx");
    assert!(column_file.exists());
    let column_bytes = std::fs::read(&column_file).unwrap();

    let expression_row = vec![
        Value::Int(1),
        Value::Json(
            parse_json_text(r#"{"author":"Ada"}"#)
                .unwrap()
                .into_boxed_slice(),
        ),
    ];
    let expression_rid = catalog.insert("A_B", &expression_row).unwrap();
    catalog.sync_wal().unwrap();

    let path = author_path();
    let expression_id = catalog
        .create_expression_index_metadata("A_B", 1, path.canonical_text(), path, false)
        .unwrap();
    assert_eq!(expression_id, 1);
    assert_eq!(catalog.next_index_id(), 2);
    let expression_file = dir
        .path()
        .join(expression_index_file_name("A_B", expression_id));
    assert!(expression_file.exists());
    assert_eq!(std::fs::read(&column_file).unwrap(), column_bytes);
    assert_eq!(
        catalog
            .index_lookup("A", "B_idx_1", &Value::Str("legacy-key".into()))
            .unwrap(),
        Some(column_row.clone())
    );
    assert_eq!(
        catalog
            .expression_index_lookup_all("A_B", expression_id, &Value::Str("Ada".into()))
            .unwrap(),
        vec![expression_rid]
    );
    catalog.checkpoint().unwrap();
    drop(catalog);

    // A missing expression artifact is rebuilt from the heap. The old flat
    // name is a live column index here, so reopen must neither inspect nor
    // delete it while reconstructing the dedicated `.eidx` file.
    std::fs::remove_file(&expression_file).unwrap();
    let mut reopened = Catalog::open(dir.path()).unwrap();
    assert!(expression_file.exists());
    assert_eq!(std::fs::read(&column_file).unwrap(), column_bytes);
    assert_eq!(
        reopened
            .index_lookup("A", "B_idx_1", &Value::Str("legacy-key".into()))
            .unwrap(),
        Some(column_row)
    );
    assert_eq!(
        reopened
            .expression_index_lookup_all("A_B", expression_id, &Value::Str("Ada".into()))
            .unwrap(),
        vec![expression_rid]
    );

    reopened
        .drop_expression_index("A_B", expression_id)
        .unwrap();
    assert!(!expression_file.exists());
    assert!(column_file.exists());
    assert_eq!(std::fs::read(column_file).unwrap(), column_bytes);
}

#[test]
fn catalog_v6_activates_lazily_and_ids_survive_crash_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::create(dir.path()).unwrap();
    catalog.create_table(schema("Doc")).unwrap();
    catalog.create_index("Doc", "id").unwrap();
    assert_eq!(catalog.active_catalog_version(), LEGACY_CATALOG_VERSION);
    assert_eq!(catalog.next_index_id(), 1);

    let invalid = catalog.create_expression_index_metadata(
        "Doc",
        1,
        "v1:.missing->\"author\"",
        StoredJsonPathV1::new(
            "missing",
            vec![StoredJsonPathSegmentV1::Key("author".into())],
        ),
        false,
    );
    assert!(invalid.is_err());
    assert_eq!(catalog.active_catalog_version(), LEGACY_CATALOG_VERSION);
    assert_eq!(catalog.next_index_id(), 1);

    // A crash after the file fsync but before catalog activation can leave an
    // unreferenced ID file. The durable allocator must be able to reclaim that
    // exact orphan without consuming or reusing a referenced ID.
    let first_file = dir.path().join(expression_index_file_name("Doc", 1));
    std::fs::write(&first_file, b"orphan").unwrap();

    let path = author_path();
    let first = catalog
        .create_expression_index_metadata("Doc", 1, path.canonical_text(), path.clone(), false)
        .unwrap();
    assert_eq!(first, 1);
    assert_eq!(catalog.active_catalog_version(), CATALOG_VERSION);
    assert_eq!(catalog.next_index_id(), 2);
    assert!(first_file.exists());
    assert!(dir.path().join("Doc_id.idx").exists());

    let tree = catalog.expression_index_btree_mut("Doc", first).unwrap();
    tree.insert_duplicate(
        Value::Str("Ada".into()),
        RowId {
            page_id: 9,
            slot_index: 2,
        },
    );
    tree.insert_empty(RowId {
        page_id: 9,
        slot_index: 3,
    });
    catalog.checkpoint().unwrap();
    std::mem::forget(catalog);

    let mut reopened = Catalog::open(dir.path()).unwrap();
    assert_eq!(reopened.active_catalog_version(), CATALOG_VERSION);
    assert_eq!(reopened.next_index_id(), 2);
    let metadata = reopened.index_metadata("Doc").unwrap();
    assert!(metadata.iter().any(|index| matches!(
        &index.source,
        IndexKeySource::Column { column } if column == "id"
    )));
    assert!(metadata.iter().any(|index| matches!(
        &index.source,
        IndexKeySource::Expression {
            index_id: 1,
            canonical_version: 1,
            canonical_text,
            json_path,
        } if canonical_text == "v1:.data->\"author\"" && json_path == &path
    )));
    let tree = reopened.expression_index_btree("Doc", first).unwrap();
    assert_eq!(tree.format_version(), BTREE_VERSION);
    assert_eq!(
        tree.lookup_all(&Value::Str("Ada".into())),
        vec![RowId {
            page_id: 9,
            slot_index: 2
        }]
    );
    assert_eq!(
        tree.empty_rids(),
        &[RowId {
            page_id: 9,
            slot_index: 3
        }]
    );

    reopened.create_table(schema("Other")).unwrap();
    assert_eq!(reopened.active_catalog_version(), CATALOG_VERSION);
    let second_path =
        StoredJsonPathV1::new("data", vec![StoredJsonPathSegmentV1::Key("score".into())]);
    let second = reopened
        .create_expression_index_metadata(
            "Doc",
            1,
            second_path.canonical_text(),
            second_path,
            false,
        )
        .unwrap();
    assert_eq!(second, 2);
    drop(reopened);

    let final_open = Catalog::open(dir.path()).unwrap();
    assert_eq!(final_open.next_index_id(), 3);
    assert_eq!(
        final_open.expression_index_metadata("Doc").unwrap().len(),
        2
    );
}

#[test]
fn btree_v2_raw_duplicates_split_delete_order_and_empty_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("expression.idx");
    let mut tree = BTree::create_v2(&path).unwrap();
    let key = Value::Str("same".into());

    for page_id in 1_000..1_700 {
        tree.insert_duplicate(
            key.clone(),
            RowId {
                page_id,
                slot_index: (page_id % 11) as u16,
            },
        );
    }
    tree.insert_duplicate(
        key.clone(),
        RowId {
            page_id: 7,
            slot_index: 1,
        },
    );
    tree.insert_duplicate(
        Value::Bool(false),
        RowId {
            page_id: 3,
            slot_index: 0,
        },
    );
    tree.insert_duplicate(
        Value::Int(42),
        RowId {
            page_id: 4,
            slot_index: 0,
        },
    );
    let empty_a = RowId {
        page_id: 8,
        slot_index: 2,
    };
    let empty_b = RowId {
        page_id: 8,
        slot_index: 1,
    };
    tree.insert_duplicate(Value::Empty, empty_a);
    tree.insert_duplicate(Value::Empty, empty_b);

    let duplicates = tree.lookup_all(&key);
    assert_eq!(duplicates.len(), 701);
    assert_eq!(duplicates[0].page_id, 7);
    assert!(duplicates.windows(2).all(|pair| {
        (pair[0].page_id, pair[0].slot_index) < (pair[1].page_id, pair[1].slot_index)
    }));
    assert!(tree.lookup_all(&Value::Empty).is_empty());
    assert_eq!(tree.empty_rids(), &[empty_b, empty_a]);

    let removed = RowId {
        page_id: 1_350,
        slot_index: (1_350 % 11) as u16,
    };
    assert!(tree.delete_pair(&key, removed));
    assert!(!tree.lookup_all(&key).contains(&removed));
    assert!(tree.delete_pair(&Value::Empty, empty_a));
    assert_eq!(tree.empty_rids(), &[empty_b]);

    let ordered = tree.ordered_pairs();
    assert!(ordered.windows(2).all(|pair| {
        pair[0].0 < pair[1].0
            || (pair[0].0 == pair[1].0
                && (pair[0].1.page_id, pair[0].1.slot_index)
                    < (pair[1].1.page_id, pair[1].1.slot_index))
    }));
    let nulls_last = tree.ordered_pairs_nulls_last();
    assert_eq!(nulls_last.last(), Some(&(Value::Empty, empty_b)));
    tree.save().unwrap();

    let reopened = BTree::load(&path).unwrap();
    assert_eq!(reopened.format_version(), BTREE_VERSION);
    assert_eq!(reopened.lookup_all(&key).len(), 700);
    assert_eq!(reopened.empty_rids(), &[empty_b]);

    let legacy_path = dir.path().join("legacy.idx");
    let mut legacy = BTree::create(&legacy_path).unwrap();
    legacy.insert(
        Value::Int(1),
        RowId {
            page_id: 1,
            slot_index: 0,
        },
    );
    legacy.save().unwrap();
    assert_eq!(
        BTree::load(&legacy_path).unwrap().format_version(),
        LEGACY_BTREE_VERSION
    );
}

#[test]
fn expression_index_files_follow_root_and_table_lifecycle_without_reusing_ids() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::create(dir.path()).unwrap();
    catalog.create_table(schema("Doc")).unwrap();
    catalog.create_table(schema("Archive")).unwrap();

    let first_path = author_path();
    let first = catalog
        .create_expression_index_metadata("Doc", 1, first_path.canonical_text(), first_path, false)
        .unwrap();
    assert_eq!(first, 1);
    let first_file = dir.path().join(expression_index_file_name("Doc", first));
    assert!(first_file.exists());

    catalog.alter_table_drop_column("Doc", "data").unwrap();
    assert!(catalog.expression_index_metadata("Doc").unwrap().is_empty());
    assert!(!first_file.exists());
    assert_eq!(catalog.next_index_id(), 2);

    let archive_path = author_path();
    let second = catalog
        .create_expression_index_metadata(
            "Archive",
            1,
            archive_path.canonical_text(),
            archive_path,
            false,
        )
        .unwrap();
    assert_eq!(second, 2);
    let second_file = dir
        .path()
        .join(expression_index_file_name("Archive", second));
    assert!(second_file.exists());

    catalog.drop_table("Archive").unwrap();
    assert!(!second_file.exists());
    assert!(catalog.index_metadata("Archive").is_none());
    assert_eq!(catalog.next_index_id(), 3);
    drop(catalog);

    let reopened = Catalog::open(dir.path()).unwrap();
    assert_eq!(reopened.active_catalog_version(), CATALOG_VERSION);
    assert_eq!(reopened.next_index_id(), 3);
    assert!(reopened
        .expression_index_metadata("Doc")
        .unwrap()
        .is_empty());
    assert!(reopened.index_metadata("Archive").is_none());
}
