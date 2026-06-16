pub const PAGE_SIZE: usize = 4096;
/// Page header layout (20 bytes):
///   [0..4]   page_id (u32)
///   [4]      page_type (u8)
///   [5]      flags (u8) — bit 0 (`FLAG_HAS_CHECKSUM`) marks a page written
///                         in the checksummed format. Pages written by older
///                         builds have this bit clear and are read without
///                         CRC verification (validate-if-present).
///   [6..8]   free_start (u16)
///   [8..16]  lsn (u64) — log sequence number of the last WAL record
///                         applied to this page. Used for idempotent
///                         WAL replay: records with LSN <= page LSN
///                         are skipped.
///   [16..20] crc32 (u32) — CRC32 (crc32fast) of the entire 4KB page with
///                          these 4 bytes treated as zero. Present only when
///                          `FLAG_HAS_CHECKSUM` is set. WS3.
pub const PAGE_HEADER_SIZE: usize = 20;
/// The pre-WS3 header size. Pages written by older builds packed row data
/// starting at byte 16 (no CRC field), so the lower bound for a valid slot
/// offset is 16, not 20 — otherwise backward-compat reads of legacy pages
/// would reject rows living in `[16..20]`. New pages reserve `[16..20]` for
/// the CRC and start data at `PAGE_HEADER_SIZE` (20).
const LEGACY_HEADER_SIZE: usize = 16;
const SLOT_COUNT_SIZE: usize = 2; // u16 at bottom of page
const SLOT_ENTRY_SIZE: usize = 4; // u16 offset + u16 length per slot
const DELETED_MARKER: u16 = 0xFFFF;

/// Maximum encoded row size that can ever fit in a single page: a fresh
/// empty page minus the slot-count word and the row's own slot entry.
/// Anything larger must be rejected at the heap boundary as a clean
/// `StorageError::RowTooLarge` — with `panic = "abort"` in release builds,
/// a panicking insert path would otherwise kill the whole server process
/// (remote DoS via one oversized `insert`).
pub const MAX_ROW_DATA_SIZE: usize =
    PAGE_SIZE - PAGE_HEADER_SIZE - SLOT_COUNT_SIZE - SLOT_ENTRY_SIZE;

/// Byte range holding the page CRC32 (WS3). Lives just after the legacy
/// 16-byte header so the slot directory at the bottom of the page is
/// untouched — old and new pages share the same slot/slot_count layout,
/// which keeps the mmap fast paths in heap.rs format-agnostic.
const CRC_OFFSET: usize = 16;
const CRC_SIZE: usize = 4;

/// `flags` bit 0: set when the page carries a CRC32 in `[CRC_OFFSET..]`.
/// Pre-WS3 files have this clear; their pages are read without verification.
const FLAG_HAS_CHECKSUM: u8 = 0b0000_0001;
const PAGE_VERSION_SHIFT: u8 = 4;
const PAGE_VERSION_MASK: u8 = 0b1111_0000;
pub const PAGE_FORMAT_VERSION: u8 = 1;

#[inline]
fn encode_page_version(version: u8) -> u8 {
    version << PAGE_VERSION_SHIFT
}

#[inline]
pub fn page_format_version_from_flags(flags: u8) -> crate::error::Result<u8> {
    let version = (flags & PAGE_VERSION_MASK) >> PAGE_VERSION_SHIFT;
    if version > PAGE_FORMAT_VERSION {
        return Err(crate::error::StorageError::PageCorrupt(format!(
            "unsupported page format version: {version}"
        )));
    }
    Ok(version)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    Data = 1,
    Index = 2,
    Overflow = 3,
    Wal = 4,
    Meta = 5,
}

impl PageType {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(PageType::Data),
            2 => Some(PageType::Index),
            3 => Some(PageType::Overflow),
            4 => Some(PageType::Wal),
            5 => Some(PageType::Meta),
            _ => None,
        }
    }
}

/// A 4KB page with header, row data growing down, slot directory growing up.
///
/// Layout:
///   [0..4]         page_id (u32)
///   [4]            page_type (u8)
///   [5]            flags (u8)
///   [6..8]         free_start (u16)
///   [8..16]        lsn (u64)
///   [16..free_start] Row data (grows downward from header)
///   [free_start..dir_bottom] Free space
///   [dir_bottom..4094] Slot directory (grows upward): each entry is offset(u16) + length(u16)
///   [4094..4096]  slot_count(u16)
#[derive(Clone)]
pub struct Page {
    data: [u8; PAGE_SIZE],
}

