//! Malformed serialized counts must not drive index-open allocations.
use powdb_storage::btree::BTree;
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

// SAFETY: forwarding preserves System's pointer/layout contract; counters never
// allocate. This binary has one synchronous test and tracks only BTree::load.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forward the caller's layout unchanged.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            allocated(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forward the original allocation pointer and layout.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: forward the allocation and requested size unchanged.
        let next = unsafe { System.realloc(ptr, layout, size) };
        if !next.is_null() {
            allocated(size);
        }
        next
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn file(version: u16, nodes: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = b"BIDX".to_vec();
    bytes.extend_from_slice(&version.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&nodes.to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(&crc32fast::hash(&bytes).to_le_bytes());
    bytes
}

#[test]
fn impossible_counts_are_rejected_before_reserving_capacity() {
    // Large enough to detect eager reservations, small enough to reproduce the
    // original bug safely without actually requesting the fuzz cases' 2 GiB.
    let many = 100_000u32.to_le_bytes();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.idx");
    for version in 1..=3 {
        let cases = [
            ("node", file(version, 50_000, &[])),
            (
                "internal key",
                file(version, 1, &[&[0][..], &many, &0u32.to_le_bytes()].concat()),
            ),
            (
                "leaf key",
                file(version, 1, &[&[1][..], &many, &[0][..]].concat()),
            ),
            (
                "child",
                file(version, 1, &[&[0][..], &0u32.to_le_bytes(), &many].concat()),
            ),
        ];
        for (name, bytes) in cases {
            std::fs::write(&path, bytes).unwrap();
            ALLOCATED.store(0, Ordering::Relaxed);
            TRACK.store(true, Ordering::Relaxed);
            let result = BTree::load(&path);
            TRACK.store(false, Ordering::Relaxed);
            let total = ALLOCATED.load(Ordering::Relaxed);
            let error = result.err().expect("accepted impossible count");
            assert!(error.to_string().contains(name), "v{version}: {error}");
            assert!(
                total < 64 * 1024,
                "{name} v{version} reserved {total} bytes"
            );
        }

        // A single variable-width key passes the outer count guards. Its own
        // untrusted length must fail cleanly, including on 32-bit usize.
        for tag in [4u8, 7, 8] {
            for length in [u32::MAX, 1 << 31] {
                let leaf = [
                    &[1][..],
                    &1u32.to_le_bytes(),
                    &[tag][..],
                    &length.to_le_bytes(),
                    &[0; 7][..], // RID + next flag.
                ]
                .concat();
                std::fs::write(&path, file(version, 1, &leaf)).unwrap();
                assert_eq!(
                    BTree::load(&path).err().unwrap().kind(),
                    std::io::ErrorKind::UnexpectedEof,
                );
            }
        }

        // Exact minima used by the guards must still load, including the
        // one-byte Empty key and the six-byte empty leaf in every format.
        for keys in [0u32, 1] {
            let mut leaf = vec![1];
            leaf.extend_from_slice(&keys.to_le_bytes());
            if keys == 1 {
                leaf.extend_from_slice(&[0; 7]); // Empty type tag + RID.
            }
            leaf.push(0); // No next leaf.
            if version >= 2 {
                leaf.extend_from_slice(&0u32.to_le_bytes()); // No empty RIDs.
            }
            std::fs::write(&path, file(version, 1, &leaf)).unwrap();
            let tree = BTree::load(&path).unwrap();
            assert_eq!(tree.len(), keys as usize);
            assert_eq!(tree.format_version(), version);
        }
    }
}
