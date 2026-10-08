//! Allocation evidence for one cold Off-mode statement, independent of timers.
use powdb_storage::catalog::Catalog;
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};
use powdb_storage::wal::WalSyncMode;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct CountingAllocator;
static TRACK: AtomicBool = AtomicBool::new(false);
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static FREED: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every allocation operation is forwarded unchanged to System. The
// counters allocate nothing and tracking is confined to this one-test binary.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: GlobalAlloc's caller provides a valid layout.
        let ptr = unsafe { System.alloc(layout) };
        if TRACK.load(Ordering::Relaxed) && !ptr.is_null() {
            ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if TRACK.load(Ordering::Relaxed) {
            FREED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        // SAFETY: the pointer and original layout are forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) };
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller's allocation and new size are forwarded unchanged.
        let replacement = unsafe { System.realloc(ptr, layout, new_size) };
        if TRACK.load(Ordering::Relaxed) && !replacement.is_null() {
            ALLOCATED.fetch_add(new_size, Ordering::Relaxed);
            FREED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        replacement
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn measured_update(rows: i64) -> (usize, isize) {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::create(dir.path()).unwrap();
    catalog.set_wal_sync_mode(WalSyncMode::Off);
    catalog
        .create_table(Schema {
            table_name: "T".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    type_id: TypeId::Int,
                    required: true,
                    position: 0,
                },
                ColumnDef {
                    name: "payload".into(),
                    type_id: TypeId::Str,
                    required: true,
                    position: 1,
                },
            ],
        })
        .unwrap();
    let payload = "a".repeat(64);
    for id in 0..rows {
        catalog
            .insert("T", &vec![Value::Int(id), Value::Str(payload.clone())])
            .unwrap();
    }
    catalog.create_index_unique("T", "id", true).unwrap();
    catalog.checkpoint().unwrap();
    let rid = catalog.scan("T").unwrap().next().unwrap().unwrap().0;
    let replacement = vec![Value::Int(0), Value::Str("b".repeat(64))];
    ALLOCATED.store(0, Ordering::Relaxed);
    FREED.store(0, Ordering::Relaxed);
    TRACK.store(true, Ordering::Relaxed);
    catalog.begin_statement_transaction().unwrap();
    catalog.update("T", rid, &replacement).unwrap();
    catalog.commit_transaction().unwrap();
    TRACK.store(false, Ordering::Relaxed);
    let allocated = ALLOCATED.load(Ordering::Relaxed);
    let net = allocated as isize - FREED.load(Ordering::Relaxed) as isize;
    assert_eq!(
        catalog.get_table("T").unwrap().get(rid).unwrap().unwrap(),
        replacement
    );
    (allocated, net)
}

#[test]
fn one_page_statement_allocation_does_not_scale_with_total_heap_size() {
    let small = measured_update(1_000);
    let large = measured_update(100_000);
    eprintln!("cold Off update: small allocated/net={small:?}; large={large:?}");
    assert!(
        large.0 <= small.0 + 4096,
        "whole-heap allocation: small={small:?}, large={large:?}"
    );
    // This is net application-allocator activity, not total process RSS. Allow
    // small persistent bookkeeping changes but not retained page-image/table clones.
    assert!(
        large.1 <= 4096,
        "large retained allocation after commit: {large:?}"
    );
}
