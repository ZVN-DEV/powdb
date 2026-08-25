use super::*;

fn fail_next_catalog_persist_at(stage: u8) {
    CATALOG_PERSIST_FAILPOINT.with(|failpoint| failpoint.set(stage));
}

fn temp_catalog(name: &str) -> Catalog {
    let dir = std::env::temp_dir().join(format!("powdb_cat_{name}_{}", std::process::id()));
    Catalog::create(&dir).unwrap()
}

/// Recursively hash every file's path + bytes under `dir` so a test can
/// assert a read-only open leaves the directory byte-identical. Lock
/// artifacts are not created at the catalog layer (only the engine takes a
/// lock), so nothing needs excluding here.
fn hash_dir_tree(dir: &std::path::Path) -> String {
    let mut entries: Vec<std::path::PathBuf> = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let mut items: Vec<_> = fs::read_dir(dir).unwrap().flatten().collect();
        items.sort_by_key(std::fs::DirEntry::path);
        for item in items {
            let path = item.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    walk(dir, &mut entries);
    let mut hasher = crc32fast::Hasher::new();
    for path in &entries {
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update(&fs::read(path).unwrap());
    }
    format!("{:08x}", hasher.finalize())
}

fn seed_quiescent_dir(dir: &std::path::Path) {
    let mut catalog = Catalog::create(dir).unwrap();
    catalog
        .create_table(Schema {
            table_name: "User".into(),
            columns: vec![
                ColumnDef {
                    name: "name".into(),
                    type_id: TypeId::Str,
                    required: true,
                    position: 0,
                },
                ColumnDef {
                    name: "age".into(),
                    type_id: TypeId::Int,
                    required: false,
                    position: 1,
                },
            ],
        })
        .unwrap();
    catalog.create_index("User", "age").unwrap();
    catalog
        .insert("User", &vec![Value::Str("Ada".into()), Value::Int(36)])
        .unwrap();
    catalog
        .insert("User", &vec![Value::Str("Bo".into()), Value::Int(20)])
        .unwrap();
    // Clean drop checkpoints: flush heaps + truncate the WAL, leaving a
    // quiescent (WAL-clean) directory.
    drop(catalog);
}

#[test]
fn open_read_only_serves_reads_on_clean_dir() {
    let dir = tempfile::tempdir().unwrap();
    seed_quiescent_dir(dir.path());

    let catalog = Catalog::open_read_only(dir.path()).unwrap();
    let rows: Vec<_> = catalog.scan("User").unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(rows.len(), 2);
    // Column-index read works read-only.
    let hit = catalog
        .index_lookup("User", "age", &Value::Int(36))
        .unwrap();
    assert_eq!(hit.unwrap()[0], Value::Str("Ada".into()));
}

#[test]
fn open_read_only_never_mutates_dir() {
    let dir = tempfile::tempdir().unwrap();
    seed_quiescent_dir(dir.path());
    let before = hash_dir_tree(dir.path());

    {
        let catalog = Catalog::open_read_only(dir.path()).unwrap();
        let _ = catalog
            .scan("User")
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap()
            .len();
        let _ = catalog
            .index_lookup("User", "age", &Value::Int(20))
            .unwrap();
        // Drop the read-only catalog: must not checkpoint/truncate.
    }
    let after = hash_dir_tree(dir.path());
    assert_eq!(
        before, after,
        "read-only open + queries + drop must leave the directory byte-identical"
    );
}

#[test]
fn open_read_only_refuses_non_empty_wal() {
    let dir = tempfile::tempdir().unwrap();
    // Seed rows but DO NOT checkpoint: keep the WAL non-empty by forgetting
    // the catalog (a crash), so recovery would be required.
    {
        let mut catalog = Catalog::create(dir.path()).unwrap();
        catalog
            .create_table(Schema {
                table_name: "T".into(),
                columns: vec![ColumnDef {
                    name: "id".into(),
                    type_id: TypeId::Int,
                    required: true,
                    position: 0,
                }],
            })
            .unwrap();
        catalog.insert("T", &vec![Value::Int(1)]).unwrap();
        catalog.sync_wal().unwrap();
        std::mem::forget(catalog); // leave the WAL non-empty, as a crash would
    }
    let err = match Catalog::open_read_only(dir.path()) {
        Ok(_) => panic!("read-only open must refuse a non-empty WAL"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("WAL is not empty"),
        "expected a WAL-not-empty refusal naming the remedy, got: {err}"
    );
    assert!(err.to_string().contains("read-write engine"));
}

#[test]
fn open_read_only_expression_index_reads_work() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut catalog = Catalog::create(dir.path()).unwrap();
        catalog
            .create_table(Schema {
                table_name: "Doc".into(),
                columns: vec![ColumnDef {
                    name: "data".into(),
                    type_id: TypeId::Json,
                    required: false,
                    position: 0,
                }],
            })
            .unwrap();
        let path =
            StoredJsonPathV1::new("data", vec![StoredJsonPathSegmentV1::Key("author".into())]);
        catalog
            .create_expression_index_metadata("Doc", 1, path.canonical_text(), path, false)
            .unwrap();
        drop(catalog);
    }
    // Opening read-only must load the expression index without writing.
    let before = hash_dir_tree(dir.path());
    let catalog = Catalog::open_read_only(dir.path()).unwrap();
    assert_eq!(
        catalog
            .scan("Doc")
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap()
            .len(),
        0
    );
    drop(catalog);
    let after = hash_dir_tree(dir.path());
    assert_eq!(
        before, after,
        "read-only expression-index load must not write"
    );
}

