//! B+ tree edge case tests.
//!
//! These tests exercise split boundaries, deletion, duplicate handling,
//! large keys, and empty-tree operations against the in-memory B+ tree
//! implementation in `powdb_storage::btree`.

use powdb_storage::btree::BTree;
use powdb_storage::types::{RowId, Value};

fn temp_btree(name: &str) -> BTree {
    let path = std::env::temp_dir().join(format!(
        "powdb_btree_edge_{name}_{}_{:?}",
        std::process::id(),
        std::time::Instant::now()
    ));
    BTree::create(&path).unwrap()
}

fn rid(page: u32, slot: u16) -> RowId {
    RowId {
        page_id: page,
        slot_index: slot,
    }
}

// ── Leaf node splits ──────────────────────────────────────────────────

/// ORDER is 256, so a leaf holds up to 256 keys before splitting.
/// Insert 257 sequential keys to force exactly one leaf split.
#[test]
fn test_leaf_split_boundary() {
    let mut bt = temp_btree("leaf_split");
    for i in 0..257 {
        bt.insert(Value::Int(i), rid(0, i as u16));
    }
    assert_eq!(bt.len(), 257);
    // Every key must still be reachable after the split.
    for i in 0..257 {
        assert!(
            bt.lookup(&Value::Int(i)).is_some(),
            "key {i} missing after leaf split"
        );
    }
}

/// Insert exactly ORDER (256) keys — should NOT split (boundary is <=).
#[test]
fn test_no_split_at_order() {
    let mut bt = temp_btree("no_split");
    for i in 0..256 {
        bt.insert(Value::Int(i), rid(0, i as u16));
    }
    assert_eq!(bt.len(), 256);
    for i in 0..256 {
        assert!(bt.lookup(&Value::Int(i)).is_some(), "key {i} missing");
    }
}

// ── Internal node splits ──────────────────────────────────────────────

/// Inserting 70,000 sequential keys forces multiple levels of internal
/// node splits (height >= 3 with ORDER=256).
#[test]
fn test_internal_node_splits() {
    let mut bt = temp_btree("internal_split");
    let n = 70_000i64;
    for i in 0..n {
        bt.insert_int(i, rid((i / 1000) as u32, (i % 1000) as u16));
    }
    assert_eq!(bt.len(), n as usize);
    // Spot-check first, middle, last keys.
    assert!(bt.lookup_int(0).is_some());
    assert!(bt.lookup_int(n / 2).is_some());
    assert!(bt.lookup_int(n - 1).is_some());
    assert!(bt.lookup_int(n).is_none());
}

// ── Range scans across split boundaries ───────────────────────────────

/// Range scan that spans at least one leaf split boundary. With 500 keys
/// and ORDER=256, there are 2+ leaves. A range across the split point
/// must follow the next_leaf chain correctly.
#[test]
fn test_range_scan_across_splits() {
    let mut bt = temp_btree("range_split");
    for i in 0..500 {
        bt.insert(Value::Int(i), rid(0, i as u16));
    }
    // The split happens around key 128 (mid of 0..256). Scan 100..300
    // which spans both leaves.
    let results: Vec<_> = bt.range(&Value::Int(100), &Value::Int(300)).collect();
    assert_eq!(results.len(), 201); // 100..=300 inclusive
    for (j, (k, _)) in results.iter().enumerate() {
        assert_eq!(*k, Value::Int(100 + j as i64));
    }
}

/// Range scan that covers the entire key space.
#[test]
fn test_range_scan_full_tree() {
    let mut bt = temp_btree("range_full");
    for i in 0..1000 {
        bt.insert(Value::Int(i), rid(0, i as u16));
    }
    let results: Vec<_> = bt.range(&Value::Int(0), &Value::Int(999)).collect();
    assert_eq!(results.len(), 1000);
}

