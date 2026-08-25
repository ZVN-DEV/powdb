#![no_main]
use libfuzzer_sys::fuzz_target;
use powdb_storage::btree::BTree;
use powdb_storage::types::Value;
use std::sync::atomic::{AtomicU64, Ordering};

// Index files are trusted at open: `Table::open` calls `BTree::load` on
// every `.idx` next to the heap, and the loaded tree's node graph is then
// walked by every keyed read. `fuzz_catalog_open` mutates `catalog.bin`
// while leaving the index files valid; nothing mutated the index bytes
// themselves. A half-flushed rebuild, a torn page, or a hostile file in a
// copied data dir arrives here first.
//
// Invariant: `BTree::load` returns `Ok` or a clean `Err` — never a panic,
// an out-of-bounds index, or a pre-allocation from an attacker-controlled
// length field — and on `Ok` the read surface over the loaded tree
// (point lookup, full ordered walk, range walk) is equally total.
//
// Random bytes die at the header check instantly, so the checked-in seeds
// are REAL index files (unique int, non-unique str) written by the current
// format. The fuzzer mutates outward from valid structure.

static CASE_ID: AtomicU64 = AtomicU64::new(0);

fuzz_target!(|data: &[u8]| {
    let path = std::env::temp_dir().join(format!(
        "powdb_fuzz_btree_case_{}_{}.idx",
        std::process::id(),
        CASE_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, data).expect("write fuzz index file");
    if let Ok(tree) = BTree::load(&path) {
        let _ = tree.lookup(&Value::Int(1));
        let _ = tree.lookup_all(&Value::Str("u1".into()));
        let _ = tree.ordered_pairs();
        let _ = tree.raw_range_rids(Some(&Value::Int(0)), Some(&Value::Int(100)));
        let _ = tree.empty_rids();
    }
    let _ = std::fs::remove_file(&path);
});