#[test]
fn v5_reader_rejects_v6_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::create(dir.path()).unwrap();
    catalog
        .create_table(Schema {
            table_name: "Doc".into(),
            columns: vec![ColumnDef {
                name: "data".into(),
                type_id: TypeId::Json,
                required: false,
                position: 0,
            }],
        })
        .unwrap();
    let path = StoredJsonPathV1::new("data", vec![StoredJsonPathSegmentV1::Key("author".into())]);
    catalog
        .create_expression_index_metadata("Doc", 1, path.canonical_text(), path, false)
        .unwrap();
    let result =
        read_catalog_file_with_max_version(&dir.path().join(CATALOG_FILE), LEGACY_CATALOG_VERSION);
    let error = match result {
        Ok(_) => panic!("a v5 reader must reject v6 before decoding its payload"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("unsupported catalog version: 6"));
}

/// A v7 catalog (one that declared a link) must be refused by a reader
/// capped at v6, with the same "unsupported catalog version" error the
/// version gate already produces — not a crash, not silent corruption.
#[test]
fn v6_reader_rejects_v7_catalog() {
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
    catalog
        .create_link(LinkDef {
            owner_type: "Order".into(),
            name: "user".into(),
            target_type: "User".into(),
            local_key: "user_id".into(),
            target_key: "id".into(),
            kind: LinkKind::ToMany,
        })
        .unwrap();
    assert_eq!(catalog.active_catalog_version(), CATALOG_VERSION);

    let result = read_catalog_file_with_max_version(
        &dir.path().join(CATALOG_FILE),
        EXPRESSION_INDEX_CATALOG_VERSION,
    );
    let error = match result {
        Ok(_) => panic!("a v6 reader must reject v7 before decoding its payload"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("unsupported catalog version: 7"));
}

#[test]
fn expression_index_rolls_back_only_before_catalog_rename() {
    let before_dir = tempfile::tempdir().unwrap();
    let mut before = Catalog::create(before_dir.path()).unwrap();
    before
        .create_table(Schema {
            table_name: "Doc".into(),
            columns: vec![ColumnDef {
                name: "data".into(),
                type_id: TypeId::Json,
                required: false,
                position: 0,
            }],
        })
        .unwrap();
    let path = StoredJsonPathV1::new("data", vec![StoredJsonPathSegmentV1::Key("score".into())]);

    fail_next_catalog_persist_at(1);
    let error = before
        .create_expression_index_metadata("Doc", 1, path.canonical_text(), path.clone(), false)
        .unwrap_err();
    assert!(error.to_string().contains("before rename"));
    assert_eq!(before.active_catalog_version(), LEGACY_CATALOG_VERSION);
    assert_eq!(before.next_index_id(), 1);
    assert!(before.expression_index_metadata("Doc").unwrap().is_empty());
    assert!(!before_dir
        .path()
        .join(expression_index_file_name("Doc", 1))
        .exists());

    let before_index_id = before
        .create_expression_index_metadata("Doc", 1, path.canonical_text(), path.clone(), false)
        .unwrap();
    fail_next_catalog_persist_at(1);
    let error = before
        .drop_expression_index("Doc", before_index_id)
        .unwrap_err();
    assert!(error.to_string().contains("before rename"));
    assert!(before
        .expression_index_btree("Doc", before_index_id)
        .is_some());
    assert!(before_dir
        .path()
        .join(expression_index_file_name("Doc", 1))
        .exists());
    std::mem::forget(before);
    let before_reopened = Catalog::open(before_dir.path()).unwrap();
    assert!(before_reopened
        .expression_index_btree("Doc", before_index_id)
        .is_some());

    let after_dir = tempfile::tempdir().unwrap();
    let mut after = Catalog::create(after_dir.path()).unwrap();
    after
        .create_table(Schema {
            table_name: "Doc".into(),
            columns: vec![ColumnDef {
                name: "data".into(),
                type_id: TypeId::Json,
                required: false,
                position: 0,
            }],
        })
        .unwrap();
    fail_next_catalog_persist_at(2);
    let index_id = after
        .create_expression_index_metadata("Doc", 1, path.canonical_text(), path.clone(), false)
        .unwrap();
    assert_eq!(index_id, 1);
    assert_eq!(
        after.active_catalog_version(),
        EXPRESSION_INDEX_CATALOG_VERSION
    );
    assert_eq!(after.next_index_id(), 2);
    assert!(after.expression_index_btree("Doc", index_id).is_some());
    assert!(after_dir
        .path()
        .join(expression_index_file_name("Doc", 1))
        .exists());
    std::mem::forget(after);

    let mut reopened = Catalog::open(after_dir.path()).unwrap();
    assert!(reopened.expression_index_btree("Doc", index_id).is_some());
    fail_next_catalog_persist_at(2);
    reopened.drop_expression_index("Doc", index_id).unwrap();
    assert!(reopened
        .expression_index_metadata("Doc")
        .unwrap()
        .is_empty());
    assert!(!after_dir
        .path()
        .join(expression_index_file_name("Doc", 1))
        .exists());
    std::mem::forget(reopened);

    let final_open = Catalog::open(after_dir.path()).unwrap();
    assert!(final_open
        .expression_index_metadata("Doc")
        .unwrap()
        .is_empty());
}

/// A pre-rename persist failure during the *first* `create_link` must revert
/// the format version 7 -> 6/5, leave the in-memory registry empty, and
/// leave no v7 catalog on disk (mirrors the expression-index rollback test).
#[test]
fn first_create_link_rolls_back_version_and_registry_before_rename() {
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
    catalog.create_index_unique("User", "id", true).unwrap();

    let version_before = catalog.active_catalog_version();
    assert_eq!(version_before, LEGACY_CATALOG_VERSION);

    fail_next_catalog_persist_at(1);
    let error = catalog
        .create_link(LinkDef {
            owner_type: "Order".into(),
            name: "user".into(),
            target_type: "User".into(),
            local_key: "user_id".into(),
            target_key: "id".into(),
            kind: LinkKind::ToMany,
        })
        .unwrap_err();
    assert!(error.to_string().contains("before rename"));
    // Version and registry reverted; nothing persisted.
    assert_eq!(catalog.active_catalog_version(), version_before);
    assert!(catalog.link("Order", "user").is_none());
    assert_eq!(catalog.links().count(), 0);
    assert_eq!(
        read_active_catalog_version(dir.path()).unwrap(),
        version_before
    );

    // A subsequent clean create_link succeeds and derives ToOne (unique id).
    catalog
        .create_link(LinkDef {
            owner_type: "Order".into(),
            name: "user".into(),
            target_type: "User".into(),
            local_key: "user_id".into(),
            target_key: "id".into(),
            kind: LinkKind::ToMany, // ignored; derived from uniqueness
        })
        .unwrap();
    assert_eq!(catalog.active_catalog_version(), CATALOG_VERSION);
    assert_eq!(catalog.link("Order", "user").unwrap().kind, LinkKind::ToOne);
}

#[test]
fn ordinary_catalog_persist_reports_post_rename_directory_sync_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::create(dir.path()).unwrap();
    fail_next_catalog_persist_at(2);
    let error = catalog
        .create_table(Schema {
            table_name: "VisibleAfterRename".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            }],
        })
        .unwrap_err();
    assert!(error.to_string().contains("after rename"));
    assert!(catalog.schema("VisibleAfterRename").is_some());

    std::mem::forget(catalog);
    let reopened = Catalog::open(dir.path()).unwrap();
    assert!(reopened.schema("VisibleAfterRename").is_some());
}