/// range_from and range_to across split boundaries.
#[test]
fn test_range_from_and_range_to() {
    let mut bt = temp_btree("range_from_to");
    for i in 0..500 {
        bt.insert(Value::Int(i), rid(0, i as u16));
    }
    let from = bt.range_from(&Value::Int(400));
    assert_eq!(from.len(), 100); // 400..=499
    let to = bt.range_to(&Value::Int(50));
    assert_eq!(to.len(), 51); // 0..=50
}

// ── Deletion from leaf nodes ──────────────────────────────────────────

/// Delete every other key and verify the remaining keys are intact.
#[test]
fn test_delete_every_other() {
    let mut bt = temp_btree("delete_every_other");
    for i in 0..500 {
        bt.insert(Value::Int(i), rid(0, i as u16));
    }
    for i in (0..500).step_by(2) {
        assert!(bt.delete(&Value::Int(i)));
    }
    assert_eq!(bt.len(), 250);
    for i in 0..500 {
        if i % 2 == 0 {
            assert_eq!(bt.lookup(&Value::Int(i)), None, "key {i} should be deleted");
        } else {
            assert!(bt.lookup(&Value::Int(i)).is_some(), "key {i} should exist");
        }
    }
}

/// Delete a key that does not exist — should return false.
#[test]
fn test_delete_nonexistent() {
    let mut bt = temp_btree("delete_nonexistent");
    bt.insert(Value::Int(1), rid(0, 0));
    assert!(!bt.delete(&Value::Int(999)));
    assert_eq!(bt.len(), 1);
}

/// Delete all keys, then verify tree is empty and range scans return nothing.
#[test]
fn test_delete_all_keys() {
    let mut bt = temp_btree("delete_all");
    for i in 0..100 {
        bt.insert(Value::Int(i), rid(0, i as u16));
    }
    for i in 0..100 {
        assert!(bt.delete(&Value::Int(i)));
    }
    assert_eq!(bt.len(), 0);
    assert!(bt.is_empty());
    let results: Vec<_> = bt.range(&Value::Int(0), &Value::Int(100)).collect();
    assert!(results.is_empty());
}

/// Delete from an empty tree.
#[test]
fn test_delete_from_empty() {
    let mut bt = temp_btree("delete_empty");
    assert!(!bt.delete(&Value::Int(42)));
}

// ── Deletion using int-specialized path ───────────────────────────────

#[test]
fn test_delete_int_across_splits() {
    let mut bt = temp_btree("delete_int_splits");
    for i in 0..1000 {
        bt.insert_int(i, rid(0, i as u16));
    }
    // Delete a range that spans split boundaries
    for i in 100..400 {
        assert!(bt.delete_int(i), "delete_int({i}) should succeed");
    }
    assert_eq!(bt.len(), 700);
    for i in 0..1000 {
        if (100..400).contains(&i) {
            assert_eq!(bt.lookup_int(i), None);
        } else {
            assert!(bt.lookup_int(i).is_some(), "key {i} missing");
        }
    }
}

// ── Duplicate key handling ────────────────────────────────────────────

/// Inserting the same key twice should update the value, not create a duplicate.
#[test]
fn test_duplicate_key_overwrites() {
    let mut bt = temp_btree("dup_overwrite");
    bt.insert(Value::Int(42), rid(1, 0));
    bt.insert(Value::Int(42), rid(2, 5));
    assert_eq!(bt.len(), 1);
    assert_eq!(bt.lookup(&Value::Int(42)), Some(rid(2, 5)));
}

/// Same test via the int-specialized insert path.
#[test]
fn test_duplicate_key_overwrites_int() {
    let mut bt = temp_btree("dup_overwrite_int");
    bt.insert_int(42, rid(1, 0));
    bt.insert_int(42, rid(2, 5));
    assert_eq!(bt.len(), 1);
    assert_eq!(bt.lookup_int(42), Some(rid(2, 5)));
}

/// Mass duplicate inserts: insert 1000 keys then re-insert all 1000 with
/// new RowIds.
#[test]
fn test_mass_duplicate_update() {
    let mut bt = temp_btree("mass_dup");
    for i in 0..1000 {
        bt.insert(Value::Int(i), rid(0, i as u16));
    }
    for i in 0..1000 {
        bt.insert(Value::Int(i), rid(99, i as u16));
    }
    assert_eq!(bt.len(), 1000);
    for i in 0..1000 {
        assert_eq!(bt.lookup(&Value::Int(i)), Some(rid(99, i as u16)));
    }
}

