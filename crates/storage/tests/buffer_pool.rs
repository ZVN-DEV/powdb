//! Buffer pool integration tests.
//!
//! Tests the `BufferPool` with its clock-sweep eviction, pin/unpin
//! semantics, dirty-page flush, and concurrent-style access patterns.

use powdb_storage::buffer::BufferPool;
use powdb_storage::page::PageType;

fn temp_pool(name: &str, capacity: usize) -> (BufferPool, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "powdb_bp_integ_{name}_{}_{:?}",
        std::process::id(),
        std::time::Instant::now()
    ));
    let pool = BufferPool::new(&path, capacity).unwrap();
    (pool, path)
}

// ── Basic get/put operations ──────────────────────────────────────────

#[test]
fn test_new_page_returns_unique_ids() {
    let (mut pool, path) = temp_pool("unique_ids", 10);
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(pool.new_page(PageType::Data).unwrap());
    }
    // All IDs should be distinct.
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 5);
    drop(pool);
    std::fs::remove_file(&path).ok();
}

#[test]
fn test_write_and_read_back() {
    let (mut pool, path) = temp_pool("write_read", 10);
    let pid = pool.new_page(PageType::Data).unwrap();
    {
        let page = pool.get_page_mut(pid).unwrap();
        page.insert(b"hello buffer pool");
    }
    pool.mark_dirty(pid);
    pool.flush_all().unwrap();

    let page = pool.get_page(pid).unwrap();
    assert_eq!(page.get(0).unwrap(), b"hello buffer pool");
    drop(pool);
    std::fs::remove_file(&path).ok();
}

#[test]
fn test_multiple_slots_on_page() {
    let (mut pool, path) = temp_pool("multi_slot", 10);
    let pid = pool.new_page(PageType::Data).unwrap();
    {
        let page = pool.get_page_mut(pid).unwrap();
        page.insert(b"row A");
        page.insert(b"row B");
        page.insert(b"row C");
    }
    pool.mark_dirty(pid);
    pool.flush_all().unwrap();

    let page = pool.get_page(pid).unwrap();
    assert_eq!(page.get(0).unwrap(), b"row A");
    assert_eq!(page.get(1).unwrap(), b"row B");
    assert_eq!(page.get(2).unwrap(), b"row C");
    drop(pool);
    std::fs::remove_file(&path).ok();
}

// ── Eviction under pressure ───────────────────────────────────────────

/// With capacity=3, create 6 pages. The pool must evict earlier pages
/// to make room. Re-accessing evicted pages should reload from disk.
#[test]
fn test_eviction_reloads_from_disk() {
    let (mut pool, path) = temp_pool("evict_reload", 3);
    let mut ids = Vec::new();
    for i in 0..6 {
        let pid = pool.new_page(PageType::Data).unwrap();
        {
            let page = pool.get_page_mut(pid).unwrap();
            page.insert(format!("data-{i}").as_bytes());
        }
        pool.mark_dirty(pid);
        ids.push(pid);
    }
    pool.flush_all().unwrap();

    // Access the earliest page — it was evicted, should reload from disk.
    let page = pool.get_page(ids[0]).unwrap();
    assert_eq!(page.get(0).unwrap(), b"data-0");
    drop(pool);
    std::fs::remove_file(&path).ok();
}

/// Verify that a dirty page evicted under pressure is flushed to disk
/// before being removed from the buffer.
#[test]
fn test_dirty_eviction_persists() {
    let (mut pool, path) = temp_pool("dirty_evict_persist", 2);
    let p0 = pool.new_page(PageType::Data).unwrap();
    {
        let page = pool.get_page_mut(p0).unwrap();
        page.insert(b"must survive eviction");
    }
    pool.mark_dirty(p0);

    // Fill the pool with 2 more pages, forcing p0 to be evicted.
    let _p1 = pool.new_page(PageType::Data).unwrap();
    let _p2 = pool.new_page(PageType::Data).unwrap();

    // p0 should still be readable (reloaded from disk).
    let page = pool.get_page(p0).unwrap();
    assert_eq!(page.get(0).unwrap(), b"must survive eviction");
    drop(pool);
    std::fs::remove_file(&path).ok();
}

// ── Pin / unpin behavior ──────────────────────────────────────────────

#[test]
fn test_pinned_page_survives_pressure() {
    let (mut pool, path) = temp_pool("pin_survive", 3);
    let p0 = pool.new_page(PageType::Data).unwrap();
    {
        let page = pool.get_page_mut(p0).unwrap();
        page.insert(b"pinned data");
    }
    pool.mark_dirty(p0);
    pool.pin(p0);

    // Create more pages than the pool can hold.
    for _ in 0..5 {
        let _ = pool.new_page(PageType::Data);
    }

    // p0 is pinned and should still be in the buffer (no disk reload needed).
    let page = pool.get_page(p0).unwrap();
    assert_eq!(page.get(0).unwrap(), b"pinned data");

    pool.unpin(p0);
    drop(pool);
    std::fs::remove_file(&path).ok();
}