fn schema_two_cols() -> Schema {
    Schema {
        table_name: "T".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            },
            ColumnDef {
                name: "status".into(),
                type_id: TypeId::Str,
                required: false,
                position: 1,
            },
        ],
    }
}

#[test]
fn replay_records_treats_reused_tx_ids_as_ordered_spans() {
    let mut cat = temp_catalog("reused_tx_ids");
    let schema = schema_two_cols();
    cat.create_table(schema.clone()).unwrap();
    cat.checkpoint().unwrap();

    let mut committed_row = Vec::new();
    encode_row_into(
        &schema,
        &[Value::Int(1), Value::Str("committed".into())],
        &mut committed_row,
    );
    let mut incomplete_row = Vec::new();
    encode_row_into(
        &schema,
        &[Value::Int(2), Value::Str("incomplete".into())],
        &mut incomplete_row,
    );

    let records = vec![
        WalRecord {
            tx_id: 1,
            record_type: WalRecordType::Begin,
            lsn: 1,
            data: Vec::new(),
        },
        WalRecord {
            tx_id: 1,
            record_type: WalRecordType::Insert,
            lsn: 2,
            data: encode_wal_payload(
                "T",
                RowId {
                    page_id: 1,
                    slot_index: 0,
                },
                &committed_row,
            ),
        },
        WalRecord {
            tx_id: 1,
            record_type: WalRecordType::Commit,
            lsn: 3,
            data: Vec::new(),
        },
        WalRecord {
            tx_id: 1,
            record_type: WalRecordType::Begin,
            lsn: 4,
            data: Vec::new(),
        },
        WalRecord {
            tx_id: 1,
            record_type: WalRecordType::Insert,
            lsn: 5,
            data: encode_wal_payload(
                "T",
                RowId {
                    page_id: 1,
                    slot_index: 1,
                },
                &incomplete_row,
            ),
        },
    ];

    cat.apply_wal_records(&records).unwrap();
    let rows: Vec<_> = cat.scan("T").unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1[0], Value::Int(1));
    assert_eq!(rows[0].1[1], Value::Str("committed".into()));
}