// ── Large / extreme key values ────────────────────────────────────────

#[test]
fn test_extreme_int_keys() {
    let mut bt = temp_btree("extreme_int");
    let keys = [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX];
    for (i, &k) in keys.iter().enumerate() {
        bt.insert(Value::Int(k), rid(i as u32, 0));
    }
    assert_eq!(bt.len(), keys.len());
    for &k in &keys {
        assert!(bt.lookup(&Value::Int(k)).is_some(), "key {k} missing");
    }
    // Range scan from MIN to MAX should return all keys in order.
    let results: Vec<_> = bt
        .range(&Value::Int(i64::MIN), &Value::Int(i64::MAX))
        .collect();
    assert_eq!(results.len(), keys.len());
    for (j, (k, _)) in results.iter().enumerate() {
        assert_eq!(*k, Value::Int(keys[j]));
    }
}

#[test]
fn test_large_string_keys() {
    let mut bt = temp_btree("large_str");
    // 10KB string keys
    let key_a = "a".repeat(10_000);
    let key_b = "b".repeat(10_000);
    bt.insert(Value::Str(key_a.clone()), rid(0, 0));
    bt.insert(Value::Str(key_b.clone()), rid(0, 1));
    assert_eq!(bt.len(), 2);
    assert_eq!(
        bt.lookup(&Value::Str(key_a)).unwrap().slot_index,
        0
    );
    assert_eq!(
        bt.lookup(&Value::Str(key_b)).unwrap().slot_index,
        1
    );
}

// ── Empty tree operations ─────────────────────────────────────────────

#[test]
fn test_empty_tree_range_scan() {
    let bt = temp_btree("empty_range");
    let results: Vec<_> = bt.range(&Value::Int(0), &Value::Int(100)).collect();
    assert!(results.is_empty());
}

#[test]
fn test_empty_tree_range_from() {
    let bt = temp_btree("empty_range_from");
    let results = bt.range_from(&Value::Int(0));
    assert!(results.is_empty());
}

#[test]
fn test_empty_tree_range_to() {
    let bt = temp_btree("empty_range_to");
    let results = bt.range_to(&Value::Int(100));
    assert!(results.is_empty());
}

#[test]
fn test_empty_tree_lookup() {
    let bt = temp_btree("empty_lookup");
    assert_eq!(bt.lookup(&Value::Int(0)), None);
    assert_eq!(bt.lookup_int(0), None);
}

#[test]
fn test_empty_tree_len() {
    let bt = temp_btree("empty_len");
    assert_eq!(bt.len(), 0);
    assert!(bt.is_empty());
}

// ── Save/load round-trip with split trees ─────────────────────────────