/// All pages pinned with no room left should fail with an error.
#[test]
fn test_all_pinned_returns_error() {
    let (mut pool, path) = temp_pool("all_pinned", 2);
    let p0 = pool.new_page(PageType::Data).unwrap();
    let p1 = pool.new_page(PageType::Data).unwrap();
    pool.pin(p0);
    pool.pin(p1);

    let result = pool.new_page(PageType::Data);
    assert!(result.is_err(), "should fail when all pages are pinned");

    pool.unpin(p0);
    pool.unpin(p1);
    drop(pool);
    std::fs::remove_file(&path).ok();
}

/// Unpin below zero saturates at zero (saturating_sub).
#[test]
fn test_unpin_saturates() {
    let (mut pool, path) = temp_pool("unpin_saturate", 4);
    let p0 = pool.new_page(PageType::Data).unwrap();
    // Unpin without ever pinning — should not panic.
    pool.unpin(p0);
    pool.unpin(p0);

    // The page should still be evictable.
    let _p1 = pool.new_page(PageType::Data).unwrap();
    let _p2 = pool.new_page(PageType::Data).unwrap();
    let _p3 = pool.new_page(PageType::Data).unwrap();
    let _p4 = pool.new_page(PageType::Data).unwrap();
    // If p0 was not evictable this would fail (capacity=4, 5 pages).
    drop(pool);
    std::fs::remove_file(&path).ok();
}

// ── flush_page (single page flush) ───────────────────────────────────

#[test]
fn test_flush_single_page() {
    let (mut pool, path) = temp_pool("flush_single", 10);
    let p0 = pool.new_page(PageType::Data).unwrap();
    let p1 = pool.new_page(PageType::Data).unwrap();
    {
        let page = pool.get_page_mut(p0).unwrap();
        page.insert(b"page zero");
    }
    pool.mark_dirty(p0);
    {
        let page = pool.get_page_mut(p1).unwrap();
        page.insert(b"page one");
    }
    pool.mark_dirty(p1);

    // Flush only p0.
    pool.flush_page(p0).unwrap();

    // Both should still be readable.
    assert_eq!(pool.get_page(p0).unwrap().get(0).unwrap(), b"page zero");
    assert_eq!(pool.get_page(p1).unwrap().get(0).unwrap(), b"page one");
    drop(pool);
    std::fs::remove_file(&path).ok();
}

// ── Capacity-1 edge case ─────────────────────────────────────────────

#[test]
fn test_capacity_one() {
    let (mut pool, path) = temp_pool("cap1", 1);
    let p0 = pool.new_page(PageType::Data).unwrap();
    {
        let page = pool.get_page_mut(p0).unwrap();
        page.insert(b"only page");
    }
    pool.mark_dirty(p0);

    // Creating a second page evicts p0.
    let p1 = pool.new_page(PageType::Data).unwrap();
    {
        let page = pool.get_page_mut(p1).unwrap();
        page.insert(b"second page");
    }
    pool.mark_dirty(p1);

    // Access p0 again — should reload from disk.
    let page = pool.get_page(p0).unwrap();
    assert_eq!(page.get(0).unwrap(), b"only page");
    drop(pool);
    std::fs::remove_file(&path).ok();
}

// ── mark_dirty on non-loaded page is a no-op ─────────────────────────

#[test]
fn test_mark_dirty_missing_page_no_panic() {
    let (mut pool, path) = temp_pool("mark_missing", 4);
    // Mark a page ID that was never allocated — should be a no-op.
    pool.mark_dirty(9999);
    drop(pool);
    std::fs::remove_file(&path).ok();
}

// ── Re-open from existing file ───────────────────────────────────────

#[test]
fn test_reopen_existing_file() {
    let path = std::env::temp_dir().join(format!("powdb_bp_reopen_{}", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let pid;
    {
        let mut pool = BufferPool::new(&path, 10).unwrap();
        pid = pool.new_page(PageType::Data).unwrap();
        {
            let page = pool.get_page_mut(pid).unwrap();
            page.insert(b"persistent");
        }
        pool.mark_dirty(pid);
        pool.flush_all().unwrap();
    }
    {
        let mut pool = BufferPool::new(&path, 10).unwrap();
        let page = pool.get_page(pid).unwrap();
        assert_eq!(page.get(0).unwrap(), b"persistent");
    }
    std::fs::remove_file(&path).ok();
}
