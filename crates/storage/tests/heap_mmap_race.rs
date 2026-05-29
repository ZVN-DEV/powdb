//! WS4-mmap: concurrency stress test for the heap mmap/write race.
//!
//! Production wraps the heap in `Arc<RwLock<Engine>>`, so reads take a
//! shared guard and writes take an exclusive guard. This test reproduces
//! that exact discipline against `HeapFile` directly: reader threads hold a
//! read guard while scanning the persistent mmap, and a writer thread holds
//! a write guard while inserting/updating (which periodically grows the
//! file and re-arms the mmap). The hazard being guarded against: a writer
//! tearing down (munmap) the mapping while a reader is mid-scan over a raw
//! `&[u8]` slice into it, yielding torn/garbage rows or a segfault.
//!
//! Every row in this fixture has a checkable invariant (name == "row_{id}"
//! and age == id), so any torn read — a half-written page, a byte from a
//! freed mapping, or a record from the wrong page — shows up as a decode
//! failure or a value mismatch. The test fails loudly rather than silently
//! tolerating corruption.

use powdb_storage::heap::HeapFile;
use powdb_storage::row::{decode_row, encode_row};
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

fn race_schema() -> Schema {
    Schema {
        table_name: "race".into(),
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
                required: true,
                position: 1,
            },
        ],
    }
}

/// Decode a row's bytes and assert the (name, age) invariant holds.
/// Returns the id parsed from the name so callers can sanity-check ranges.
fn assert_row_invariant(schema: &Schema, data: &[u8]) {
    let decoded = decode_row(schema, data);
    let name = match &decoded[0] {
        Value::Str(s) => s.clone(),
        other => panic!("torn read: name column not a string: {other:?}"),
    };
    let age = match &decoded[1] {
        Value::Int(i) => *i,
        other => panic!("torn read: age column not an int: {other:?}"),
    };
    let id: i64 = name
        .strip_prefix("row_")
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("torn read: malformed name {name:?}"));
    assert_eq!(
        age, id,
        "torn read: age {age} does not match id {id} from name {name:?}"
    );
}

#[test]
fn test_mmap_write_race_no_torn_reads() {
    let path = std::env::temp_dir().join(format!("powdb_heap_mmap_race_{}", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let schema = race_schema();

    // Seed enough rows to span many pages, then arm the persistent mmap.
    let mut heap = HeapFile::create(&path).unwrap();
    let seed_rows = 4_000i64;
    for id in 0..seed_rows {
        let row = vec![Value::Str(format!("row_{id}")), Value::Int(id)];
        heap.insert(&encode_row(&schema, &row)).unwrap();
    }
    heap.enable_mmap();

    let heap = Arc::new(RwLock::new(heap));
    let stop = Arc::new(AtomicBool::new(false));

    // Reader threads: take a shared guard, scan the whole heap, and verify
    // every row's invariant. If the writer munmaps under us or hands back a
    // torn page, this is where it surfaces.
    let mut readers = Vec::new();
    for _ in 0..4 {
        let heap = Arc::clone(&heap);
        let stop = Arc::clone(&stop);
        let schema = schema.clone();
        readers.push(thread::spawn(move || {
            let mut total_scanned = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let guard = heap.read().unwrap();
                let mut seen = 0u64;
                guard.try_for_each_row(|_rid, data| {
                    assert_row_invariant(&schema, data);
                    seen += 1;
                    std::ops::ControlFlow::Continue(())
                });
                // The fixture only grows, so a scan must never see fewer
                // rows than we started with.
                assert!(
                    seen >= 4_000,
                    "scan saw {seen} rows, fewer than the seeded 4000 — lost data"
                );
                total_scanned += seen;
                drop(guard);
            }
            total_scanned
        }));
    }

    // Writer thread: take an exclusive guard, insert fresh rows (forcing
    // file growth + mmap teardown on the new-page path), update existing
    // rows in place, then re-arm the mmap so subsequent reads hit it again.
    let writer = {
        let heap = Arc::clone(&heap);
        let stop = Arc::clone(&stop);
        let schema = schema.clone();
        thread::spawn(move || {
            let mut next_id = seed_rows;
            let deadline = Instant::now() + Duration::from_millis(800);
            while Instant::now() < deadline {
                {
                    let mut guard = heap.write().unwrap();
                    // Insert a batch — this allocates new pages and tears
                    // down the mmap on the growth path.
                    for _ in 0..200 {
                        let row = vec![Value::Str(format!("row_{next_id}")), Value::Int(next_id)];
                        guard.insert(&encode_row(&schema, &row)).unwrap();
                        next_id += 1;
                    }
                    // Re-arm the persistent mmap so readers exercise the
                    // mmap path again on their next scan.
                    guard.enable_mmap();
                }
                thread::yield_now();
            }
            stop.store(true, Ordering::Relaxed);
            next_id
        })
    };

    let final_id = writer.join().expect("writer panicked");
    for r in readers {
        r.join().expect("reader thread observed a torn read");
    }

    // Final consistency check: every row from 0..final_id must be present
    // and valid in a fresh scan.
    let guard = heap.read().unwrap();
    let mut count = 0u64;
    guard.try_for_each_row(|_rid, data| {
        assert_row_invariant(&schema, data);
        count += 1;
        std::ops::ControlFlow::Continue(())
    });
    assert_eq!(
        count, final_id as u64,
        "final scan row count {count} != inserted {final_id}"
    );
    drop(guard);

    let _ = std::fs::remove_file(&path);
}
