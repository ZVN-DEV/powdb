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

    /// Path to the published format table, resolved at run time rather than
    /// with `include_str!` so the doc never becomes a compile input of the
    /// published crate (`docs/` is outside the package).
    fn format_doc_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/FORMAT.md")
    }

    /// The "Current writer" numbers `docs/FORMAT.md` publishes for one row of
    /// the "Current format versions" table, keyed by the leading cell.
    ///
    /// The cells are prose ranges ("5; 6 once the first expression index
    /// activates it (since v0.13.0); 7 ..."), so the parse strips the
    /// parenthesised asides first: they carry release numbers like `v0.13.0`
    /// that are not format versions. What is left is the set of versions the
    /// doc says this build may write, and the constant has to be one of them.
    fn documented_writer_versions(doc: &str, row_label: &str) -> Vec<u32> {
        let table = doc
            .split_once("## Current format versions")
            .unwrap_or_else(|| panic!("docs/FORMAT.md has no \"Current format versions\" section"))
            .1;
        let table = table
            .split("\n## ")
            .next()
            .expect("split always yields one");
        let row = table
            .lines()
            .filter(|line| line.starts_with('|'))
            .find(|line| {
                line.split('|')
                    .nth(1)
                    .is_some_and(|c| c.trim() == row_label)
            })
            .unwrap_or_else(|| panic!("no row labelled {row_label:?} in the format-version table"));
        let cell = row
            .split('|')
            .nth(3)
            .unwrap_or_else(|| panic!("row {row_label:?} has no \"Current writer\" column"));
        let mut prose = String::with_capacity(cell.len());
        let mut depth = 0usize;
        for ch in cell.chars() {
            match ch {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                _ if depth == 0 => prose.push(ch),
                _ => {}
            }
        }
        let mut versions = Vec::new();
        let mut digits = String::new();
        for ch in prose.chars().chain(std::iter::once(' ')) {
            if ch.is_ascii_digit() {
                digits.push(ch);
            } else if !digits.is_empty() {
                versions.push(digits.parse().expect("a run of ascii digits"));
                digits.clear();
            }
        }
        assert!(
            !versions.is_empty(),
            "row {row_label:?} documents no writer version at all: {cell:?}"
        );
        versions
    }

    /// The numbers `docs/FORMAT.md` publishes under "Current format versions"
    /// are read out of the doc itself, so a format bump that moves a constant
    /// without moving the table fails here. Comparing the constants against a
    /// second hand-written copy of them in this file (which is what this test
    /// used to do) can only ever prove that two literals in the same crate
    /// agree, and the doc is the artifact that goes stale.
    #[test]
    fn the_documented_format_versions_are_the_ones_this_build_writes() {
        let path = format_doc_path();
        let doc = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let current = CURRENT_FORMAT_VERSIONS;
        for (row_label, constant) in [
            ("Catalog (`catalog.bin`)", u32::from(current.catalog)),
            ("B+tree index (`*.idx`)", u32::from(current.btree)),
            ("Heap file (`*.heap`)", u32::from(current.heap_file)),
            ("Heap page", u32::from(current.heap_page)),
            ("Row payload", u32::from(current.row)),
            ("WAL (`wal.log`)", u32::from(current.wal)),
        ] {
            let documented = documented_writer_versions(&doc, row_label);
            assert!(
                documented.contains(&constant),
                "this build writes {row_label} version {constant}, which docs/FORMAT.md \
                 does not list under \"Current writer\" (it lists {documented:?}); \
                 update the table"
            );
        }
    }

    /// The parse above is what makes the doc load-bearing, so prove it reads
    /// the number it claims to: a doc whose WAL row says 9 must not agree with
    /// a build that writes WAL version 1.
    #[test]
    fn a_drifted_format_table_is_not_accepted() {
        let doc = std::fs::read_to_string(format_doc_path()).expect("read docs/FORMAT.md");
        let drifted = doc.replace(
            "| WAL (`wal.log`) | `PWAL` file header | 1 |",
            "| WAL (`wal.log`) | `PWAL` file header | 9 |",
        );
        assert_ne!(
            drifted, doc,
            "the WAL row is not spelled the way this test expects"
        );
        assert_eq!(
            documented_writer_versions(&drifted, "WAL (`wal.log`)"),
            vec![9],
            "the parse must read the drifted number, not the constant"
        );
        assert!(
            !documented_writer_versions(&drifted, "WAL (`wal.log`)")
                .contains(&u32::from(CURRENT_FORMAT_VERSIONS.wal)),
            "a drifted table must not contain the version this build writes"
        );
    }

    /// What the constants are, pinned in one place, so an accidental bump is a
    /// deliberate edit here as well as in the doc.
    #[test]
    fn the_format_version_constants_are_the_ones_this_release_pins() {
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
