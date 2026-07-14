//! Storage lifecycle for the `Json` column type (design 2026-07-13, section 4).
//! Json values are variable-length canonical PJ1 bytes, so they ride the exact
//! v0.11 overflow machinery Str/Bytes use: small documents store inline, large
//! ones (> 4070 B) spill through an overflow chain. This suite exercises
//! create -> insert (inline + spilled) -> read-back byte-exact -> update ->
//! delete -> crash-recover, mirroring `overflow_lifecycle.rs`.

use powdb_storage::catalog::Catalog;
use powdb_storage::pj1::{parse_json_text, pj1_to_text};
use powdb_storage::types::{ColumnDef, RowId, Schema, TypeId, Value};

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_jsonlife_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// `type docs { unique auto id: int, body: json }`
fn docs_schema() -> Schema {
    Schema {
        table_name: "docs".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                type_id: TypeId::Int,
                required: true,
                position: 0,
            },
            ColumnDef {
                name: "body".into(),
                type_id: TypeId::Json,
                required: true,
                position: 1,
            },
        ],
    }
}

/// Canonical PJ1 for a JSON text (panics if the text is invalid).
fn json(text: &str) -> Value {
    Value::Json(parse_json_text(text).expect("valid json").into())
}

/// Extract the PJ1 bytes of a row's `body` column.
fn body_bytes(row: &[Value]) -> Vec<u8> {
    match &row[1] {
        Value::Json(b) => b.to_vec(),
        other => panic!("expected Json, got {other:?}"),
    }
}

/// A JSON object whose canonical encoding comfortably exceeds the 4070 B inline
/// cap, forcing an overflow spill. A long string value does it.
fn big_json_text(marker: char, filler_len: usize) -> String {
    format!(
        "{{\"marker\":\"{marker}\",\"blob\":\"{}\"}}",
        marker.to_string().repeat(filler_len)
    )
}

#[test]
fn inline_json_roundtrips_byte_exact() {
    let dir = temp_dir("inline");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(docs_schema()).unwrap();

    // A small document stores inline (v1 row). Insertion order and duplicate
    // keys are canonicalized: read-back is sorted, deduped, byte-identical to a
    // fresh parse of the canonical text.
    let want = json("{\"z\":1,\"a\":[true,null,\"x\"],\"a\":[true,null,\"x\"]}");
    let rid = cat
        .insert("docs", &vec![Value::Int(1), want.clone()])
        .unwrap();
    cat.sync_wal().unwrap();

    let row = cat.get("docs", rid).unwrap();
    assert_eq!(row[1], want, "inline json must round-trip byte-exact");
    // The canonical text is sorted-keys.
    assert_eq!(
        pj1_to_text(&body_bytes(&row)).unwrap(),
        "{\"a\":[true,null,\"x\"],\"z\":1}"
    );

    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn large_json_spills_and_roundtrips() {
    let dir = temp_dir("spill");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(docs_schema()).unwrap();

    let big = json(&big_json_text('S', 8000)); // > 4070 B -> spills
    let rid = cat
        .insert("docs", &vec![Value::Int(1), big.clone()])
        .unwrap();
    cat.sync_wal().unwrap();

    assert!(
        cat.get_table("docs").unwrap().has_overflow_rows(),
        "an 8 KB json document must spill to an overflow chain"
    );

    let row = cat.get("docs", rid).unwrap();
    assert_eq!(row[1], big, "spilled json must reassemble byte-exact");

    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn update_inline_to_spilled_to_inline() {
    let dir = temp_dir("transitions");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(docs_schema()).unwrap();
    cat.create_index_unique("docs", "id", true).unwrap();

    // inline
    let small = json("{\"n\":1}");
    let rid = cat
        .insert("docs", &vec![Value::Int(1), small.clone()])
        .unwrap();
    cat.sync_wal().unwrap();

    // -> spilled (relocates; id not in the changed-set exercises index repoint)
    let big = json(&big_json_text('B', 9000));
    cat.update_hinted("docs", rid, &vec![Value::Int(1), big.clone()], Some(&[1]))
        .unwrap();
    cat.sync_wal().unwrap();
    assert_eq!(
        cat.index_lookup("docs", "id", &Value::Int(1))
            .unwrap()
            .unwrap()[1],
        big,
        "after spill, keyed lookup returns the big json"
    );

    // -> back inline
    let small2 = json("{\"n\":2}");
    cat.update_hinted(
        "docs",
        rid,
        &vec![Value::Int(1), small2.clone()],
        Some(&[1]),
    )
    .unwrap();
    cat.sync_wal().unwrap();
    assert_eq!(
        cat.index_lookup("docs", "id", &Value::Int(1))
            .unwrap()
            .unwrap()[1],
        small2
    );

    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn delete_frees_json_row() {
    let dir = temp_dir("delete");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(docs_schema()).unwrap();

    let big = json(&big_json_text('D', 7000));
    let rid = cat.insert("docs", &vec![Value::Int(1), big]).unwrap();
    cat.sync_wal().unwrap();
    assert_eq!(cat.scan("docs").unwrap().count(), 1);

    cat.delete("docs", rid).unwrap();
    cat.sync_wal().unwrap();
    assert_eq!(cat.scan("docs").unwrap().count(), 0);

    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn crash_recovery_preserves_spilled_json() {
    let dir = temp_dir("crash");
    std::fs::create_dir_all(&dir).unwrap();
    let big = json(&big_json_text('C', 60_000)); // multi-page chain
    {
        let mut cat = Catalog::create(&dir).unwrap();
        cat.create_table(docs_schema()).unwrap();
        cat.insert("docs", &vec![Value::Int(1), big.clone()])
            .unwrap();
        cat.sync_wal().unwrap();
        // Simulate a crash before checkpoint: abandon the catalog. The WAL +
        // direct chain writes carry durability.
        std::mem::forget(cat);
    }

    let cat = Catalog::open(&dir).unwrap();
    let rows: Vec<(RowId, Vec<Value>)> = cat.scan("docs").unwrap().collect();
    assert_eq!(rows.len(), 1, "spilled json row must recover");
    assert_eq!(rows[0].1[1], big, "recovered json must be byte-exact");

    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn many_mixed_json_documents_survive_restart() {
    let dir = temp_dir("mixed");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cat = Catalog::create(&dir).unwrap();
    cat.create_table(docs_schema()).unwrap();

    // Alternate tiny (inline) and large (spilled) documents.
    let mut expected: Vec<(i64, Value)> = Vec::new();
    for i in 0..40i64 {
        let v = if i % 2 == 0 {
            json(&format!("{{\"i\":{i},\"kind\":\"small\"}}"))
        } else {
            json(&big_json_text((b'a' + (i as u8 % 26)) as char, 5000))
        };
        cat.insert("docs", &vec![Value::Int(i), v.clone()]).unwrap();
        expected.push((i, v));
    }
    cat.sync_wal().unwrap();
    cat.checkpoint().unwrap();
    drop(cat);

    let cat = Catalog::open(&dir).unwrap();
    let mut got: Vec<(i64, Value)> = cat
        .scan("docs")
        .unwrap()
        .map(|(_, row)| match &row[0] {
            Value::Int(n) => (*n, row[1].clone()),
            other => panic!("expected Int id, got {other:?}"),
        })
        .collect();
    got.sort_by_key(|(i, _)| *i);
    assert_eq!(
        got, expected,
        "every json document survives a restart byte-exact"
    );

    drop(cat);
    std::fs::remove_dir_all(&dir).ok();
}