#[test]
fn ddl_create_table_codec_roundtrips_defaults_and_auto() {
    let schema = schema_two_cols();
    let defaults = vec![None, Some(Value::Str("active".into()))];
    let auto_cols = vec![true, false];
    let encoded = encode_ddl_create_table(&schema, &defaults, &auto_cols);
    let (decoded_schema, decoded_defaults, decoded_auto) =
        decode_ddl_create_table(&encoded).unwrap();
    assert_eq!(decoded_schema.columns.len(), 2);
    assert_eq!(decoded_defaults, defaults);
    assert_eq!(decoded_auto, auto_cols);
}

#[test]
fn ddl_create_table_codec_back_compat_without_trailing_sections() {
    // Simulate a record written before column defaults / auto existed: the
    // old encoder stopped right after the columns, with no trailing
    // sections. The new decoder must read those as "none".
    let schema = schema_two_cols();
    let full = encode_ddl_create_table(&schema, &[], &[]);
    // Each empty trailing section is a u16 count of 0 (two bytes); chop
    // both off to mimic the pre-feature on-disk shape.
    let legacy = &full[..full.len() - 4];
    let (decoded_schema, decoded_defaults, decoded_auto) = decode_ddl_create_table(legacy).unwrap();
    assert_eq!(decoded_schema.columns.len(), 2);
    assert!(decoded_defaults.is_empty(), "no defaults section -> empty");
    assert!(decoded_auto.is_empty(), "no auto section -> empty");
}

#[test]
fn ddl_create_table_codec_back_compat_defaults_but_no_auto() {
    // A record from the column-defaults release (#129) has a defaults
    // section but no auto section; the auto-aware decoder must still read it.
    let schema = schema_two_cols();
    let defaults = vec![None, Some(Value::Str("active".into()))];
    let full = encode_ddl_create_table(&schema, &defaults, &[]);
    // Drop only the trailing auto section (its empty u16 count).
    let legacy = &full[..full.len() - 2];
    let (_schema, decoded_defaults, decoded_auto) = decode_ddl_create_table(legacy).unwrap();
    assert_eq!(decoded_defaults, defaults);
    assert!(decoded_auto.is_empty());
}

#[test]
fn read_catalog_file_accepts_intermediate_versions_3_and_4() {
    // Regression: the version gate accepted only {1, 2, CATALOG_VERSION}, so
    // a catalog written at version 3 (v0.6.x) or 4 (the column-defaults
    // release) was rejected with "unsupported catalog version" — the
    // database would fail to open on upgrade from those releases = data
    // loss. The field-reading staircase already handles v3/v4; only the gate
    // was stale. Build faithful v3/v4 catalog files by hand and confirm they
    // load (defaults/auto default to empty for the versions that lack them).
    use std::io::Write as _;
    fn write_legacy_catalog(path: &std::path::Path, version: u16) {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(CATALOG_MAGIC);
        buf.extend_from_slice(&version.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes()); // n_tables
                                                    // table "T"
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(b"T");
        buf.extend_from_slice(&2u16.to_le_bytes()); // n_cols
                                                    // col id: Int, required, pos 0
        buf.extend_from_slice(&2u32.to_le_bytes());
        buf.extend_from_slice(b"id");
        buf.push(TypeId::Int as u8);
        buf.push(1);
        buf.extend_from_slice(&0u16.to_le_bytes());
        // col status: Str, not required, pos 1
        buf.extend_from_slice(&6u32.to_le_bytes());
        buf.extend_from_slice(b"status");
        buf.push(TypeId::Str as u8);
        buf.push(0);
        buf.extend_from_slice(&1u16.to_le_bytes());
        // version >= 3: indexed-column section (count 0).
        buf.extend_from_slice(&0u16.to_le_bytes());
        // version >= 4: column-defaults section (none here). v3 omits it.
        if version >= 4 {
            encode_defaults_section(&mut buf, &[None, None]);
        }
        // v3/v4 never wrote the v5 auto section.
        let crc = crc32fast::hash(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        let mut f = fs::File::create(path).unwrap();
        f.write_all(&buf).unwrap();
    }

    for version in [3u16, 4u16] {
        let path = std::env::temp_dir().join(format!(
            "powdb_cat_v{version}_compat_{}.bin",
            std::process::id()
        ));
        write_legacy_catalog(&path, version);
        let catalog_file = read_catalog_file(&path)
            .unwrap_or_else(|e| panic!("version {version} catalog must load, got: {e}"));
        let entries = catalog_file.entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].schema.table_name, "T");
        assert_eq!(entries[0].schema.columns.len(), 2);
        assert!(
            entries[0].auto_cols.is_empty(),
            "v{version} has no auto cols"
        );
        fs::remove_file(&path).ok();
    }
}

