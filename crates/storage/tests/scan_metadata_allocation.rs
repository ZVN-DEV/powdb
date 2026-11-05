//! One synchronous allocator test; counts total allocation traffic, not RSS.
use powdb_storage::catalog::Catalog;
use powdb_storage::row::decode_row;
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Allocator;
static TRACK: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
fn record(bytes: usize) {
    if TRACK.load(Ordering::Relaxed) {
        CALLS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(bytes, Ordering::Relaxed);
    }
}
// SAFETY: forwards original arguments to System; counters never allocate.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: caller-provided valid layout is forwarded unchanged.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record(layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: original pointer and allocation layout are forwarded.
        unsafe { System.dealloc(ptr, layout) };
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: caller-provided allocation and size are forwarded unchanged.
        let next = unsafe { System.realloc(ptr, layout, size) };
        if !next.is_null() {
            record(size);
        }
        next
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn measure(f: impl FnOnce() -> usize) -> (usize, usize, usize) {
    CALLS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    TRACK.store(true, Ordering::Relaxed);
    let count = f();
    TRACK.store(false, Ordering::Relaxed);
    (
        count,
        CALLS.load(Ordering::Relaxed),
        BYTES.load(Ordering::Relaxed),
    )
}

#[test]
fn cached_scan_removes_per_row_metadata_allocation() {
    const ROWS: usize = 1000;
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::create(dir.path()).unwrap();
    let schema = Schema {
        table_name: "T".into(),
        columns: [TypeId::Int, TypeId::Str, TypeId::Int, TypeId::Str]
            .into_iter()
            .enumerate()
            .map(|(position, type_id)| ColumnDef {
                name: format!("c{position}"),
                type_id,
                required: false,
                position: position as u16,
            })
            .collect(),
    };
    catalog.create_table(schema.clone()).unwrap();
    let table = catalog.get_table_mut("T").unwrap();
    for n in 0..ROWS {
        table
            .insert(&vec![
                Value::Int(n as i64),
                Value::Str("a".into()),
                Value::Int(42),
                Value::Str("b".into()),
            ])
            .unwrap();
    }
    // Fixture setup and warm-up are deliberately outside tracking.
    assert_eq!(table.scan().count(), ROWS);
    let reference = measure(|| {
        let mut count = 0;
        for item in table.heap.scan() {
            let (_, data) = item.unwrap();
            let row = decode_row(&schema, &data);
            if row[2] == Value::Int(42) {
                count += 1;
            }
            std::hint::black_box(row);
        }
        count
    });
    let scan = measure(|| {
        table
            .scan()
            .map(|item| {
                let (_, row) = item.unwrap();
                let matches = row[2] == Value::Int(42);
                std::hint::black_box(row);
                matches
            })
            .filter(|matches| *matches)
            .count()
    });
    eprintln!("rows/calls/bytes: old heap+decoder={reference:?}, Table::scan={scan:?}");
    assert_eq!(scan.0, ROWS);
    assert_eq!(reference.0, ROWS);
    // Both sides retain identical heap iteration/copies. Two temporary
    // metadata vectors account for two calls and seven usize slots per row.
    assert!(scan.1 + ROWS * 2 <= reference.1);
    assert!(scan.2 + ROWS * 7 * std::mem::size_of::<usize>() <= reference.2);
}
