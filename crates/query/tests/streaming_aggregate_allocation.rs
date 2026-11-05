//! Total application-allocator bytes during one cold readonly query, not RSS.
//! Reallocation counts the full new size; deallocations cannot hide input work.
use powdb_query::executor::Engine;
use powdb_storage::types::Value;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct CountingAllocator;
static TRACK: AtomicBool = AtomicBool::new(false);
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

fn allocated(bytes: usize) {
    if TRACK.load(Ordering::Relaxed) {
        ALLOCATED.fetch_add(bytes, Ordering::Relaxed);
    }
}

// SAFETY: pointers/layouts are forwarded unchanged to System; counters do not
// allocate. This binary runs one synchronous test with tracking enabled only
// during a cold query, not during fixture construction.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded caller-provided layout.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            allocated(layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded original pointer and layout.
        unsafe { System.dealloc(ptr, layout) };
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: forwarded caller-provided allocation and new size.
        let next = unsafe { System.realloc(ptr, layout, size) };
        if !next.is_null() && TRACK.load(Ordering::Relaxed) {
            allocated(size);
        }
        next
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn query_allocation(rows: i64) -> usize {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { id: int, n: int, body: str }")
        .unwrap();
    for id in 0..rows {
        engine
            .catalog_mut()
            .get_table_mut("T")
            .unwrap()
            .insert(&vec![
                Value::Int(id),
                Value::Int(id % 100),
                Value::Str("x".repeat(256)),
            ])
            .unwrap();
    }
    ALLOCATED.store(0, Ordering::Relaxed);
    TRACK.store(true, Ordering::Relaxed);
    let result = engine.execute_powql_readonly(
        "T group 1 { cnt: count(.id), total: sum(.n), mean: avg(.n), highest: max(.id) }",
    );
    TRACK.store(false, Ordering::Relaxed);
    match result.unwrap() {
        powdb_query::result::QueryResult::Rows { rows: actual, .. } => {
            assert_eq!(
                actual,
                vec![vec![
                    Value::Int(rows),
                    Value::Int(rows * 99 / 2),
                    Value::Float(49.5),
                    Value::Int(rows - 1)
                ]]
            );
        }
        other => panic!("expected aggregate row: {other:?}"),
    }
    ALLOCATED.load(Ordering::Relaxed)
}

#[test]
fn scalar_multiaggregate_total_allocation_stays_bounded() {
    let small = query_allocation(1_000);
    let large = query_allocation(20_000);
    eprintln!("aggregate total query allocation: small={small}, large={large}");
    assert!(large <= 1024 * 1024, "materialized input: {large} bytes");
    assert!(
        large <= small + 8 * 1024,
        "row-dependent state: small={small}, large={large}"
    );
}