#[test]
fn read_catalog_file_rejects_implausible_table_count() {
    // A corrupt/hostile catalog must not be trusted to size an allocation:
    // `Vec::with_capacity(n_tables)` on an unvalidated u32 would attempt a
    // huge allocation and abort the host. A file can describe at most as
    // many tables as it has bytes, so a count exceeding the payload length
    // is rejected with a clear error before any allocation. (We use a small
    // implausible count over a tiny buffer; a genuinely huge count would
    // abort the test runner pre-fix, but it hits the very same guard.)
    use std::io::Write as _;
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(CATALOG_MAGIC);
    buf.extend_from_slice(&CATALOG_VERSION.to_le_bytes());
    buf.extend_from_slice(&1000u32.to_le_bytes()); // claims 1000 tables…
    buf.extend_from_slice(&1u64.to_le_bytes()); // valid v6 next-index id
                                                // …but no table data follows.
    let crc = crc32fast::hash(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());
    let path = std::env::temp_dir().join(format!("powdb_cat_badcount_{}.bin", std::process::id()));
    fs::File::create(&path).unwrap().write_all(&buf).unwrap();

    let msg = match read_catalog_file(&path) {
        Ok(_) => panic!("implausible table count must be rejected, got Ok"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("implausible table count"),
        "expected an implausible-table-count error, got: {msg}"
    );
    fs::remove_file(&path).ok();
}

#[test]
fn data_dir_and_max_lsn_accessors() {
    let dir = std::env::temp_dir().join(format!("powdb_cat_maxlsn_{}", std::process::id()));
    let mut cat = Catalog::create(&dir).unwrap();

    // data_dir() reflects the directory the catalog was created in.
    assert_eq!(cat.data_dir(), dir.as_path());

    // A fresh catalog has stamped no page LSNs yet.
    assert_eq!(cat.max_lsn(), 0);

    let schema = Schema {
        table_name: "users".into(),
        columns: vec![ColumnDef {
            name: "name".into(),
            type_id: TypeId::Str,
            required: true,
            position: 0,
        }],
    };
    cat.create_table(schema).unwrap();

    cat.insert("users", &vec![Value::Str("Alice".into())])
        .unwrap();
    cat.sync_wal().unwrap();

    // An inserted (and synced) row stamps a page LSN, raising the
    // durability high-water mark above zero.
    assert!(cat.max_lsn() > 0);
}

#[test]
fn test_create_table_and_insert() {
    let mut cat = temp_catalog("basic");
    let schema = Schema {
        table_name: "users".into(),
        columns: vec![
            ColumnDef {
                name: "name".into(),
                type_id: TypeId::Str,
                required: true,
                position: 0,
            },
            ColumnDef {
                name: "age".into(),
                type_id: TypeId::Int,
                required: false,
                position: 1,
            },
        ],
    };
    cat.create_table(schema).unwrap();

    let row = vec![Value::Str("Alice".into()), Value::Int(30)];
    let rid = cat.insert("users", &row).unwrap();

    let result = cat.get("users", rid).unwrap();
    assert_eq!(result[0], Value::Str("Alice".into()));
    assert_eq!(result[1], Value::Int(30));
}

#[test]
fn test_scan_table() {
    let mut cat = temp_catalog("scan");
    let schema = Schema {
        table_name: "items".into(),
        columns: vec![
            ColumnDef {
                name: "name".into(),
                type_id: TypeId::Str,
                required: true,
                position: 0,
            },
            ColumnDef {
                name: "price".into(),
                type_id: TypeId::Float,
                required: true,
                position: 1,
            },
        ],
    };
    cat.create_table(schema).unwrap();

    for i in 0..50 {
        cat.insert(
            "items",
            &vec![
                Value::Str(format!("item_{i}")),
                Value::Float(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }

    let rows: Vec<_> = cat.scan("items").unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(rows.len(), 50);
}

#[test]
fn test_index_lookup() {
    let mut cat = temp_catalog("idx");
    let schema = Schema {
        table_name: "users".into(),
        columns: vec![
            ColumnDef {
                name: "email".into(),
                type_id: TypeId::Str,
                required: true,
                position: 0,
            },
            ColumnDef {
                name: "name".into(),
                type_id: TypeId::Str,
                required: true,
                position: 1,
            },
        ],
    };
    cat.create_table(schema).unwrap();
    cat.create_index("users", "email").unwrap();

    cat.insert(
        "users",
        &vec![
            Value::Str("alice@example.com".into()),
            Value::Str("Alice".into()),
        ],
    )
    .unwrap();
    cat.insert(
        "users",
        &vec![
            Value::Str("bob@example.com".into()),
            Value::Str("Bob".into()),
        ],
    )
    .unwrap();

    let result = cat
        .index_lookup("users", "email", &Value::Str("bob@example.com".into()))
        .unwrap();
    assert!(result.is_some());
    let row = result.unwrap();
    assert_eq!(row[1], Value::Str("Bob".into()));
}

#[test]
fn test_delete_row() {
    let mut cat = temp_catalog("delete");
    let schema = Schema {
        table_name: "t".into(),
        columns: vec![ColumnDef {
            name: "v".into(),
            type_id: TypeId::Int,
            required: true,
            position: 0,
        }],
    };
    cat.create_table(schema).unwrap();
    let r1 = cat.insert("t", &vec![Value::Int(1)]).unwrap();
    let r2 = cat.insert("t", &vec![Value::Int(2)]).unwrap();
    cat.delete("t", r1).unwrap();
    assert!(cat.get("t", r1).is_none());
    assert!(cat.get("t", r2).is_some());
}

#[test]
fn test_update_row() {
    let mut cat = temp_catalog("update");
    let schema = Schema {
        table_name: "t".into(),
        columns: vec![ColumnDef {
            name: "v".into(),
            type_id: TypeId::Int,
            required: true,
            position: 0,
        }],
    };
    cat.create_table(schema).unwrap();
    let rid = cat.insert("t", &vec![Value::Int(1)]).unwrap();
    let new_rid = cat.update("t", rid, &vec![Value::Int(99)]).unwrap();
    let row = cat.get("t", new_rid).unwrap();
    assert_eq!(row[0], Value::Int(99));
}

#[test]
fn test_persist_and_reopen() {
    let dir = std::env::temp_dir().join(format!("powdb_cat_persist_{}", std::process::id()));
    // Fresh dir
    let _ = std::fs::remove_dir_all(&dir);

    {
        let mut cat = Catalog::create(&dir).unwrap();
        cat.create_table(Schema {
            table_name: "users".into(),
            columns: vec![
                ColumnDef {
                    name: "name".into(),
                    type_id: TypeId::Str,
                    required: true,
                    position: 0,
                },
                ColumnDef {
                    name: "age".into(),
                    type_id: TypeId::Int,
                    required: false,
                    position: 1,
                },
            ],
        })
        .unwrap();
        cat.insert("users", &vec![Value::Str("Alice".into()), Value::Int(30)])
            .unwrap();
        cat.insert("users", &vec![Value::Str("Bob".into()), Value::Int(25)])
            .unwrap();
    }

    // Reopen — schema and rows should both still be there
    let cat = Catalog::open(&dir).unwrap();
    let schema = cat.schema("users").unwrap();
    assert_eq!(schema.columns.len(), 2);
    assert_eq!(schema.columns[0].name, "name");
    assert_eq!(schema.columns[0].type_id, TypeId::Str);
    assert_eq!(schema.columns[1].type_id, TypeId::Int);

    let rows: Vec<_> = cat.scan("users").unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(rows.len(), 2);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_open_missing_dir_errors() {
    let dir = std::env::temp_dir().join(format!("powdb_cat_missing_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // No catalog.bin yet
    assert!(Catalog::open(&dir).is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_list_tables() {
    let mut cat = temp_catalog("list");
    cat.create_table(Schema {
        table_name: "a".into(),
        columns: vec![ColumnDef {
            name: "x".into(),
            type_id: TypeId::Int,
            required: true,
            position: 0,
        }],
    })
    .unwrap();
    cat.create_table(Schema {
        table_name: "b".into(),
        columns: vec![ColumnDef {
            name: "y".into(),
            type_id: TypeId::Int,
            required: true,
            position: 0,
        }],
    })
    .unwrap();
    let mut tables = cat.list_tables();
    tables.sort();
    assert_eq!(tables, vec!["a", "b"]);
}

#[test]
fn test_path_traversal_table_name_rejected() {
    let mut cat = temp_catalog("path_trav");
    // Names with path separators must be rejected.
    let bad_names = vec![
        "../etc/passwd",
        "foo/bar",
        "table\0name",
        "",
        "123starts_with_digit",
        "has-dashes",
        "has spaces",
        "has.dots",
    ];
    for name in bad_names {
        let schema = Schema {
            table_name: name.into(),
            columns: vec![ColumnDef {
                name: "x".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            }],
        };
        let result = cat.create_table(schema);
        assert!(result.is_err(), "expected error for table name '{name}'");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
    // Valid names must still work.
    let good_names = vec!["users", "_private", "Table_123", "_"];
    for name in good_names {
        let schema = Schema {
            table_name: name.into(),
            columns: vec![ColumnDef {
                name: "x".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            }],
        };
        assert!(
            cat.create_table(schema).is_ok(),
            "expected ok for table name '{name}'"
        );
    }
}

#[test]
fn test_path_traversal_column_name_rejected() {
    let mut cat = temp_catalog("col_path_trav");
    let schema = Schema {
        table_name: "valid_table".into(),
        columns: vec![ColumnDef {
            name: "../bad".into(),
            type_id: TypeId::Int,
            required: true,
            position: 0,
        }],
    };
    let result = cat.create_table(schema);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn test_drop_table_validates_name() {
    let mut cat = temp_catalog("drop_trav");
    let result = cat.drop_table("../etc/passwd");
    assert!(result.is_err());
    // Should fail with InvalidInput (validation), not NotFound.
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
}

/// Two-column table used by the DDL-in-transaction refusal tests.
fn ddl_guard_schema(name: &str) -> Schema {
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
                name: "label".into(),
                type_id: TypeId::Str,
                required: false,
                position: 1,
            },
        ],
    }
}

fn seed_ddl_guard_catalog(dir: &std::path::Path) -> Catalog {
    let mut cat = Catalog::create(dir).unwrap();
    cat.create_table(ddl_guard_schema("Keep")).unwrap();
    cat.create_index_unique("Keep", "id", true).unwrap();
    cat.insert("Keep", &vec![Value::Int(1), Value::Str("one".into())])
        .unwrap();
    cat.sync_wal().unwrap();
    cat
}

fn assert_refused_in_transaction(result: io::Result<()>, verb: &str) {
    let err = result.unwrap_err();
    assert_eq!(
        err.kind(),
        io::ErrorKind::InvalidInput,
        "{verb} inside a transaction must be refused as InvalidInput"
    );
    let message = err.to_string();
    assert!(
        message.starts_with("cannot ") && message.contains("explicit transaction"),
        "{verb} refusal must name the active transaction, got: {message}"
    );
}

#[test]
fn drop_table_inside_transaction_is_refused_and_rollback_keeps_data() {
    let dir = tempfile::tempdir().unwrap();
    let mut cat = seed_ddl_guard_catalog(dir.path());
    // Checkpoint first so the WAL no longer holds the records that created
    // and populated `Keep`: rollback then has nothing to replay and the
    // table can only survive if the DROP was refused outright.
    cat.checkpoint().unwrap();

    cat.begin_transaction().unwrap();
    assert_refused_in_transaction(cat.drop_table("Keep"), "drop table");
    cat.rollback_to_last_sync().unwrap();

    let rows: Vec<_> = cat.scan("Keep").unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(rows.len(), 1, "rolled-back DROP must not destroy data");
    assert_eq!(rows[0].1[0], Value::Int(1));

    // The heap file itself must still be there for the next open.
    drop(cat);
    let reopened = Catalog::open(dir.path()).unwrap();
    assert_eq!(
        reopened
            .scan("Keep")
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn every_ddl_verb_inside_transaction_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut cat = seed_ddl_guard_catalog(dir.path());
    cat.create_table(ddl_guard_schema("Other")).unwrap();
    cat.create_index_unique("Other", "id", true).unwrap();
    cat.checkpoint().unwrap();

    cat.begin_transaction().unwrap();

    assert_refused_in_transaction(cat.create_table(ddl_guard_schema("New")), "create table");
    assert_refused_in_transaction(cat.drop_table("Keep"), "drop table");
    assert_refused_in_transaction(
        cat.alter_table_add_column(
            "Keep",
            ColumnDef {
                name: "extra".into(),
                type_id: TypeId::Int,
                required: false,
                position: 2,
            },
        ),
        "alter table add column",
    );
    assert_refused_in_transaction(
        cat.alter_table_drop_column("Keep", "label"),
        "alter table drop column",
    );
    assert_refused_in_transaction(cat.create_index("Keep", "label"), "create index");
    assert_refused_in_transaction(
        cat.create_index_unique("Keep", "label", true),
        "create unique index",
    );
    assert_refused_in_transaction(
        cat.create_link(LinkDef {
            owner_type: "Other".into(),
            name: "keep".into(),
            target_type: "Keep".into(),
            local_key: "id".into(),
            target_key: "id".into(),
            kind: LinkKind::ToOne,
        }),
        "create link",
    );
    assert_refused_in_transaction(cat.drop_link("Other", "keep"), "drop link");

    cat.rollback_to_last_sync().unwrap();

    // Every refused verb left the catalog exactly as it was.
    let mut tables = cat.list_tables();
    tables.sort_unstable();
    assert_eq!(tables, vec!["Keep", "Other"]);
    let schema = cat.schema("Keep").unwrap();
    assert_eq!(schema.columns.len(), 2);
    assert!(!cat.has_index("Keep", "label"));
    assert!(cat.links().next().is_none());
    assert_eq!(
        cat.scan("Keep")
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn ddl_still_works_after_commit_and_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let mut cat = seed_ddl_guard_catalog(dir.path());

    cat.begin_transaction().unwrap();
    assert!(cat.drop_table("Keep").is_err());
    cat.rollback_to_last_sync().unwrap();
    cat.create_table(ddl_guard_schema("AfterRollback")).unwrap();

    cat.begin_transaction().unwrap();
    cat.insert("Keep", &vec![Value::Int(2), Value::Str("two".into())])
        .unwrap();
    cat.commit_transaction().unwrap();
    cat.drop_table("AfterRollback").unwrap();

    let mut tables = cat.list_tables();
    tables.sort_unstable();
    assert_eq!(tables, vec!["Keep"]);
    assert_eq!(
        cat.scan("Keep")
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn transaction_over_dirty_page_budget_is_refused_and_catalog_stays_usable() {
    let dir = tempfile::tempdir().unwrap();
    let mut cat = Catalog::create(dir.path()).unwrap();
    cat.create_table(ddl_guard_schema("Big")).unwrap();
    // 8 pages across every table: small enough to trip in a few hundred
    // rows, large enough that the first insert still fits.
    cat.set_dirty_page_budget_bytes(8 * crate::page::PAGE_SIZE);
    assert_eq!(cat.dirty_page_budget_bytes(), 8 * crate::page::PAGE_SIZE);

    cat.begin_transaction().unwrap();
    let mut refusal = None;
    for i in 0..100_000i64 {
        let row = vec![Value::Int(i), Value::Str(format!("row-{i:06}"))];
        if let Err(e) = cat.insert("Big", &row) {
            refusal = Some(e);
            break;
        }
    }
    let err = refusal.expect("an 8-page budget must refuse an unbounded transaction");
    let typed = err
        .get_ref()
        .and_then(|source| source.downcast_ref::<StorageError>());
    assert!(
        matches!(typed, Some(StorageError::TransactionTooLarge { .. })),
        "expected a typed TransactionTooLarge, got: {err}"
    );
    assert!(cat.dirty_pages_buffered() <= 8);

    // The refusal is not fatal: the connection rolls back and keeps working.
    cat.rollback_to_last_sync().unwrap();
    assert_eq!(cat.dirty_page_budget_bytes(), 8 * crate::page::PAGE_SIZE);
    assert_eq!(
        cat.scan("Big")
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap()
            .len(),
        0
    );
    cat.insert("Big", &vec![Value::Int(1), Value::Str("after".into())])
        .unwrap();
    assert_eq!(
        cat.scan("Big")
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap()
            .len(),
        1
    );
}

/// `drop_table` must write the catalog before it unlinks the heap.
///
/// The window matters because `Catalog::open` opens every heap named by the
/// on-disk catalog *before* it replays the WAL: a catalog that still names a
/// table whose heap file is already gone does not degrade; it refuses to
/// open at all, and the `DdlDropTable` record that would have finished the
/// job never gets read. A persist failure is the observable stand-in for a
/// crash in that window, with the catalog written first, the heap is still
/// on disk and the database still opens.
#[test]
fn drop_table_persists_the_catalog_before_unlinking_the_heap() {
    let dir = tempfile::tempdir().unwrap();
    let mut cat = Catalog::create(dir.path()).unwrap();
    cat.create_table(ddl_guard_schema("Gone")).unwrap();
    cat.insert("Gone", &vec![Value::Int(1), Value::Str("row".into())])
        .unwrap();
    cat.checkpoint().unwrap();

    let heap_path = dir.path().join("Gone.heap");
    assert!(heap_path.exists());

    fail_next_catalog_persist_at(1);
    let error = cat.drop_table("Gone").unwrap_err();
    assert!(
        error.to_string().contains("before rename"),
        "expected the injected pre-rename persist failure, got: {error}"
    );
    assert!(
        heap_path.exists(),
        "the heap must not be unlinked until the catalog no longer names it"
    );

    std::mem::forget(cat);
    // The drop's intent was logged and flushed before any of this, so
    // recovery finishes it; the point of the assertion is that the open
    // gets far enough to replay at all.
    let reopened = Catalog::open(dir.path()).unwrap();
    assert!(reopened.schema("Gone").is_none());
    assert!(!heap_path.exists());
}

#[test]
fn autocommit_writes_are_not_capped_by_the_dirty_page_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut cat = Catalog::create(dir.path()).unwrap();
    cat.create_table(ddl_guard_schema("Bulk")).unwrap();
    cat.set_dirty_page_budget_bytes(8 * crate::page::PAGE_SIZE);

    // Nothing pins the buffer for ROLLBACK here, so the budget is relieved
    // by writing pages out rather than by failing the statement.
    for i in 0..5_000i64 {
        let row = vec![Value::Int(i), Value::Str(format!("row-{i:06}"))];
        cat.insert("Bulk", &row).unwrap();
    }
    assert!(cat.dirty_pages_buffered() <= 8);
    assert_eq!(
        cat.scan("Bulk")
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap()
            .len(),
        5_000
    );
}