impl Page {
    /// Create a fresh empty page in the checksummed (WS3) format.
    pub fn new(page_id: u32, page_type: PageType) -> Self {
        let mut data = [0u8; PAGE_SIZE];
        data[0..4].copy_from_slice(&page_id.to_le_bytes());
        data[4] = page_type as u8;
        // flags: checksum + page format version. Row data starts after the
        // full header (which includes the 4-byte CRC field). The CRC bytes
        // [16..20] are reserved and never hold row data on a checksummed page.
        data[5] = FLAG_HAS_CHECKSUM | encode_page_version(PAGE_FORMAT_VERSION);
        let free_start = PAGE_HEADER_SIZE as u16;
        data[6..8].copy_from_slice(&free_start.to_le_bytes());
        // slot_count = 0 (unchanged location: very bottom of page)
        data[PAGE_SIZE - 2..PAGE_SIZE].copy_from_slice(&0u16.to_le_bytes());
        Page { data }
    }

    /// Construct a page from raw bytes without CRC verification. Retained
    /// for callers on paths that have already validated (or intentionally
    /// skip) the checksum — e.g. WAL replay reconstructing pages, or the
    /// zero-copy mmap scan which slices `iter_page_slots` directly. Disk
    /// reads on the heap go through [`from_bytes_verified`] instead.
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() != PAGE_SIZE {
            return None;
        }
        if page_format_version_from_flags(buf[5]).is_err() {
            return None;
        }
        let mut data = [0u8; PAGE_SIZE];
        data.copy_from_slice(buf);
        Some(Page { data })
    }

    /// Construct a page from raw bytes, verifying the CRC32 if the page was
    /// written in the checksummed format (validate-if-present). Pages from
    /// pre-WS3 files (flag clear) are accepted without verification so old
    /// data files still open. Returns `PageCorrupt` on a CRC mismatch.
    pub fn from_bytes_verified(buf: &[u8]) -> crate::error::Result<Self> {
        if buf.len() != PAGE_SIZE {
            return Err(crate::error::StorageError::PageCorrupt(format!(
                "page buffer is {} bytes, expected {PAGE_SIZE}",
                buf.len()
            )));
        }
        page_format_version_from_flags(buf[5])?;
        // Validate-if-present: only pages stamped with FLAG_HAS_CHECKSUM
        // carry a CRC. Older pages are trusted (no checksum to check).
        if buf[5] & FLAG_HAS_CHECKSUM != 0 {
            let stored = u32::from_le_bytes(
                buf[CRC_OFFSET..CRC_OFFSET + CRC_SIZE]
                    .try_into()
                    .expect("4-byte CRC slice"),
            );
            let actual = checksum_with_crc_zeroed(buf);
            if stored != actual {
                let page_id = u32::from_le_bytes(buf[0..4].try_into().expect("4-byte page_id"));
                return Err(crate::error::StorageError::PageCorrupt(format!(
                    "page {page_id} CRC32 mismatch: stored {stored:#010x}, computed {actual:#010x}"
                )));
            }
        }
        let mut data = [0u8; PAGE_SIZE];
        data.copy_from_slice(buf);
        Ok(Page { data })
    }

    /// Compute the page CRC32 and write it into the header. Must be called
    /// immediately before the page is written back to disk so the stored
    /// CRC matches the final byte image. Idempotent: zeroes the CRC field
    /// before hashing so re-stamping a page yields a stable value.
    ///
    /// WS3: stamping happens on the *flush* path (once per dirty page),
    /// not per row insert/update, to keep the write-path regression small.
    pub fn stamp_checksum(&mut self) {
        self.data[5] |= FLAG_HAS_CHECKSUM | encode_page_version(PAGE_FORMAT_VERSION);
        let crc = checksum_with_crc_zeroed(&self.data);
        self.data[CRC_OFFSET..CRC_OFFSET + CRC_SIZE].copy_from_slice(&crc.to_le_bytes());
    }

    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.data
    }

    pub fn page_id(&self) -> u32 {
        // Infallible: slice is exactly 4 bytes from a fixed-size array.
        u32::from_le_bytes(self.data[0..4].try_into().expect("page_id: 4-byte slice"))
    }

    /// Returns the page type, or `None` if the type byte is invalid
    /// (e.g. from a corrupted or uninitialized page).
    pub fn page_type(&self) -> Option<PageType> {
        PageType::from_u8(self.data[4])
    }

    /// Log sequence number of the most recent WAL record applied to this
    /// page. Zero on a freshly allocated page. Used by idempotent WAL
    /// replay to skip records that have already been applied.
    pub fn lsn(&self) -> u64 {
        // SAFETY: slice is exactly 8 bytes, try_into is infallible.
        u64::from_le_bytes(self.data[8..16].try_into().expect("8-byte slice"))
    }

    /// Update the page's LSN. Should be called after applying a WAL
    /// record so replay can skip it next time.
    pub fn set_lsn(&mut self, lsn: u64) {
        self.data[8..16].copy_from_slice(&lsn.to_le_bytes());
    }

    fn free_start(&self) -> u16 {
        // Infallible: slice is exactly 2 bytes from a fixed-size array.
        u16::from_le_bytes(
            self.data[6..8]
                .try_into()
                .expect("free_start: 2-byte slice"),
        )
    }

    fn set_free_start(&mut self, v: u16) {
        self.data[6..8].copy_from_slice(&v.to_le_bytes());
    }

    pub fn slot_count(&self) -> u16 {
        // Infallible: slice is exactly 2 bytes from a fixed-size array.
        u16::from_le_bytes(
            self.data[PAGE_SIZE - 2..PAGE_SIZE]
                .try_into()
                .expect("slot_count: 2-byte slice"),
        )
    }

    fn set_slot_count(&mut self, v: u16) {
        self.data[PAGE_SIZE - 2..PAGE_SIZE].copy_from_slice(&v.to_le_bytes());
    }

    /// Byte offset where slot entry `i` starts in the page.
    /// Slot directory grows upward from the bottom (before slot_count).
    fn slot_entry_offset(&self, i: u16) -> usize {
        let required = (i as usize + 1) * SLOT_ENTRY_SIZE + SLOT_COUNT_SIZE;
        assert!(
            required <= PAGE_SIZE,
            "slot index {i} exceeds page capacity (need {required} bytes, page is {PAGE_SIZE})"
        );
        PAGE_SIZE - SLOT_COUNT_SIZE - ((i as usize + 1) * SLOT_ENTRY_SIZE)
    }

    fn read_slot_entry(&self, i: u16) -> (u16, u16) {
        let off = self.slot_entry_offset(i);
        // Infallible: slices are exactly 2 bytes each from a fixed-size array.
        let offset = u16::from_le_bytes(
            self.data[off..off + 2]
                .try_into()
                .expect("slot offset: 2-byte slice"),
        );
        let length = u16::from_le_bytes(
            self.data[off + 2..off + 4]
                .try_into()
                .expect("slot length: 2-byte slice"),
        );
        (offset, length)
    }

    fn write_slot_entry(&mut self, i: u16, offset: u16, length: u16) {
        let off = self.slot_entry_offset(i);
        self.data[off..off + 2].copy_from_slice(&offset.to_le_bytes());
        self.data[off + 2..off + 4].copy_from_slice(&length.to_le_bytes());
    }

    /// Available free space for new data + a new slot entry.
    pub fn free_space(&self) -> usize {
        let data_end = self.free_start() as usize;
        let dir_start = if self.slot_count() == 0 {
            PAGE_SIZE - SLOT_COUNT_SIZE
        } else {
            self.slot_entry_offset(self.slot_count() - 1)
        };
        dir_start.saturating_sub(data_end)
    }

    /// Insert data into the page. Returns slot index, or None if not enough space.
    pub fn insert(&mut self, row_data: &[u8]) -> Option<u16> {
        let needed = row_data.len() + SLOT_ENTRY_SIZE;
        if needed > self.free_space() {
            return None;
        }
        let slot_idx = self.slot_count();
        let offset = self.free_start();

        // Write row data
        let start = offset as usize;
        let end = start + row_data.len();
        self.data[start..end].copy_from_slice(row_data);

        // Write slot entry
        self.write_slot_entry(slot_idx, offset, row_data.len() as u16);

        // Update header
        self.set_free_start(end as u16);
        self.set_slot_count(slot_idx + 1);

        Some(slot_idx)
    }

    /// True if this page's header is uninitialised — a zero page produced by
    /// `DiskManager::allocate_page`'s file extension that was never written
    /// with a valid layout (its `free_start` reads 0 instead of
    /// `PAGE_HEADER_SIZE`). WAL replay uses this to detect and re-initialise
    /// such a page before placing a row on it. A valid page — empty or with a
    /// persisted prefix — always has `free_start >= PAGE_HEADER_SIZE`.
    pub fn is_blank(&self) -> bool {
        (self.free_start() as usize) < PAGE_HEADER_SIZE
    }

    /// Place `row_data` so it occupies exactly slot index `slot`. Used only
    /// by WAL replay to reconstruct the original RowId layout deterministically
    /// (the normal `insert` self-assigns the next slot, which can diverge from
    /// the logged RowId after a partial-flush crash and orphan later
    /// Update/Delete records).
    ///
    /// - `slot < slot_count`: the slot already exists (it was part of a
    ///   persisted prefix). Idempotent no-op — returns `true`.
    /// - `slot == slot_count`: append, identical to `insert`.
    /// - `slot > slot_count`: bridge the gap with tombstone (deleted) slot
    ///   entries so the directory indices line up, then append the row. Gaps
    ///   are not expected under prefix-persistence but are handled so a
    ///   surprising log can never silently shift RowIds.
    ///
    /// Returns `false` if the row plus any required slot entries don't fit.
    pub fn insert_at_slot(&mut self, slot: u16, row_data: &[u8]) -> bool {
        let sc = self.slot_count();
        if slot < sc {
            return true; // already present (persisted prefix) — idempotent
        }
        // Slot entries needed: one tombstone per gap slot in [sc, slot), plus
        // the row's own entry at `slot`.
        let new_entries = (slot - sc) as usize + 1;
        let needed = row_data.len() + SLOT_ENTRY_SIZE * new_entries;
        if needed > self.free_space() {
            return false;
        }
        // Bridge the gap with tombstones (offset 0, length = DELETED_MARKER).
        while self.slot_count() < slot {
            let idx = self.slot_count();
            self.write_slot_entry(idx, 0, DELETED_MARKER);
            self.set_slot_count(idx + 1);
        }
        // Append the row at `slot` (== current slot_count).
        let idx = self.slot_count();
        let offset = self.free_start();
        let start = offset as usize;
        let end = start + row_data.len();
        self.data[start..end].copy_from_slice(row_data);
        self.write_slot_entry(idx, offset, row_data.len() as u16);
        self.set_free_start(end as u16);
        self.set_slot_count(idx + 1);
        true
    }

    /// Read data at slot index. Returns None if slot is deleted or out of range.
    pub fn get(&self, slot: u16) -> Option<&[u8]> {
        if slot >= self.slot_count() {
            return None;
        }
        let (offset, length) = self.read_slot_entry(slot);
        if length == DELETED_MARKER {
            return None;
        }
        let start = offset as usize;
        let end = start + length as usize;
        Some(&self.data[start..end])
    }

    /// Mutable view into an existing slot's raw bytes. The returned slice is
    /// exactly as long as the current row encoding — the caller MUST NOT
    /// resize it. Used by fixed-size column update fast paths that patch a
    /// field in place without re-encoding the whole row.
    ///
    /// Mission C Phase 4: the old update path went through decode_row +
    /// Vec<Value> allocation + encode_row_into + page.update, even when the
    /// change was a single 8-byte int. This primitive lets the executor skip
    /// all of that by writing the new bytes directly into the page.
    #[inline]
    pub fn slot_bytes_mut(&mut self, slot: u16) -> Option<&mut [u8]> {
        if slot >= self.slot_count() {
            return None;
        }
        let (offset, length) = self.read_slot_entry(slot);
        if length == DELETED_MARKER {
            return None;
        }
        let start = offset as usize;
        let end = start + length as usize;
        Some(&mut self.data[start..end])
    }

    /// Shrink an existing slot's recorded length. The caller has already
    /// written the new bytes into the first `new_len` bytes of the slot —
    /// this call only updates the slot-directory entry. Returns `false` if
    /// the slot is deleted, out of range, or `new_len` > current length
    /// (growth is not supported by this primitive).
    ///
    /// Mission C Phase 10: backs the var-column in-place update fast path.
    /// For `update { status := "senior" }` over a 50K-row filter, the old
    /// values cycle through "active"/"inactive"/"pending" (≥ 6 bytes) and
    /// the new value is 6 bytes — every row shrinks or matches, so we can
    /// patch the slot bytes directly and then truncate via this method
    /// instead of re-encoding the whole row.
    #[inline]
    pub fn shrink_slot(&mut self, slot: u16, new_len: u16) -> bool {
        if slot >= self.slot_count() {
            return false;
        }
        let (offset, old_length) = self.read_slot_entry(slot);
        if old_length == DELETED_MARKER {
            return false;
        }
        if new_len > old_length {
            return false;
        }
        self.write_slot_entry(slot, offset, new_len);
        true
    }

    /// Mark a slot as deleted. Does not reclaim space (compaction is separate).
    pub fn delete(&mut self, slot: u16) {
        if slot < self.slot_count() {
            let (offset, _) = self.read_slot_entry(slot);
            self.write_slot_entry(slot, offset, DELETED_MARKER);
        }
    }

    /// Update data in a slot in place if it fits, otherwise append at free_start.
    pub fn update(&mut self, slot: u16, row_data: &[u8]) -> bool {
        if slot >= self.slot_count() {
            return false;
        }
        let (offset, old_length) = self.read_slot_entry(slot);
        if old_length == DELETED_MARKER {
            return false;
        }
        if row_data.len() <= old_length as usize {
            let start = offset as usize;
            self.data[start..start + row_data.len()].copy_from_slice(row_data);
            self.write_slot_entry(slot, offset, row_data.len() as u16);
            true
        } else {
            // Need more space — append at free_start
            if row_data.len() > self.free_space() {
                return false;
            }
            let new_offset = self.free_start();
            let start = new_offset as usize;
            self.data[start..start + row_data.len()].copy_from_slice(row_data);
            self.write_slot_entry(slot, new_offset, row_data.len() as u16);
            self.set_free_start((start + row_data.len()) as u16);
            true
        }
    }

    /// Iterate over all live (non-deleted) slots. Returns (slot_index, data).
    pub fn iter(&self) -> impl Iterator<Item = (u16, &[u8])> {
        (0..self.slot_count()).filter_map(move |i| self.get(i).map(|data| (i, data)))
    }
}

