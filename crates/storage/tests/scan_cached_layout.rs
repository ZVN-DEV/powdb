use powdb_storage::catalog::Catalog;
use powdb_storage::row::{decode_row, encode_row, ROW_PREFIX_SIZE};
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};

fn schema(types: &[TypeId]) -> Schema {
    Schema {
        table_name: "T".into(),
        columns: types
            .iter()
            .enumerate()
            .map(|(i, &type_id)| ColumnDef {
                name: format!("c{i}"),
                type_id,
                required: false,
                position: i as u16,
            })
            .collect(),
    }
}

#[test]
fn table_scan_matches_old_decoder_for_legacy_and_prefixed_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::create(dir.path()).unwrap();
    let schema = schema(&[
        TypeId::Str,
        TypeId::Int,
        TypeId::Bytes,
        TypeId::Float,
        TypeId::Bool,
        TypeId::Json,
        TypeId::DateTime,
        TypeId::Uuid,
        TypeId::Int,
    ]);
    catalog.create_table(schema.clone()).unwrap();
    let row = vec![
        Value::Str("a".into()),
        Value::Int(i64::MIN),
        Value::Bytes(vec![0, 255]),
        Value::Float(1.25),
        Value::Bool(true),
        Value::Json(vec![0].into()),
        Value::DateTime(i64::MAX),
        Value::Uuid([42; 16]),
        Value::Empty,
    ];
    let table = catalog.get_table_mut("T").unwrap();
    let mut expected = Vec::new();
    for values in [&row, &vec![Value::Empty; row.len()]] {
        let encoded = encode_row(&schema, values);
        for bytes in [&encoded[..], &encoded[ROW_PREFIX_SIZE..]] {
            table.heap.insert(bytes).unwrap();
            expected.push(decode_row(&schema, bytes));
        }
    }
    assert_eq!(
        table.scan().map(|item| item.unwrap().1).collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn cached_scan_preserves_large_overflow_values() {
    for length in [10_000, 70_000] {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::create(dir.path()).unwrap();
        catalog
            .create_table(schema(&[TypeId::Str, TypeId::Int, TypeId::Bytes]))
            .unwrap();
        let table = catalog.get_table_mut("T").unwrap();
        let expected = vec![
            Value::Str("a".repeat(length)),
            Value::Int(42),
            Value::Bytes(vec![255; length]),
        ];
        let rid = table.insert(&expected).unwrap();
        assert!(table.has_overflow_rows());
        assert_eq!(table.get(rid).unwrap().unwrap(), expected);
        let rows: Vec<_> = table.scan().map(|item| item.unwrap().1).collect();
        assert_eq!(rows, vec![expected]);
    }
}

#[test]
fn cached_scan_layout_tracks_add_and_drop_columns() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::create(dir.path()).unwrap();
    catalog
        .create_table(schema(&[TypeId::Str, TypeId::Int, TypeId::Bytes]))
        .unwrap();
    catalog
        .get_table_mut("T")
        .unwrap()
        .insert(&vec![
            Value::Str("a".into()),
            Value::Int(7),
            Value::Bytes(vec![1, 2]),
        ])
        .unwrap();
    catalog
        .alter_table_add_column(
            "T",
            ColumnDef {
                name: "extra".into(),
                type_id: TypeId::Int,
                required: false,
                position: 3,
            },
        )
        .unwrap();
    let read = |catalog: &Catalog| {
        catalog
            .get_table("T")
            .unwrap()
            .scan()
            .map(|item| item.unwrap().1)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        read(&catalog),
        vec![vec![
            Value::Str("a".into()),
            Value::Int(7),
            Value::Bytes(vec![1, 2]),
            Value::Empty
        ]]
    );
    catalog.alter_table_drop_column("T", "c0").unwrap();
    assert_eq!(
        read(&catalog),
        vec![vec![Value::Int(7), Value::Bytes(vec![1, 2]), Value::Empty]]
    );
    catalog.alter_table_drop_column("T", "c1").unwrap();
    assert_eq!(
        read(&catalog),
        vec![vec![Value::Bytes(vec![1, 2]), Value::Empty]]
    );
    catalog.checkpoint().unwrap();
    drop(catalog);
    let reopened = Catalog::open(dir.path()).unwrap();
    assert_eq!(
        read(&reopened),
        vec![vec![Value::Bytes(vec![1, 2]), Value::Empty]]
    );
}
