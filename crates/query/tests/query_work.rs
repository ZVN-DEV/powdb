//! Work counters prove the selected access path without timing assertions.
use powdb_query::executor::{capture_query_work, Engine};
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

#[test]
fn indexed_in_visits_matching_heap_rows_on_both_surfaces() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type Work { id: int, key: int }")
        .unwrap();
    for id in 0..1_000 {
        engine
            .catalog_mut()
            .get_table_mut("Work")
            .unwrap()
            .insert(&vec![Value::Int(id), Value::Int(id)])
            .unwrap();
    }
    let (_, scan) = capture_query_work(|| engine.execute_powql("Work").unwrap());
    assert_eq!(scan.seq_rows, 1_000);
    engine
        .catalog_mut()
        .create_index_unique("Work", "key", false)
        .unwrap();

    for readonly in [false, true] {
        let (result, work) = capture_query_work(|| {
            let q = "Work filter .key in (3, 3, 7) order .id { .id }";
            if readonly {
                engine.execute_powql_readonly(q)
            } else {
                engine.execute_powql(q)
            }
        });
        let QueryResult::Rows { rows, .. } = result.unwrap() else {
            panic!("expected rows")
        };
        assert_eq!(rows, vec![vec![Value::Int(3)], vec![Value::Int(7)]]);
        assert_eq!(
            work.seq_rows, 0,
            "an indexed membership query must not scan the heap"
        );
        assert_eq!(
            work.index_probes, 2,
            "duplicate keys must not add index probes"
        );
        assert_eq!(work.heap_fetches, 2, "only matching rows are fetched");
    }
}
