//! Project(IndexScan) returns a spilled column only when the query projects
//! it. Two rows share id 1, so that equality is a batch rather than a point
//! read. Two other ids keep the literal under half the index, so lowering
//! leaves the IndexScan in place. A 70_000-byte payload cannot be re-inlined
//! into a v1 row.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

const LARGE_LEN: usize = 70_000;

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn seed() -> (tempfile::TempDir, Engine, Vec<u8>, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { id: int, payload: bytes }")
        .unwrap();
    let large = vec![0xAB; LARGE_LEN];
    let short = b"hi".to_vec();
    {
        let table = engine.catalog_mut().get_table_mut("T").unwrap();
        table
            .insert(&vec![Value::Int(1), Value::Bytes(large.clone())])
            .unwrap();
        table
            .insert(&vec![Value::Int(1), Value::Bytes(short.clone())])
            .unwrap();
        // Different ids, so `id = 1` is two rows out of four and stays an
        // IndexScan. A match of the whole table is lowered to a seq scan.
        table
            .insert(&vec![Value::Int(2), Value::Bytes(b"no".to_vec())])
            .unwrap();
        table
            .insert(&vec![Value::Int(3), Value::Bytes(b"no".to_vec())])
            .unwrap();
    }
    engine.execute_powql("alter T add index .id").unwrap();
    assert!(
        engine.catalog().table_has_overflow("T"),
        "70_000-byte payload must spill"
    );
    (dir, engine, large, short)
}

fn assert_matches_generic(engine: &mut Engine, query: &str) -> Vec<Vec<Value>> {
    let plan = format!(
        "{:?}",
        engine.execute_powql(&format!("explain {query}")).unwrap()
    );
    assert!(plan.contains("IndexScan"), "{query} plan: {plan}");

    engine.set_force_generic_path(true);
    engine.reset_forced_generic_sites();
    let generic = rows(engine.execute_powql(query).unwrap());
    assert!(
        engine
            .forced_generic_sites()
            .contains(&"project-over-index-scan"),
        "{query} did not reach project-over-index-scan: {:?}",
        engine.forced_generic_sites()
    );
    engine.reset_forced_generic_sites();
    let generic_readonly = rows(engine.execute_powql_readonly(query).unwrap());
    assert!(
        engine
            .forced_generic_sites()
            .contains(&"readonly:project-over-index-scan"),
        "{query} readonly did not reach the readonly index projection: {:?}",
        engine.forced_generic_sites()
    );
    assert_eq!(generic_readonly, generic, "{query} generic readonly");

    engine.set_force_generic_path(false);
    assert_eq!(
        rows(engine.execute_powql(query).unwrap()),
        generic,
        "{query}"
    );
    assert_eq!(
        rows(engine.execute_powql_readonly(query).unwrap()),
        generic,
        "{query} readonly"
    );
    generic
}

#[test]
fn projecting_id_or_id_and_payload_matches_generic_and_readonly() {
    let (_dir, mut engine, large, short) = seed();
    let ids = assert_matches_generic(&mut engine, "T filter .id = 1 { .id }");
    assert_eq!(ids, vec![vec![Value::Int(1)], vec![Value::Int(1)]]);

    let both = assert_matches_generic(&mut engine, "T filter .id = 1 { .id, .payload }");
    assert_eq!(both.len(), 2);
    let mut payloads: Vec<Vec<u8>> = both
        .iter()
        .map(|row| {
            assert_eq!(row[0], Value::Int(1));
            let Value::Bytes(bytes) = &row[1] else {
                panic!("payload column is bytes, got {:?}", row[1]);
            };
            assert!(
                !bytes.is_empty(),
                "a projected spill must not come back Empty"
            );
            bytes.clone()
        })
        .collect();
    payloads.sort();
    let mut expected = vec![large, short];
    expected.sort();
    assert_eq!(payloads, expected);
    assert_eq!(
        payloads.iter().map(Vec::len).max(),
        Some(LARGE_LEN),
        "the 70_000-byte payload must round-trip"
    );
}
