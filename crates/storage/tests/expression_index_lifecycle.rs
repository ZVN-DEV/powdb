use powdb_storage::catalog::{
    expression_index_file_name, Catalog, IndexOrderDirection, EXPRESSION_INDEX_CATALOG_VERSION,
    LEGACY_CATALOG_VERSION,
};
use powdb_storage::pj1::parse_json_text;
use powdb_storage::stored_json_path::{StoredJsonPathSegmentV1, StoredJsonPathV1};
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};

fn schema() -> Schema {
    Schema {
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
            ColumnDef {
                name: "padding".into(),
                type_id: TypeId::Str,
                required: false,
                position: 2,
            },
        ],
    }
}

fn author_path() -> StoredJsonPathV1 {
    StoredJsonPathV1::new("data", vec![StoredJsonPathSegmentV1::Key("author".into())])
}

fn json(text: &str) -> Value {
    Value::Json(parse_json_text(text).unwrap().into_boxed_slice())
}

fn row(id: i64, data: Value, padding: impl Into<String>) -> Vec<Value> {
    vec![Value::Int(id), data, Value::Str(padding.into())]
}

#[test]
fn existing_build_insert_update_delete_spill_relocation_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::create(dir.path()).unwrap();
    catalog.create_table(schema()).unwrap();
    let ada = catalog
        .insert("Doc", &row(1, json(r#"{"author":"Ada"}"#), ""))
        .unwrap();
    let missing = catalog
        .insert("Doc", &row(2, json(r#"{"title":"Notes"}"#), ""))
        .unwrap();
    let null = catalog
        .insert("Doc", &row(3, json(r#"{"author":null}"#), ""))
        .unwrap();
    let large = "x".repeat(12_000);
    let preexisting_spilled = catalog
        .insert(
            "Doc",
            &row(
                4,
                json(&format!(r#"{{"author":"Grace","body":"{large}"}}"#)),
                "",
            ),
        )
        .unwrap();
    catalog.sync_wal().unwrap();

    let path = author_path();
    let index_id = catalog
        .create_expression_index_metadata("Doc", 1, path.canonical_text(), path, false)
        .unwrap();
    let tree = catalog.expression_index_btree("Doc", index_id).unwrap();
    assert_eq!(tree.lookup_all(&Value::Str("Ada".into())), vec![ada]);
    assert_eq!(tree.empty_rids(), &[missing, null]);
    assert_eq!(
        tree.lookup_all(&Value::Str("Grace".into())),
        vec![preexisting_spilled]
    );
    assert_eq!(
        catalog
            .expression_index_lookup_all("Doc", index_id, &Value::Str("Ada".into()))
            .unwrap(),
        vec![ada]
    );
    assert_eq!(
        catalog
            .expression_index_range_rids(
                "Doc",
                index_id,
                Some(&Value::Str("Ada".into())),
                Some(&Value::Str("Grace".into())),
            )
            .unwrap(),
        vec![ada, preexisting_spilled]
    );
    assert_eq!(
        catalog
            .expression_index_ordered_rids("Doc", index_id)
            .unwrap(),
        vec![ada, preexisting_spilled, missing, null]
    );
    assert_eq!(
        catalog
            .expression_index_ordered_rids_bounded("Doc", index_id, IndexOrderDirection::Asc, 1, 3,)
            .unwrap(),
        vec![preexisting_spilled, missing, null]
    );
    assert_eq!(
        catalog
            .expression_index_ordered_rids_bounded(
                "Doc",
                index_id,
                IndexOrderDirection::Desc,
                1,
                3,
            )
            .unwrap(),
        vec![ada, missing, null]
    );
    assert_eq!(
        catalog
            .get_projected("Doc", preexisting_spilled, &[1])
            .unwrap()
            .unwrap(),
        vec![json(&format!(r#"{{"author":"Grace","body":"{large}"}}"#))]
    );

    let spilled = catalog
        .insert(
            "Doc",
            &row(
                5,
                json(&format!(r#"{{"author":"Hopper","body":"{large}"}}"#)),
                "",
            ),
        )
        .unwrap();
    assert!(catalog.table_by_slot(0).has_overflow_rows());
    assert_eq!(
        catalog
            .expression_index_btree("Doc", index_id)
            .unwrap()
            .lookup_all(&Value::Str("Hopper".into())),
        vec![spilled]
    );

    let mut relocation_target = None;
    for id in 10..120 {
        let inserted = catalog
            .insert(
                "Doc",
                &row(id, json(&format!(r#"{{"author":"A{id}"}}"#)), ""),
            )
            .unwrap();
        if id == 10 {
            relocation_target = Some(inserted);
        }
    }
    let old_rid = relocation_target.unwrap();
    let new_rid = catalog
        .update(
            "Doc",
            old_rid,
            &row(10, json(r#"{"author":"Relocated"}"#), "p".repeat(3_500)),
        )
        .unwrap();
    assert_ne!(new_rid, old_rid, "test setup must force RID relocation");
    let tree = catalog.expression_index_btree("Doc", index_id).unwrap();
    assert!(tree.lookup_all(&Value::Str("A10".into())).is_empty());
    assert_eq!(
        tree.lookup_all(&Value::Str("Relocated".into())),
        vec![new_rid]
    );

    let moved_from_empty = catalog
        .update("Doc", missing, &row(2, json(r#"{"author":"Turing"}"#), ""))
        .unwrap();
    let tree = catalog.expression_index_btree("Doc", index_id).unwrap();
    assert!(!tree.empty_rids().contains(&missing));
    assert_eq!(
        tree.lookup_all(&Value::Str("Turing".into())),
        vec![moved_from_empty]
    );

    catalog.delete("Doc", spilled).unwrap();
    assert!(catalog
        .expression_index_btree("Doc", index_id)
        .unwrap()
        .lookup_all(&Value::Str("Hopper".into()))
        .is_empty());
    catalog.checkpoint().unwrap();
    drop(catalog);

    let reopened = Catalog::open(dir.path()).unwrap();
    let tree = reopened.expression_index_btree("Doc", index_id).unwrap();
    assert_eq!(
        tree.lookup_all(&Value::Str("Relocated".into())),
        vec![new_rid]
    );
    assert_eq!(
        tree.lookup_all(&Value::Str("Turing".into())),
        vec![moved_from_empty]
    );
    assert!(tree.empty_rids().contains(&null));
}

#[test]
fn unique_and_non_scalar_fail_before_heap_overflow_or_catalog_activation() {
    let duplicate_dir = tempfile::tempdir().unwrap();
    let mut duplicate_catalog = Catalog::create(duplicate_dir.path()).unwrap();
    duplicate_catalog.create_table(schema()).unwrap();
    duplicate_catalog
        .insert("Doc", &row(1, json(r#"{"author":"Ada"}"#), ""))
        .unwrap();
    duplicate_catalog
        .insert("Doc", &row(2, json(r#"{"author":"Ada"}"#), ""))
        .unwrap();
    let path = author_path();
    assert!(duplicate_catalog
        .create_expression_index_metadata("Doc", 1, path.canonical_text(), path.clone(), true,)
        .is_err());
    assert_eq!(
        duplicate_catalog.active_catalog_version(),
        LEGACY_CATALOG_VERSION
    );
    assert_eq!(duplicate_catalog.next_index_id(), 1);
    assert!(!duplicate_dir
        .path()
        .join(expression_index_file_name("Doc", 1))
        .exists());

    let nonscalar_dir = tempfile::tempdir().unwrap();
    let mut nonscalar_catalog = Catalog::create(nonscalar_dir.path()).unwrap();
    nonscalar_catalog.create_table(schema()).unwrap();
    nonscalar_catalog
        .insert("Doc", &row(1, json(r#"{"author":{"name":"Ada"}}"#), ""))
        .unwrap();
    assert!(nonscalar_catalog
        .create_expression_index_metadata("Doc", 1, path.canonical_text(), path.clone(), false)
        .is_err());
    assert_eq!(
        nonscalar_catalog.active_catalog_version(),
        LEGACY_CATALOG_VERSION
    );
    assert!(!nonscalar_dir
        .path()
        .join(expression_index_file_name("Doc", 1))
        .exists());

    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::create(dir.path()).unwrap();
    catalog.create_table(schema()).unwrap();
    let first = catalog
        .insert("Doc", &row(1, json(r#"{"author":"Ada"}"#), ""))
        .unwrap();
    let second = catalog
        .insert("Doc", &row(2, json(r#"{"author":"Grace"}"#), ""))
        .unwrap();
    let index_id = catalog
        .create_expression_index_metadata("Doc", 1, path.canonical_text(), path, true)
        .unwrap();
    assert_eq!(
        catalog.active_catalog_version(),
        EXPRESSION_INDEX_CATALOG_VERSION
    );

    let huge = "x".repeat(20_000);
    assert!(catalog
        .insert(
            "Doc",
            &row(
                3,
                json(&format!(r#"{{"author":"Ada","body":"{huge}"}}"#)),
                "",
            ),
        )
        .is_err());
    assert!(catalog
        .insert(
            "Doc",
            &row(
                4,
                json(&format!(r#"{{"author":{{"name":"New"}},"body":"{huge}"}}"#)),
                "",
            ),
        )
        .is_err());
    assert!(!catalog.table_by_slot(0).has_overflow_rows());
    assert_eq!(catalog.scan("Doc").unwrap().count(), 2);

    assert!(catalog
        .update("Doc", second, &row(2, json(r#"{"author":"Ada"}"#), ""),)
        .is_err());
    assert!(catalog
        .update("Doc", second, &row(2, json(r#"{"author":["bad"]}"#), ""),)
        .is_err());
    assert_eq!(
        catalog.get("Doc", second).unwrap()[1],
        json(r#"{"author":"Grace"}"#)
    );
    let tree = catalog.expression_index_btree("Doc", index_id).unwrap();
    assert_eq!(tree.lookup(&Value::Str("Ada".into())), Some(first));
    assert_eq!(tree.lookup(&Value::Str("Grace".into())), Some(second));
    assert!(catalog
        .patch_var_col_in_place("Doc", first, 1, Some(b"ignored"))
        .is_err());
}

#[test]
fn crash_replay_and_rollback_restore_expression_index_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::create(dir.path()).unwrap();
    catalog.create_table(schema()).unwrap();
    let path = author_path();
    let index_id = catalog
        .create_expression_index_metadata("Doc", 1, path.canonical_text(), path, false)
        .unwrap();
    catalog.checkpoint().unwrap();

    let crash_rid = catalog
        .insert("Doc", &row(1, json(r#"{"author":"Durable"}"#), ""))
        .unwrap();
    catalog.sync_wal().unwrap();
    std::mem::forget(catalog);

    let mut recovered = Catalog::open(dir.path()).unwrap();
    assert_eq!(
        recovered
            .expression_index_btree("Doc", index_id)
            .unwrap()
            .lookup_all(&Value::Str("Durable".into())),
        vec![crash_rid]
    );
    recovered.checkpoint().unwrap();

    let rolled_back = recovered
        .insert("Doc", &row(2, json(r#"{"author":"Transient"}"#), ""))
        .unwrap();
    assert_eq!(
        recovered
            .expression_index_btree("Doc", index_id)
            .unwrap()
            .lookup_all(&Value::Str("Transient".into())),
        vec![rolled_back]
    );
    recovered.rollback_to_last_sync().unwrap();
    assert!(recovered
        .expression_index_btree("Doc", index_id)
        .unwrap()
        .lookup_all(&Value::Str("Transient".into()))
        .is_empty());
    assert_eq!(
        recovered
            .expression_index_btree("Doc", index_id)
            .unwrap()
            .lookup_all(&Value::Str("Durable".into())),
        vec![crash_rid]
    );
}

#[test]
fn explicit_expression_index_drop_removes_metadata_and_file_without_reusing_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::create(dir.path()).unwrap();
    catalog.create_table(schema()).unwrap();
    let path = author_path();
    let index_id = catalog
        .create_expression_index_metadata("Doc", 1, path.canonical_text(), path, false)
        .unwrap();
    let index_file = dir.path().join(expression_index_file_name("Doc", 1));
    assert!(index_file.exists());

    catalog.drop_expression_index("Doc", index_id).unwrap();
    assert!(!index_file.exists());
    assert!(catalog.expression_index_metadata("Doc").unwrap().is_empty());
    assert_eq!(catalog.next_index_id(), 2);
    assert!(catalog
        .expression_index_lookup_all("Doc", index_id, &Value::Int(1))
        .is_err());
    drop(catalog);

    let reopened = Catalog::open(dir.path()).unwrap();
    assert_eq!(
        reopened.active_catalog_version(),
        EXPRESSION_INDEX_CATALOG_VERSION
    );
    assert_eq!(reopened.next_index_id(), 2);
    assert!(reopened
        .expression_index_metadata("Doc")
        .unwrap()
        .is_empty());
}