/// Iterate live slots directly from a page-sized byte slice without copying.
/// Used by mmap-based scans to avoid the 4KB memcpy in `Page::from_bytes`.
///
/// Mission F: `#[inline]` so the slot-walking closure can fold into the
/// `for_each_row` mmap loop in heap.rs. With LTO this becomes a tight loop
/// over `entry_off` with no function call per slot.
/// Read the LSN from a page-sized byte slice without constructing a `Page`.
/// Used by WAL replay to check whether a record has already been applied.
/// CRC32 (crc32fast) over a full 4KB page image, treating the 4-byte CRC
/// field as zero. Used both to stamp a page on write-back and to verify it
/// on read — the "with CRC zeroed" convention means the same input bytes
/// produce the same hash regardless of any stale CRC already present.
#[inline]
fn checksum_with_crc_zeroed(page_bytes: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    // Hash everything before the CRC field, four zero bytes in its place,
    // then everything after — no allocation, no full-page copy.
    hasher.update(&page_bytes[..CRC_OFFSET]);
    hasher.update(&[0u8; CRC_SIZE]);
    hasher.update(&page_bytes[CRC_OFFSET + CRC_SIZE..]);
    hasher.finalize()
}

#[inline]
pub fn page_lsn(page_bytes: &[u8]) -> u64 {
    u64::from_le_bytes(
        page_bytes[8..16]
            .try_into()
            .expect("page_lsn: 8-byte slice"),
    )
}