#[test]
fn test_save_load_after_splits_and_deletes() {
    let path = std::env::temp_dir().join(format!(
        "powdb_btree_edge_save_del_{}.idx",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);

    let mut bt = BTree::create(&path).unwrap();
    for i in 0..1000 {
        bt.insert_int(i, rid((i / 256) as u32, (i % 256) as u16));
    }
    // Delete some keys before saving
    for i in (0..500).step_by(3) {
        bt.delete_int(i);
    }
    bt.save().unwrap();

    let reloaded = BTree::load(&path).unwrap();
    assert_eq!(reloaded.len(), bt.len());
    for i in 0..1000 {
        assert_eq!(
            reloaded.lookup_int(i),
            bt.lookup_int(i),
            "mismatch at key {i}"
        );
    }

    std::fs::remove_file(&path).ok();
}

/// Corrupt a saved btree file — flip bytes in the middle. Loading should
/// return an error (CRC mismatch), not panic.
#[test]
fn test_load_corrupted_file() {
    let path = std::env::temp_dir().join(format!(
        "powdb_btree_edge_corrupt_{}.idx",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);

    let mut bt = BTree::create(&path).unwrap();
    for i in 0..100 {
        bt.insert_int(i, rid(0, i as u16));
    }
    bt.save().unwrap();

    // Corrupt bytes in the middle of the file
    let mut data = std::fs::read(&path).unwrap();
    let mid = data.len() / 2;
    data[mid] ^= 0xFF;
    data[mid + 1] ^= 0xFF;
    std::fs::write(&path, &data).unwrap();

    let result = BTree::load(&path);
    assert!(result.is_err(), "loading a corrupted btree should error");

    std::fs::remove_file(&path).ok();
}

/// Loading a truncated btree file should return an error.
#[test]
fn test_load_truncated_file() {
    let path = std::env::temp_dir().join(format!(
        "powdb_btree_edge_trunc_{}.idx",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);

    let mut bt = BTree::create(&path).unwrap();
    for i in 0..100 {
        bt.insert_int(i, rid(0, i as u16));
    }
    bt.save().unwrap();

    // Truncate to half the original size
    let data = std::fs::read(&path).unwrap();
    std::fs::write(&path, &data[..data.len() / 2]).unwrap();

    let result = BTree::load(&path);
    assert!(result.is_err(), "loading a truncated btree should error");

    std::fs::remove_file(&path).ok();
}

// ── Reverse and random insertion orders ───────────────────────────────

/// Insert keys in reverse order — stresses position-0 insertions in every leaf.
#[test]
fn test_reverse_order_large() {
    let mut bt = temp_btree("reverse_large");
    for i in (0..2000i64).rev() {
        bt.insert_int(i, rid(0, (i % 1000) as u16));
    }
    assert_eq!(bt.len(), 2000);
    // Range scan should still return sorted order.
    let results: Vec<_> = bt.range(&Value::Int(0), &Value::Int(1999)).collect();
    assert_eq!(results.len(), 2000);
    for (j, (k, _)) in results.iter().enumerate() {
        assert_eq!(*k, Value::Int(j as i64));
    }
}

/// Interleaved insert and delete — exercises tree shape mutations in flight.
#[test]
fn test_interleaved_insert_delete() {
    let mut bt = temp_btree("interleaved");
    // Insert 0..500
    for i in 0..500 {
        bt.insert_int(i, rid(0, i as u16));
    }
    // Delete even keys 0..500, insert 500..1000
    for i in 0..500 {
        if i % 2 == 0 {
            bt.delete_int(i);
        }
        bt.insert_int(500 + i, rid(1, i as u16));
    }
    // Should have 250 odd keys from 0..500 + 500 keys from 500..1000 = 750
    assert_eq!(bt.len(), 750);
    for i in 0..500 {
        if i % 2 == 0 {
            assert_eq!(bt.lookup_int(i), None);
        } else {
            assert!(bt.lookup_int(i).is_some());
        }
    }
    for i in 500..1000 {
        assert!(bt.lookup_int(i).is_some(), "key {i} missing");
    }
}

// ── Dirty flag tracking ───────────────────────────────────────────────

#[test]
fn test_dirty_flag_lifecycle() {
    let path = std::env::temp_dir().join(format!(
        "powdb_btree_edge_dirty_{}.idx",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);

    let mut bt = BTree::create(&path).unwrap();
    assert!(!bt.is_dirty(), "fresh tree should not be dirty");

    bt.insert(Value::Int(1), rid(0, 0));
    assert!(bt.is_dirty(), "tree should be dirty after insert");

    bt.save().unwrap();
    assert!(!bt.is_dirty(), "tree should be clean after save");

    bt.delete(&Value::Int(1));
    assert!(bt.is_dirty(), "tree should be dirty after delete");

    bt.save_if_dirty().unwrap();
    assert!(!bt.is_dirty(), "tree should be clean after save_if_dirty");

    // save_if_dirty on a clean tree is a no-op (should not error)
    bt.save_if_dirty().unwrap();
    assert!(!bt.is_dirty());

    std::fs::remove_file(&path).ok();
}
