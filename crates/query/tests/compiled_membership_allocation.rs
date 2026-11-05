//! Total allocator traffic for a scalar membership count, not peak RSS.
use powdb_query::{executor::Engine, result::QueryResult};
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

// SAFETY: forwards System's pointer/layout contract unchanged; counters never
// allocate. The only test is synchronous and tracks queries, not fixture setup.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: unchanged caller layout.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            allocated(layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: original pointer/layout forwarded.
        unsafe { System.dealloc(ptr, layout) };
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: unchanged original allocation and requested size.
        let next = unsafe { System.realloc(ptr, layout, size) };
        if !next.is_null() {
            allocated(size);
        }
        next
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn measure(rows: i64, readonly: bool, negated: bool) -> usize {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { k: int, body: str }")
        .unwrap();
    for id in 0..rows {
        engine
            .catalog_mut()
            .get_table_mut("T")
            .unwrap()
            .insert(&vec![Value::Int(id % 100), Value::Str("x".repeat(256))])
            .unwrap();
    }
    let op = if negated { "not in" } else { "in" };
    let query = format!("count(T filter .k {op} (1, 3, 5, 7, 9))");
    ALLOCATED.store(0, Ordering::Relaxed);
    TRACK.store(true, Ordering::Relaxed);
    let result = if readonly {
        engine.execute_powql_readonly(&query)
    } else {
        engine.execute_powql(&query)
    };
    TRACK.store(false, Ordering::Relaxed);
    let bytes = ALLOCATED.load(Ordering::Relaxed);
    let expected = if negated {
        rows * 95 / 100
    } else {
        rows * 5 / 100
    };
    assert!(matches!(result.unwrap(), QueryResult::Scalar(Value::Int(n)) if n == expected));
    bytes
}

#[test]
fn membership_count_allocation_does_not_grow_with_rows() {
    for readonly in [false, true] {
        for negated in [false, true] {
            let small = measure(1_000, readonly, negated);
            let large = measure(20_000, readonly, negated);
            eprintln!("membership allocation: readonly={readonly} not_in={negated} small={small} large={large}");
            assert!(large < 1024 * 1024, "per-row decoding allocation: {large}");
            assert!(
                large <= small + 8 * 1024,
                "row-dependent traffic: {small} -> {large}"
            );
        }
    }
}