#[inline]
pub fn iter_page_slots(page_bytes: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    // SAFETY: slice is exactly 2 bytes, try_into is infallible.
    let slot_count = u16::from_le_bytes(
        page_bytes[PAGE_SIZE - 2..PAGE_SIZE]
            .try_into()
            .expect("slot_count: 2-byte slice"),
    );
    (0..slot_count).filter_map(move |i| {
        let entry_off = PAGE_SIZE - SLOT_COUNT_SIZE - ((i as usize + 1) * SLOT_ENTRY_SIZE);
        // SAFETY: slices are exactly 2 bytes each, try_into is infallible.
        let offset = u16::from_le_bytes(
            page_bytes[entry_off..entry_off + 2]
                .try_into()
                .expect("slot offset: 2-byte slice"),
        );
        let length = u16::from_le_bytes(
            page_bytes[entry_off + 2..entry_off + 4]
                .try_into()
                .expect("slot length: 2-byte slice"),
        );
        if length == DELETED_MARKER {
            return None;
        }
        let start = offset as usize;
        let end = start + length as usize;
        // Task 3: bounds validation — a corrupt page could have slot
        // offset/length that point outside the data region. Return None
        // instead of panicking on an out-of-bounds slice. Use the legacy
        // header size as the lower bound so pre-WS3 pages (data from byte
        // 16) still read.
        if end > PAGE_SIZE || start < LEGACY_HEADER_SIZE {
            return None;
        }
        Some((i, &page_bytes[start..end]))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_page() {
        let page = Page::new(0, PageType::Data);
        assert_eq!(page.page_id(), 0);
        assert_eq!(page.page_type(), Some(PageType::Data));
        assert_eq!(page.slot_count(), 0);
        assert_eq!(page.lsn(), 0);
        assert_eq!(
            page.free_space(),
            PAGE_SIZE - PAGE_HEADER_SIZE - SLOT_COUNT_SIZE
        );
    }

    #[test]
    fn test_insert_and_read_slot() {
        let mut page = Page::new(1, PageType::Data);
        let data = b"hello world";
        let slot = page.insert(data).expect("insert should succeed");
        assert_eq!(slot, 0);
        assert_eq!(page.slot_count(), 1);
        assert_eq!(page.get(0).unwrap(), data);
    }

    #[test]
    fn test_multiple_inserts() {
        let mut page = Page::new(1, PageType::Data);
        let s0 = page.insert(b"first").unwrap();
        let s1 = page.insert(b"second").unwrap();
        let s2 = page.insert(b"third").unwrap();
        assert_eq!(s0, 0);
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
        assert_eq!(page.get(0).unwrap(), b"first");
        assert_eq!(page.get(1).unwrap(), b"second");
        assert_eq!(page.get(2).unwrap(), b"third");
    }

    #[test]
    fn test_page_full() {
        let mut page = Page::new(1, PageType::Data);
        let big = vec![0u8; PAGE_SIZE];
        assert!(page.insert(&big).is_none());
    }

    #[test]
    fn test_delete_slot() {
        let mut page = Page::new(1, PageType::Data);
        page.insert(b"keep");
        page.insert(b"delete me");
        page.insert(b"keep too");
        page.delete(1);
        assert!(page.get(1).is_none());
        assert_eq!(page.get(0).unwrap(), b"keep");
        assert_eq!(page.get(2).unwrap(), b"keep too");
    }

    #[test]
    fn test_page_serialization_roundtrip() {
        let mut page = Page::new(42, PageType::Data);
        page.insert(b"hello");
        page.insert(b"world");
        let buf = page.as_bytes();
        assert_eq!(buf.len(), PAGE_SIZE);
        let page2 = Page::from_bytes(buf).unwrap();
        assert_eq!(page2.page_id(), 42);
        assert_eq!(page2.slot_count(), 2);
        assert_eq!(page2.get(0).unwrap(), b"hello");
        assert_eq!(page2.get(1).unwrap(), b"world");
    }

    #[test]
    fn test_update_in_place() {
        let mut page = Page::new(1, PageType::Data);
        page.insert(b"hello world!!");
        assert!(page.update(0, b"hi world")); // smaller — fits in place
        assert_eq!(page.get(0).unwrap(), b"hi world");
    }

    #[test]
    fn test_update_larger_appends() {
        let mut page = Page::new(1, PageType::Data);
        page.insert(b"hi");
        let free_before = page.free_space();
        assert!(page.update(0, b"hello world much longer")); // larger — appends
        assert_eq!(page.get(0).unwrap(), b"hello world much longer");
        assert!(page.free_space() < free_before);
    }

    #[test]
    fn test_shrink_slot() {
        let mut page = Page::new(1, PageType::Data);
        page.insert(b"hello world!!").unwrap();
        assert!(page.shrink_slot(0, 5));
        assert_eq!(page.get(0).unwrap(), b"hello");
        // Growing is rejected.
        assert!(!page.shrink_slot(0, 100));
        // Deleted slot is rejected.
        page.delete(0);
        assert!(!page.shrink_slot(0, 1));
    }

    #[test]
    fn test_iter_skips_deleted() {
        let mut page = Page::new(1, PageType::Data);
        page.insert(b"a");
        page.insert(b"b");
        page.insert(b"c");
        page.delete(1);
        let live: Vec<_> = page.iter().collect();
        assert_eq!(live.len(), 2);
        assert_eq!(live[0], (0, &b"a"[..]));
        assert_eq!(live[1], (2, &b"c"[..]));
    }

    #[test]
    fn test_checksum_roundtrip_and_corruption_detected() {
        // WS3 Test 1: stamp a page, flip a byte in the data region, and
        // assert verification fails with PageCorrupt.
        let mut page = Page::new(7, PageType::Data);
        page.insert(b"the quick brown fox").unwrap();
        page.insert(b"jumps over the lazy dog").unwrap();
        page.stamp_checksum();

        // A clean round-trip verifies fine.
        let bytes = *page.as_bytes();
        assert!(
            Page::from_bytes_verified(&bytes).is_ok(),
            "freshly stamped page must verify"
        );

        // Flip a byte inside the row-data region (well clear of the CRC
        // field and the slot directory) — verification must reject it.
        let mut corrupted = bytes;
        corrupted[PAGE_HEADER_SIZE + 3] ^= 0xFF;
        match Page::from_bytes_verified(&corrupted) {
            Err(crate::error::StorageError::PageCorrupt(_)) => {}
            Err(other) => panic!("expected PageCorrupt on flipped data byte, got {other:?}"),
            Ok(_) => panic!("expected PageCorrupt on flipped data byte, got Ok"),
        }
    }

    #[test]
    fn test_legacy_page_without_checksum_still_reads() {
        // WS3 Test 2: a page in the pre-checksum format (flag clear, no
        // CRC stored) must still open without error — validate-if-present.
        let mut page = Page::new(9, PageType::Data);
        page.insert(b"legacy row").unwrap();
        // Simulate the old on-disk format: clear the checksum flag and zero
        // the CRC field so it looks exactly like a page written by a
        // pre-WS3 build.
        let mut bytes = *page.as_bytes();
        bytes[5] &= !FLAG_HAS_CHECKSUM;
        bytes[CRC_OFFSET..CRC_OFFSET + CRC_SIZE].copy_from_slice(&[0u8; CRC_SIZE]);

        let reopened =
            Page::from_bytes_verified(&bytes).expect("legacy page must read without verification");
        assert_eq!(reopened.page_id(), 9);
        assert_eq!(reopened.get(0).unwrap(), b"legacy row");
    }

    #[test]
    fn test_stamp_checksum_is_idempotent() {
        // Re-stamping a page (e.g. flushed, mutated, flushed again) must
        // produce a self-consistent CRC each time, since the field is
        // zeroed before hashing.
        let mut page = Page::new(3, PageType::Data);
        page.insert(b"row").unwrap();
        page.stamp_checksum();
        let first = *page.as_bytes();
        page.stamp_checksum();
        let second = *page.as_bytes();
        assert_eq!(
            first, second,
            "re-stamping an unchanged page must be stable"
        );
        assert!(Page::from_bytes_verified(&second).is_ok());
    }

    #[test]
    fn test_fill_page_to_capacity() {
        let mut page = Page::new(0, PageType::Data);
        let mut count = 0u16;
        // Insert 10-byte rows until full
        while page.insert(&[0u8; 10]).is_some() {
            count += 1;
        }
        // 4096 - 16 (header) - 2 (slot_count) = 4078 usable
        // Each row: 10 data + 4 slot entry = 14 bytes
        // 4078 / 14 = 291 rows
        assert!(
            count > 280 && count <= 292,
            "expected ~291 rows, got {count}"
        );
        assert_eq!(page.slot_count(), count);
    }
}
