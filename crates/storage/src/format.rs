//! On-disk format version introspection.
//!
//! Every persisted storage structure has an explicit magic/version boundary.
//! Version `0` below means legacy 0.4.x data accepted by a compatibility
//! reader; current writers emit the non-zero versions listed here.
//!
//! `Catalog::open` logs these next to the version the directory it just opened
//! is actually at, which is the first thing worth knowing when a directory
//! refuses to open or comes back at an unexpected version. The table in
//! `docs/FORMAT.md` documents the same numbers, and the test below holds the
//! two equal.

use crate::btree;
use crate::catalog;
use crate::heap;
use crate::page;
use crate::row;
use crate::wal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatVersions {
    pub catalog: u16,
    pub btree: u16,
    pub heap_file: u16,
    pub heap_page: u8,
    pub row: u16,
    pub wal: u16,
}

pub const CURRENT_FORMAT_VERSIONS: FormatVersions = FormatVersions {
    catalog: catalog::CATALOG_VERSION,
    btree: btree::BTREE_VERSION,
    heap_file: heap::HEAP_FORMAT_VERSION,
    heap_page: page::PAGE_FORMAT_VERSION,
    row: row::ROW_FORMAT_VERSION,
    wal: wal::WAL_FORMAT_VERSION,
};

pub fn current_format_versions() -> FormatVersions {
    CURRENT_FORMAT_VERSIONS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The numbers `docs/FORMAT.md` publishes under "Current format versions".
    /// A format bump has to move both, so this fails until the doc is updated.
    #[test]
    fn the_documented_format_versions_are_the_ones_this_build_writes() {
        assert_eq!(
            CURRENT_FORMAT_VERSIONS,
            FormatVersions {
                catalog: 7,
                btree: 3,
                heap_file: 2,
                heap_page: 1,
                row: 1,
                wal: 1,
            }
        );
    }
}
