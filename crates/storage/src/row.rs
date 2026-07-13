use crate::types::*;
use std::io;

pub const ROW_MAGIC: &[u8; 4] = b"PROW";
/// The version a row is stamped with when every value fits inline. v0.11
/// keeps this at 1 so a database that never spills produces byte-identical
/// rows to old builds and stays backward-openable. v2 is stamped ONLY for
/// rows carrying at least one out-of-line (spilled) value; see
/// [`encode_row_v2_into`] and [`MAX_ROW_FORMAT_VERSION`].
pub const ROW_FORMAT_VERSION: u16 = 1;
/// Row format v2 = v1 + an overflow bitmap (one bit per var column, in
/// declaration order). A set bit means that var column's var-data slot holds
/// a 24-byte [`OverflowStub`] rather than the inline value. This is the only
/// row-format migration that will ever ship (door D1: no u32 row format).
pub const ROW_FORMAT_VERSION_V2: u16 = 2;
/// The highest row format version this build can read. New code accepts 1
/// and 2; legacy prefix-less rows read as version 0.
pub const MAX_ROW_FORMAT_VERSION: u16 = ROW_FORMAT_VERSION_V2;
pub const ROW_PREFIX_SIZE: usize = 6;

/// Fixed on-disk size of an overflow stub (door D2). Persisted inside a v2
/// row's var-data slot in place of a spilled value.
pub const OVERFLOW_STUB_SIZE: usize = 24;
/// Current stub layout revision, written into `stub_version`. The reserved
/// bytes, the flags byte, and this version field together are the escape hatch
/// for compression and layout evolution without another row-format bump.
pub const OVERFLOW_STUB_VERSION: u8 = 1;
/// Values below this size are never evicted to overflow unless full eviction
/// is the only way to make the row fit (spill policy step 4).
pub const OVERFLOW_MIN: usize = 256;
/// Documented engine limit on a single out-of-line value. A config-raisable
/// constant, NOT an on-disk format constant (the stub's length field is u64,
/// so the format itself has no cap). Values above this are a typed error.
pub const MAX_VALUE_SIZE: usize = 64 * 1024 * 1024;

/// The permanent on-disk pointer to an out-of-line value (door D2). Persisted
/// inside a v2 row's var-data slot in place of the spilled value.
///
/// ```text
/// offset size field
/// 0      8    total_len      u64  logical byte length of the full value
/// 8      4    first_page     u32  head of the overflow chain
/// 12     4    value_crc32    u32  CRC32 of the fully assembled value
/// 16     1    flags          u8   bit0 compressed (RESERVED), bit1 chunk-aligned (RESERVED)
/// 17     1    stub_version   u8   = OVERFLOW_STUB_VERSION
/// 18     6    reserved            zeroed
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverflowStub {
    pub total_len: u64,
    pub first_page: u32,
    pub value_crc32: u32,
    pub flags: u8,
    pub stub_version: u8,
}

impl OverflowStub {
    pub fn new(total_len: u64, first_page: u32, value_crc32: u32) -> Self {
        OverflowStub {
            total_len,
            first_page,
            value_crc32,
            flags: 0,
            stub_version: OVERFLOW_STUB_VERSION,
        }
    }

    pub fn to_bytes(&self) -> [u8; OVERFLOW_STUB_SIZE] {
        let mut b = [0u8; OVERFLOW_STUB_SIZE];
        b[0..8].copy_from_slice(&self.total_len.to_le_bytes());
        b[8..12].copy_from_slice(&self.first_page.to_le_bytes());
        b[12..16].copy_from_slice(&self.value_crc32.to_le_bytes());
        b[16] = self.flags;
        b[17] = self.stub_version;
        // bytes [18..24] stay zeroed (reserved).
        b
    }

    /// Parse a stub from a 24-byte slot. Returns `None` if the slice is the
    /// wrong length or the stub_version is unrecognised (forward-incompatible).
    pub fn from_bytes(b: &[u8]) -> Option<OverflowStub> {
        if b.len() != OVERFLOW_STUB_SIZE {
            return None;
        }
        let stub_version = b[17];
        if stub_version != OVERFLOW_STUB_VERSION {
            return None;
        }
        Some(OverflowStub {
            total_len: u64::from_le_bytes(b[0..8].try_into().expect("8-byte slice")),
            first_page: u32::from_le_bytes(b[8..12].try_into().expect("4-byte slice")),
            value_crc32: u32::from_le_bytes(b[12..16].try_into().expect("4-byte slice")),
            flags: b[16],
            stub_version,
        })
    }
}

#[inline]
pub fn row_format_version(data: &[u8]) -> io::Result<u16> {
    if data.len() >= ROW_PREFIX_SIZE && &data[0..4] == ROW_MAGIC {
        let version = u16::from_le_bytes(data[4..6].try_into().expect("2-byte row version"));
        if version > MAX_ROW_FORMAT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported row format version: {version}"),
            ));
        }
        Ok(version)
    } else {
        Ok(0)
    }
}

/// Fast, allocation-free check of whether a stored row is in format v2 (has an
/// overflow bitmap and possibly out-of-line values). The compiled-predicate
/// scan routing reads exactly this — bytes 4..6, adjacent to data already
/// touched — to decide whether a row can take the zero-copy compiled path
/// (v0/v1) or must reassemble through the overflow chains (v2).
#[inline]
pub fn row_is_v2(data: &[u8]) -> bool {
    data.len() >= ROW_PREFIX_SIZE
        && &data[0..4] == ROW_MAGIC
        && u16::from_le_bytes([data[4], data[5]]) == ROW_FORMAT_VERSION_V2
}

#[inline]
pub fn validate_row_format(data: &[u8]) -> io::Result<()> {
    let _ = row_format_version(data)?;
    Ok(())
}

#[inline]
fn row_body_offset(data: &[u8]) -> io::Result<usize> {
    if data.len() >= ROW_PREFIX_SIZE && &data[0..4] == ROW_MAGIC {
        row_format_version(data)?;
        Ok(ROW_PREFIX_SIZE)
    } else {
        Ok(0)
    }
}

#[inline]
fn row_body(data: &[u8]) -> &[u8] {
    let offset = row_body_offset(data).expect("unsupported row format version");
    &data[offset..]
}

fn prepend_row_prefix(out: &mut Vec<u8>) {
    let body_len = out.len();
    out.resize(body_len + ROW_PREFIX_SIZE, 0);
    out.copy_within(0..body_len, ROW_PREFIX_SIZE);
    out[0..4].copy_from_slice(ROW_MAGIC);
    out[4..6].copy_from_slice(&ROW_FORMAT_VERSION.to_le_bytes());
}

/// Encode a row of values into the compact binary format.
///
/// Layout: `["PROW": 4 bytes] [version: u16] [body]`. The body keeps the
/// legacy layout: `[length: u16] [null_bitmap] [fixed columns packed]
/// [var offset table] [var data]`. Legacy rows without the prefix still decode
/// as row format version 0.
///
/// Fixed columns are written in schema order, with placeholder zeros for Empty values.
/// Variable columns use an offset table (n_var + 1 entries) pointing into var data.
/// Overhead: 6 bytes (prefix) + 2 bytes (length) + ceil(n_cols/8) bytes (bitmap).
///
/// Mission C Phase 2: kept as a thin wrapper around [`encode_row_into`] so
/// existing tests continue to work. Hot callers (bench insert/update loops)
/// should go through `encode_row_into` and reuse the output buffer.
pub fn encode_row(schema: &Schema, values: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_row_into(schema, values, &mut out);
    out
}

/// Fallible version of [`encode_row`] — returns an error if the row exceeds
/// the 64KB size limit.
pub fn try_encode_row(schema: &Schema, values: &[Value]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    try_encode_row_into(schema, values, &mut out)?;
    Ok(out)
}

/// Encode a row into a caller-provided scratch buffer.
///
/// Mission C Phase 2: the previous `encode_row` allocated 5-6 temporary Vecs
/// per call (null bitmap, fixed buf, var indices, var data, var offsets,
/// final buf). On the `update_by_filter` bench that fired ~50K times. The
/// rewrite below walks the schema twice and writes straight into `out`,
/// reusing the buffer's backing store between calls.
///
/// Mission C Phase 19: thin wrapper around [`encode_row_into_with_layout`]
/// that builds a transient `RowLayout` on every call. Hot callers (inserts,
/// updates) should construct the layout once on `Table` and pass it in
/// directly — that skips the schema-walk entirely and fuses the sizing pass
/// with the bitmap pass.
///
/// Contract:
/// - `out` is cleared and filled with exactly the encoded row bytes.
/// - No allocations happen if `out.capacity()` is already large enough
///   (the common case after the first insert of a given shape).
pub fn encode_row_into(schema: &Schema, values: &[Value], out: &mut Vec<u8>) {
    let layout = RowLayout::new(schema);
    encode_row_into_with_layout(schema, &layout, values, out);
}

/// Fallible version of [`encode_row_into`] — returns an error if the row
/// exceeds the 64KB size limit.
pub fn try_encode_row_into(schema: &Schema, values: &[Value], out: &mut Vec<u8>) -> io::Result<()> {
    let layout = RowLayout::new(schema);
    try_encode_row_into_with_layout(schema, &layout, values, out)
}

/// Encode a row using a precomputed [`RowLayout`].
///
/// Mission C Phase 19: the former `encode_row_into` walked `schema.columns`
/// four separate times (size, fixed, var offsets, var data) and the value
/// slice three times (size, bitmap, fixed/var). For the `insert_batch_1k`
/// bench this added up to ~117ns out of a 232ns per-row budget. This
/// rewrite:
///
///   1. Takes the layout as an argument so we skip recomputing
///      `fixed_region_size`, `n_var`, and `bitmap_size` on every call.
///   2. Fuses the sizing pass with the bitmap pass: a single walk over
///      `values[]` both computes `var_data_size` and materialises the null
///      bitmap into a stack-local `[u8; 32]` buffer (supports ≤256 cols).
///   3. `resize`s `out` to the final size exactly once, all zeroed. That
///      automatically handles placeholder-zero writes for null fixed
///      columns — no branches, no per-column `extend_from_slice`.
///   4. Walks `schema.columns` one final time to emit fixed columns at
///      their precomputed offsets and var columns with fused offset-table
///      + payload writes (no second pass over var cols for data).
///
/// All mutation into `out` is via indexed writes, so the compiler can
/// hoist bounds checks and vectorise the common `copy_from_slice` calls.
#[inline]
pub fn encode_row_into_with_layout(
    schema: &Schema,
    layout: &RowLayout,
    values: &[Value],
    out: &mut Vec<u8>,
) {
    debug_assert_eq!(values.len(), schema.columns.len());

    let n_cols = schema.columns.len();
    let bitmap_size = layout.bitmap_size;
    let fixed_region_size = layout.fixed_region_size;
    let n_var = layout.n_var;
    let n_offsets = n_var + 1;

    // Fused pre-pass: compute null bitmap + var data size in a single walk.
    // Stack-local bitmap supports schemas up to 256 columns without any
    // heap touch. Wider schemas fall back to the (rare) heap path below.
    let mut bitmap_stack = [0u8; 32];
    let mut bitmap_heap: Vec<u8>;
    let bitmap_slice: &mut [u8] = if bitmap_size <= 32 {
        &mut bitmap_stack[..bitmap_size]
    } else {
        bitmap_heap = vec![0u8; bitmap_size];
        &mut bitmap_heap[..]
    };

    let mut var_data_size: usize = 0;
    for (i, val) in values.iter().enumerate() {
        match val {
            Value::Empty => {
                bitmap_slice[i >> 3] |= 1 << (i & 7);
            }
            Value::Str(s) => var_data_size += s.len(),
            Value::Bytes(b) => var_data_size += b.len(),
            _ => {}
        }
    }

    let body_size = bitmap_size + fixed_region_size + n_offsets * 2 + var_data_size;
    let total_size = 2 + body_size;

    // Guard: individual var-column lengths and total row size must fit in u16.
    // The infallible encode path panics in debug mode; callers handling
    // untrusted data should use `try_encode_row_into_with_layout` instead.
    debug_assert!(
        total_size <= u16::MAX as usize,
        "row too large: {total_size} bytes exceeds 64KB limit"
    );
    debug_assert!(
        var_data_size <= u16::MAX as usize,
        "variable data too large: {var_data_size} bytes exceeds 64KB limit"
    );

    // One resize → zeroed buffer. This subsumes: placeholder zeros for
    // null fixed columns, zero-init of the offset table, and the end
    // sentinel (implicitly zero if no var cols).
    out.clear();
    out.resize(total_size, 0);

    // Length prefix.
    out[0..2].copy_from_slice(&(total_size as u16).to_le_bytes());

    // Bitmap — bulk copy from the stack/heap scratch buffer.
    let bitmap_start = 2;
    out[bitmap_start..bitmap_start + bitmap_size].copy_from_slice(bitmap_slice);

    let fixed_start = bitmap_start + bitmap_size;
    let offsets_start = fixed_start + fixed_region_size;
    let var_data_start = offsets_start + n_offsets * 2;

    // Single pass over columns: fixed writes at precomputed offsets, var
    // writes update the offset table and stream payload into var data.
    let mut var_cursor: u16 = 0;
    let mut off_slot: usize = 0;

    for (i, val) in values.iter().enumerate().take(n_cols) {
        if let Some(off) = layout.fixed_offsets[i] {
            // Nulls already zero from the up-front resize.
            let pos = fixed_start + off;
            match val {
                Value::Empty => {}
                Value::Int(v) => {
                    out[pos..pos + 8].copy_from_slice(&v.to_le_bytes());
                }
                Value::Float(v) => {
                    out[pos..pos + 8].copy_from_slice(&v.to_le_bytes());
                }
                Value::Bool(v) => {
                    out[pos] = if *v { 1 } else { 0 };
                }
                Value::DateTime(v) => {
                    out[pos..pos + 8].copy_from_slice(&v.to_le_bytes());
                }
                Value::Uuid(v) => {
                    out[pos..pos + 16].copy_from_slice(v);
                }
                _ => unreachable!("fixed column with non-fixed value"),
            }
        } else {
            // Variable column — write offset, then stream payload.
            let off_pos = offsets_start + off_slot * 2;
            out[off_pos..off_pos + 2].copy_from_slice(&var_cursor.to_le_bytes());
            off_slot += 1;

            match val {
                Value::Empty => {} // zero-length, nothing to append
                Value::Str(s) => {
                    let len = s.len();
                    let abs = var_data_start + var_cursor as usize;
                    out[abs..abs + len].copy_from_slice(s.as_bytes());
                    var_cursor += len as u16;
                }
                Value::Bytes(b) => {
                    let len = b.len();
                    let abs = var_data_start + var_cursor as usize;
                    out[abs..abs + len].copy_from_slice(b);
                    var_cursor += len as u16;
                }
                _ => unreachable!("variable column with non-variable value"),
            }
        }
    }

    // End sentinel for the offset table.
    let end_pos = offsets_start + off_slot * 2;
    out[end_pos..end_pos + 2].copy_from_slice(&var_cursor.to_le_bytes());

    debug_assert_eq!(out.len(), total_size);
    prepend_row_prefix(out);
}

/// Fallible version of [`encode_row_into_with_layout`] — returns an error
/// if the total row size or any individual variable-length value exceeds
/// the 64KB u16 limit. Callers that receive values from user queries should
/// use this to prevent silent data corruption from u16 truncation.
pub fn try_encode_row_into_with_layout(
    schema: &Schema,
    layout: &RowLayout,
    values: &[Value],
    out: &mut Vec<u8>,
) -> io::Result<()> {
    let n_cols = schema.columns.len();
    let bitmap_size = layout.bitmap_size;
    let fixed_region_size = layout.fixed_region_size;
    let n_var = layout.n_var;
    let n_offsets = n_var + 1;

    let mut bitmap_stack = [0u8; 32];
    let mut bitmap_heap: Vec<u8>;
    let bitmap_slice: &mut [u8] = if bitmap_size <= 32 {
        &mut bitmap_stack[..bitmap_size]
    } else {
        bitmap_heap = vec![0u8; bitmap_size];
        &mut bitmap_heap[..]
    };

    let mut var_data_size: usize = 0;
    for (i, val) in values.iter().enumerate() {
        match val {
            Value::Empty => {
                bitmap_slice[i >> 3] |= 1 << (i & 7);
            }
            Value::Str(s) => {
                if s.len() > u16::MAX as usize {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "row too large: string value in column '{}' is {} bytes, exceeds 64KB limit",
                            schema.columns[i].name, s.len()
                        ),
                    ));
                }
                var_data_size += s.len();
            }
            Value::Bytes(b) => {
                if b.len() > u16::MAX as usize {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "row too large: bytes value in column '{}' is {} bytes, exceeds 64KB limit",
                            schema.columns[i].name, b.len()
                        ),
                    ));
                }
                var_data_size += b.len();
            }
            _ => {}
        }
    }

    let body_size = bitmap_size + fixed_region_size + n_offsets * 2 + var_data_size;
    let total_size = 2 + body_size;

    if total_size > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("row too large: {total_size} bytes exceeds 64KB limit"),
        ));
    }
    if var_data_size > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("row too large: variable data is {var_data_size} bytes, exceeds 64KB limit"),
        ));
    }

    // Delegate to the infallible version now that bounds are verified.
    // The data is already validated above, so the infallible path won't
    // encounter any truncation.
    out.clear();
    out.resize(total_size, 0);
    out[0..2].copy_from_slice(&(total_size as u16).to_le_bytes());
    let bitmap_start = 2;
    out[bitmap_start..bitmap_start + bitmap_size].copy_from_slice(bitmap_slice);

    let fixed_start = bitmap_start + bitmap_size;
    let offsets_start = fixed_start + fixed_region_size;
    let var_data_start = offsets_start + n_offsets * 2;

    let mut var_cursor: u16 = 0;
    let mut off_slot: usize = 0;

    for (i, val) in values.iter().enumerate().take(n_cols) {
        if let Some(off) = layout.fixed_offsets[i] {
            let pos = fixed_start + off;
            match val {
                Value::Empty => {}
                Value::Int(v) => {
                    out[pos..pos + 8].copy_from_slice(&v.to_le_bytes());
                }
                Value::Float(v) => {
                    out[pos..pos + 8].copy_from_slice(&v.to_le_bytes());
                }
                Value::Bool(v) => {
                    out[pos] = if *v { 1 } else { 0 };
                }
                Value::DateTime(v) => {
                    out[pos..pos + 8].copy_from_slice(&v.to_le_bytes());
                }
                Value::Uuid(v) => {
                    out[pos..pos + 16].copy_from_slice(v);
                }
                _ => unreachable!("fixed column with non-fixed value"),
            }
        } else {
            let off_pos = offsets_start + off_slot * 2;
            out[off_pos..off_pos + 2].copy_from_slice(&var_cursor.to_le_bytes());
            off_slot += 1;

            match val {
                Value::Empty => {}
                Value::Str(s) => {
                    let len = s.len();
                    let abs = var_data_start + var_cursor as usize;
                    out[abs..abs + len].copy_from_slice(s.as_bytes());
                    var_cursor += len as u16;
                }
                Value::Bytes(b) => {
                    let len = b.len();
                    let abs = var_data_start + var_cursor as usize;
                    out[abs..abs + len].copy_from_slice(b);
                    var_cursor += len as u16;
                }
                _ => unreachable!("variable column with non-variable value"),
            }
        }
    }

    let end_pos = offsets_start + off_slot * 2;
    out[end_pos..end_pos + 2].copy_from_slice(&var_cursor.to_le_bytes());

    debug_assert_eq!(out.len(), total_size);
    prepend_row_prefix(out);
    Ok(())
}

/// Precomputed layout information for fast selective column decoding.
///
/// Computing offsets requires iterating through schema columns every time,
/// which is wasteful when decoding thousands of rows. This struct caches the
/// layout once so that `decode_column` can jump directly to the right byte
/// offset.
pub struct RowLayout {
    /// Byte offset within the fixed-column region for each fixed column.
    /// Variable-length columns have `None`.
    fixed_offsets: Vec<Option<usize>>,
    /// Total size of the fixed-column region in bytes.
    fixed_region_size: usize,
    /// For each column: if it is variable-length, its index within the
    /// variable-column offset table. Fixed columns have `None`.
    var_index: Vec<Option<usize>>,
    /// Total number of variable-length columns.
    n_var: usize,
    /// Size of the null bitmap in bytes.
    bitmap_size: usize,
}

impl RowLayout {
    /// Fixed byte offset for a column (None if variable-length).
    #[inline(always)]
    pub fn fixed_offset(&self, col_idx: usize) -> Option<usize> {
        self.fixed_offsets[col_idx]
    }

    /// Size of the null bitmap in bytes.
    #[inline(always)]
    pub fn bitmap_size(&self) -> usize {
        self.bitmap_size
    }

    /// Number of variable-length columns.
    #[inline(always)]
    pub fn n_var(&self) -> usize {
        self.n_var
    }

    /// Size in bytes of the v2 overflow bitmap (one bit per var column).
    /// Zero when the schema has no variable-length columns.
    #[inline(always)]
    pub fn overflow_bitmap_size(&self) -> usize {
        self.n_var.div_ceil(8)
    }

    /// The var-table index of a column (None if fixed-length).
    #[inline(always)]
    pub fn var_index(&self, col_idx: usize) -> Option<usize> {
        self.var_index[col_idx]
    }

    /// Build a `RowLayout` from a schema. This is cheap — do it once per scan,
    /// not once per row.
    pub fn new(schema: &Schema) -> Self {
        let n_cols = schema.columns.len();
        let bitmap_size = n_cols.div_ceil(8);

        let mut fixed_offsets = vec![None; n_cols];
        let mut var_index = vec![None; n_cols];
        let mut fixed_pos: usize = 0;
        let mut var_count: usize = 0;

        for (i, col) in schema.columns.iter().enumerate() {
            if is_fixed_size(col.type_id) {
                fixed_offsets[i] = Some(fixed_pos);
                fixed_pos += fixed_size(col.type_id)
                    .expect("invariant: is_fixed_size(type_id) is true in this branch");
            } else {
                var_index[i] = Some(var_count);
                var_count += 1;
            }
        }

        RowLayout {
            fixed_offsets,
            fixed_region_size: fixed_pos,
            var_index,
            n_var: var_count,
            bitmap_size,
        }
    }
}

/// Decode a single column from the raw row bytes without allocating anything
/// for other columns.
///
/// Mission F: marked `#[inline]` so the compiler can specialise it inside
/// the per-row scan loops in `executor::project_filter_limit_fast`. With LTO
/// on, this allows the type-id match to fold away when the caller knows the
/// column type.
#[inline]
pub fn decode_column(schema: &Schema, layout: &RowLayout, data: &[u8], col_idx: usize) -> Value {
    // v2 rows carry an extra overflow bitmap between the null bitmap and the
    // fixed region, shifting every downstream offset. Fixed columns and inline
    // var columns decode correctly once shifted. A SPILLED var column's slot
    // holds a 24-byte stub, not the value — reassembly needs the heap, which a
    // pure decoder cannot reach, so this returns `Empty` for that case.
    // Callers wanting the real value of a spilled column use the heap-backed
    // reassembly (`Table::get` / `decode_row_v2` / `read_overflow_value`).
    let is_v2 = row_is_v2(data);
    let data = row_body(data);
    let col = &schema.columns[col_idx];

    // Check null bitmap
    let bitmap_start = 2; // skip 2-byte length prefix
    let is_null = (data[bitmap_start + col_idx / 8] >> (col_idx % 8)) & 1 == 1;
    if is_null {
        return Value::Empty;
    }

    let ovf_bitmap_size = if is_v2 {
        layout.overflow_bitmap_size()
    } else {
        0
    };
    let ovf_start = 2 + layout.bitmap_size;
    let fixed_start = ovf_start + ovf_bitmap_size;

    if let Some(offset) = layout.fixed_offsets[col_idx] {
        let pos = fixed_start + offset;
        match col.type_id {
            TypeId::Int => Value::Int(i64::from_le_bytes(
                data[pos..pos + 8]
                    .try_into()
                    .expect("invariant: 8-byte slice"),
            )),
            TypeId::Float => Value::Float(f64::from_le_bytes(
                data[pos..pos + 8]
                    .try_into()
                    .expect("invariant: 8-byte slice"),
            )),
            TypeId::Bool => Value::Bool(data[pos] != 0),
            TypeId::DateTime => Value::DateTime(i64::from_le_bytes(
                data[pos..pos + 8]
                    .try_into()
                    .expect("invariant: 8-byte slice"),
            )),
            TypeId::Uuid => {
                let mut v = [0u8; 16];
                v.copy_from_slice(&data[pos..pos + 16]);
                Value::Uuid(v)
            }
            _ => unreachable!(),
        }
    } else {
        let vi = layout.var_index[col_idx]
            .expect("invariant: column is variable-length (not in fixed_offsets)");
        // A spilled var column in a v2 row holds only a stub here — the pure
        // decoder cannot reassemble the value, so report it as absent.
        if is_v2 {
            let ovf_bitmap = &data[ovf_start..ovf_start + ovf_bitmap_size];
            if (ovf_bitmap[vi >> 3] >> (vi & 7)) & 1 == 1 {
                return Value::Empty;
            }
        }
        let offset_table_start = fixed_start + layout.fixed_region_size;
        let off_pos = offset_table_start + vi * 2;
        let next_off_pos = offset_table_start + (vi + 1) * 2;
        let var_offset = u16::from_le_bytes(
            data[off_pos..off_pos + 2]
                .try_into()
                .expect("invariant: 2-byte slice"),
        ) as usize;
        let var_next = u16::from_le_bytes(
            data[next_off_pos..next_off_pos + 2]
                .try_into()
                .expect("invariant: 2-byte slice"),
        ) as usize;

        let var_data_start = offset_table_start + (layout.n_var + 1) * 2;
        let start = var_data_start + var_offset;
        let end = var_data_start + var_next;
        let bytes = &data[start..end];

        match col.type_id {
            // Safety fix: use lossy UTF-8 decoding to prevent undefined
            // behavior from corrupted on-disk data. The ~5-15ns cost per
            // string is negligible compared to the safety win. Under normal
            // operation the bytes are always valid UTF-8 (they originate
            // from `String::as_bytes()` in `encode_row_into_with_layout`),
            // so `from_utf8_lossy` returns a `Borrowed` variant and avoids
            // any allocation.
            TypeId::Str => Value::Str(String::from_utf8_lossy(bytes).into_owned()),
            TypeId::Bytes => Value::Bytes(bytes.to_vec()),
            _ => unreachable!(),
        }
    }
}

/// Patch a single variable-length column in-place inside an already-encoded
/// row's raw bytes, shrinking the row if the new value is smaller than the
/// old one. Returns the new total row length on success, or `None` if the
/// new value would grow the row (caller must fall back to the full re-encode
/// path).
///
/// Mission C Phase 10: `update_by_filter` on the Mission A bench changes
/// `status` from one of `"active"/"inactive"/"pending"` (6-8 bytes) to
/// `"senior"` (6 bytes) for ~50K matching rows per iteration. Every single
/// row shrinks or matches — the old slow path still paid for a full
/// `decode_row` (3 String allocations per row) and `encode_row_into` (fresh
/// bitmap + fixed region + offset table walk) on every call. This helper
/// does the whole patch with 0 allocations by:
///   1. reading the old var offset pair from the offset table,
///   2. writing the new bytes directly over the old ones,
///   3. shifting any trailing var data back by `delta`,
///   4. decrementing every offset after the patched column by `delta`,
///   5. clearing the null bit (or setting it, if the new value is `None`),
///   6. rewriting the 2-byte length prefix.
///
/// Assumes `col_idx` is a variable-length column. The caller is expected to
/// check this (via `layout.var_index[col_idx]`) before calling; a panic in
/// the `unwrap` path is a caller bug.
#[inline]
pub fn patch_var_column_in_place(
    bytes: &mut [u8],
    layout: &RowLayout,
    col_idx: usize,
    new_value: Option<&[u8]>,
) -> Option<u16> {
    let base = row_body_offset(bytes).ok()?;
    let var_idx = layout.var_index[col_idx].expect("not a var column");
    let n_var = layout.n_var;

    let offset_table_start = base + 2 + layout.bitmap_size + layout.fixed_region_size;
    let var_data_start = offset_table_start + (n_var + 1) * 2;

    // Read old offsets for this var column from the offset table.
    let off_pos = offset_table_start + var_idx * 2;
    let next_off_pos = offset_table_start + (var_idx + 1) * 2;
    let old_var_offset = u16::from_le_bytes(
        bytes[off_pos..off_pos + 2]
            .try_into()
            .expect("invariant: 2-byte slice"),
    ) as usize;
    let old_var_next = u16::from_le_bytes(
        bytes[next_off_pos..next_off_pos + 2]
            .try_into()
            .expect("invariant: 2-byte slice"),
    ) as usize;
    let old_var_len = old_var_next - old_var_offset;

    let new_var_len = new_value.map(|v| v.len()).unwrap_or(0);
    if new_var_len > old_var_len {
        return None; // grow path — let the caller fall back to re-encode
    }
    let delta = old_var_len - new_var_len;

    // Absolute byte positions inside the row.
    let old_var_abs_start = var_data_start + old_var_offset;
    let old_var_abs_end = var_data_start + old_var_next;
    let old_row_len = bytes.len();

    // Write new bytes (if any) over the old payload.
    if let Some(v) = new_value {
        bytes[old_var_abs_start..old_var_abs_start + new_var_len].copy_from_slice(v);
    }

    // Shift trailing var data back by `delta` (no-op when same-size).
    if delta > 0 {
        bytes.copy_within(
            old_var_abs_end..old_row_len,
            old_var_abs_start + new_var_len,
        );

        // Decrement every offset AFTER this var column. The entry at
        // var_idx stays the same (it's the start of our patched column);
        // entries var_idx+1..=n_var slide back by `delta`.
        for vi in (var_idx + 1)..=n_var {
            let pos = offset_table_start + vi * 2;
            let old_off = u16::from_le_bytes(
                bytes[pos..pos + 2]
                    .try_into()
                    .expect("invariant: 2-byte slice"),
            );
            let new_off = old_off - delta as u16;
            bytes[pos..pos + 2].copy_from_slice(&new_off.to_le_bytes());
        }
    }

    // Null bitmap: clear or set the bit depending on new value.
    let bitmap_byte = base + 2 + col_idx / 8;
    let bit_mask = 1u8 << (col_idx % 8);
    if new_value.is_none() {
        bytes[bitmap_byte] |= bit_mask;
    } else {
        bytes[bitmap_byte] &= !bit_mask;
    }

    // Update the 2-byte length prefix.
    let new_row_len = old_row_len - delta;
    let new_body_len = new_row_len - base;
    bytes[base..base + 2].copy_from_slice(&(new_body_len as u16).to_le_bytes());

    Some(new_row_len as u16)
}

/// Decode a row from its compact binary format back into Values.
///
/// Mission F: `#[inline]` (not `always` — function is large) so LTO can fold
/// it into Filter+SeqScan when the inliner decides it's worth it.
#[inline]
pub fn decode_row(schema: &Schema, data: &[u8]) -> Row {
    let data = row_body(data);
    let n_cols = schema.columns.len();
    let bitmap_size = n_cols.div_ceil(8);

    let mut pos = 2; // skip length prefix

    // Read null bitmap
    let null_bitmap = &data[pos..pos + bitmap_size];
    pos += bitmap_size;

    // We'll build the result in two passes: fixed first, then merge in variable
    let mut values = vec![Value::Empty; n_cols];

    // Read fixed-size columns
    for (i, col) in schema.columns.iter().enumerate() {
        if !is_fixed_size(col.type_id) {
            continue;
        }
        let is_null = (null_bitmap[i / 8] >> (i % 8)) & 1 == 1;
        let sz = fixed_size(col.type_id)
            .expect("invariant: is_fixed_size(type_id) is true (non-fixed columns skipped above)");

        if is_null {
            pos += sz; // skip placeholder
                       // values[i] is already Empty
        } else {
            values[i] = match col.type_id {
                TypeId::Int => {
                    let v = i64::from_le_bytes(
                        data[pos..pos + 8]
                            .try_into()
                            .expect("invariant: 8-byte slice"),
                    );
                    Value::Int(v)
                }
                TypeId::Float => {
                    let v = f64::from_le_bytes(
                        data[pos..pos + 8]
                            .try_into()
                            .expect("invariant: 8-byte slice"),
                    );
                    Value::Float(v)
                }
                TypeId::Bool => Value::Bool(data[pos] != 0),
                TypeId::DateTime => {
                    let v = i64::from_le_bytes(
                        data[pos..pos + 8]
                            .try_into()
                            .expect("invariant: 8-byte slice"),
                    );
                    Value::DateTime(v)
                }
                TypeId::Uuid => {
                    let mut v = [0u8; 16];
                    v.copy_from_slice(&data[pos..pos + 16]);
                    Value::Uuid(v)
                }
                _ => unreachable!(),
            };
            pos += sz;
        }
    }

    // Read variable-length columns
    let var_col_indices: Vec<usize> = schema
        .columns
        .iter()
        .enumerate()
        .filter(|(_, c)| !is_fixed_size(c.type_id))
        .map(|(i, _)| i)
        .collect();

    let n_var = var_col_indices.len();
    let n_offsets = n_var + 1;

    let mut var_offsets = Vec::with_capacity(n_offsets);
    for _ in 0..n_offsets {
        let off = u16::from_le_bytes(
            data[pos..pos + 2]
                .try_into()
                .expect("invariant: 2-byte slice"),
        );
        var_offsets.push(off as usize);
        pos += 2;
    }

    let var_data_start = pos;

    for (vi, &col_idx) in var_col_indices.iter().enumerate() {
        let is_null = (null_bitmap[col_idx / 8] >> (col_idx % 8)) & 1 == 1;
        if is_null {
            // values[col_idx] is already Empty
            continue;
        }
        let start = var_data_start + var_offsets[vi];
        let end = var_data_start + var_offsets[vi + 1];
        let bytes = &data[start..end];
        values[col_idx] = match schema.columns[col_idx].type_id {
            // Safety fix: use lossy UTF-8 decoding (see `decode_column`
            // for the full rationale). Normal data is always valid UTF-8;
            // corrupted bytes get replacement characters instead of UB.
            TypeId::Str => Value::Str(String::from_utf8_lossy(bytes).into_owned()),
            TypeId::Bytes => Value::Bytes(bytes.to_vec()),
            _ => unreachable!(),
        };
    }

    values
}

/// Encode a row in format v2: v1 layout plus an overflow bitmap, with the
/// spilled var columns holding their 24-byte [`OverflowStub`] in place of the
/// inline value.
///
/// `spilled` has one entry per variable-length column, in declaration order
/// (var-table index order). `Some(stub)` means that column is stored
/// out-of-line: the stub bytes are written into its var-data slot and its
/// overflow bit is set. `None` means the column is stored inline exactly as in
/// v1. Fixed columns and `Empty` (null) var columns are never spilled.
///
/// v2 rows are still physically inline-capped at `MAX_ROW_DATA_SIZE`: spilling
/// shrinks the row (a large value becomes 24 bytes), so the u16 offset table
/// and length prefix keep permanent headroom (door D1).
pub fn encode_row_v2_into(
    schema: &Schema,
    layout: &RowLayout,
    values: &[Value],
    spilled: &[Option<OverflowStub>],
    out: &mut Vec<u8>,
) {
    debug_assert_eq!(values.len(), schema.columns.len());
    debug_assert_eq!(spilled.len(), layout.n_var);

    let n_cols = schema.columns.len();
    let null_bitmap_size = layout.bitmap_size;
    let ovf_bitmap_size = layout.overflow_bitmap_size();
    let fixed_region_size = layout.fixed_region_size;
    let n_var = layout.n_var;
    let n_offsets = n_var + 1;

    // Pre-pass: null bitmap, overflow bitmap, var-data size.
    let mut null_bitmap = vec![0u8; null_bitmap_size];
    let mut ovf_bitmap = vec![0u8; ovf_bitmap_size];
    let mut var_data_size: usize = 0;
    for (i, val) in values.iter().enumerate() {
        match layout.var_index[i] {
            None => {
                if matches!(val, Value::Empty) {
                    null_bitmap[i >> 3] |= 1 << (i & 7);
                }
            }
            Some(vi) => {
                if let Some(_stub) = &spilled[vi] {
                    // Spilled: 24-byte stub occupies the slot, overflow bit set.
                    ovf_bitmap[vi >> 3] |= 1 << (vi & 7);
                    var_data_size += OVERFLOW_STUB_SIZE;
                } else {
                    match val {
                        Value::Empty => null_bitmap[i >> 3] |= 1 << (i & 7),
                        Value::Str(s) => var_data_size += s.len(),
                        Value::Bytes(b) => var_data_size += b.len(),
                        _ => {}
                    }
                }
            }
        }
    }

    let body_size =
        null_bitmap_size + ovf_bitmap_size + fixed_region_size + n_offsets * 2 + var_data_size;
    let total_size = 2 + body_size;

    out.clear();
    out.resize(total_size, 0);
    out[0..2].copy_from_slice(&(total_size as u16).to_le_bytes());

    let null_start = 2;
    out[null_start..null_start + null_bitmap_size].copy_from_slice(&null_bitmap);
    let ovf_start = null_start + null_bitmap_size;
    out[ovf_start..ovf_start + ovf_bitmap_size].copy_from_slice(&ovf_bitmap);

    let fixed_start = ovf_start + ovf_bitmap_size;
    let offsets_start = fixed_start + fixed_region_size;
    let var_data_start = offsets_start + n_offsets * 2;

    let mut var_cursor: u16 = 0;
    let mut off_slot: usize = 0;
    for (i, val) in values.iter().enumerate().take(n_cols) {
        if let Some(off) = layout.fixed_offsets[i] {
            let pos = fixed_start + off;
            match val {
                Value::Empty => {}
                Value::Int(v) => out[pos..pos + 8].copy_from_slice(&v.to_le_bytes()),
                Value::Float(v) => out[pos..pos + 8].copy_from_slice(&v.to_le_bytes()),
                Value::Bool(v) => out[pos] = u8::from(*v),
                Value::DateTime(v) => out[pos..pos + 8].copy_from_slice(&v.to_le_bytes()),
                Value::Uuid(v) => out[pos..pos + 16].copy_from_slice(v),
                _ => unreachable!("fixed column with non-fixed value"),
            }
        } else {
            let vi = layout.var_index[i].expect("var column");
            let off_pos = offsets_start + off_slot * 2;
            out[off_pos..off_pos + 2].copy_from_slice(&var_cursor.to_le_bytes());
            off_slot += 1;

            if let Some(stub) = &spilled[vi] {
                let abs = var_data_start + var_cursor as usize;
                out[abs..abs + OVERFLOW_STUB_SIZE].copy_from_slice(&stub.to_bytes());
                var_cursor += OVERFLOW_STUB_SIZE as u16;
            } else {
                match val {
                    Value::Empty => {}
                    Value::Str(s) => {
                        let len = s.len();
                        let abs = var_data_start + var_cursor as usize;
                        out[abs..abs + len].copy_from_slice(s.as_bytes());
                        var_cursor += len as u16;
                    }
                    Value::Bytes(b) => {
                        let len = b.len();
                        let abs = var_data_start + var_cursor as usize;
                        out[abs..abs + len].copy_from_slice(b);
                        var_cursor += len as u16;
                    }
                    _ => unreachable!("variable column with non-variable value"),
                }
            }
        }
    }
    let end_pos = offsets_start + off_slot * 2;
    out[end_pos..end_pos + 2].copy_from_slice(&var_cursor.to_le_bytes());

    // Prefix with magic + version 2.
    let body_len = out.len();
    out.resize(body_len + ROW_PREFIX_SIZE, 0);
    out.copy_within(0..body_len, ROW_PREFIX_SIZE);
    out[0..4].copy_from_slice(ROW_MAGIC);
    out[4..6].copy_from_slice(&ROW_FORMAT_VERSION_V2.to_le_bytes());
}

/// Read the raw [`OverflowStub`] for a spilled column from a v2 row, or `None`
/// if the row is not v2, the column is fixed / not spilled, or the stub is
/// malformed. This is the raw accessor the executor's overflow-aware decode
/// and index/sweep maintenance use without reassembling the value.
pub fn raw_stub(
    schema: &Schema,
    layout: &RowLayout,
    data: &[u8],
    col_idx: usize,
) -> Option<OverflowStub> {
    if !row_is_v2(data) || col_idx >= schema.columns.len() {
        return None;
    }
    let vi = layout.var_index[col_idx]?;
    let body = &data[ROW_PREFIX_SIZE..];
    let ovf_start = 2 + layout.bitmap_size;
    let ovf_bitmap = &body[ovf_start..ovf_start + layout.overflow_bitmap_size()];
    if (ovf_bitmap[vi >> 3] >> (vi & 7)) & 1 == 0 {
        return None; // this var column is stored inline
    }
    let fixed_start = ovf_start + layout.overflow_bitmap_size();
    let offsets_start = fixed_start + layout.fixed_region_size;
    let off_pos = offsets_start + vi * 2;
    let var_offset =
        u16::from_le_bytes(body[off_pos..off_pos + 2].try_into().expect("2-byte")) as usize;
    let var_data_start = offsets_start + (layout.n_var + 1) * 2;
    let start = var_data_start + var_offset;
    OverflowStub::from_bytes(&body[start..start + OVERFLOW_STUB_SIZE])
}

/// Visit every out-of-line column in a v2 row as `(col_idx, stub)`. Used by
/// sweep's mark phase and delete-time chain freeing. A no-op for v0/v1 rows.
pub fn for_each_stub<F: FnMut(usize, OverflowStub)>(
    schema: &Schema,
    layout: &RowLayout,
    data: &[u8],
    mut f: F,
) {
    if !row_is_v2(data) {
        return;
    }
    for (col_idx, _col) in schema.columns.iter().enumerate() {
        if let Some(stub) = raw_stub(schema, layout, data, col_idx) {
            f(col_idx, stub);
        }
    }
}

/// Decode a v2 row into logical `Value`s, reassembling each spilled column by
/// calling `fetch(stub)` to read its bytes from the overflow chain. For a
/// v0/v1 row this just delegates to [`decode_row`]. The `fetch` closure is
/// responsible for CRC verification against the stub.
pub fn decode_row_v2<F>(
    schema: &Schema,
    layout: &RowLayout,
    data: &[u8],
    mut fetch: F,
) -> io::Result<Row>
where
    F: FnMut(&OverflowStub) -> io::Result<Vec<u8>>,
{
    if !row_is_v2(data) {
        return Ok(decode_row(schema, data));
    }
    let body = &data[ROW_PREFIX_SIZE..];
    let n_cols = schema.columns.len();
    let null_bitmap_size = layout.bitmap_size;
    let ovf_bitmap_size = layout.overflow_bitmap_size();

    let null_start = 2;
    let null_bitmap = &body[null_start..null_start + null_bitmap_size];
    let ovf_start = null_start + null_bitmap_size;
    let ovf_bitmap = &body[ovf_start..ovf_start + ovf_bitmap_size];
    let fixed_start = ovf_start + ovf_bitmap_size;
    let offsets_start = fixed_start + layout.fixed_region_size;
    let var_data_start = offsets_start + (layout.n_var + 1) * 2;

    let mut values = vec![Value::Empty; n_cols];
    for (i, col) in schema.columns.iter().enumerate() {
        if let Some(off) = layout.fixed_offsets[i] {
            let is_null = (null_bitmap[i >> 3] >> (i & 7)) & 1 == 1;
            if is_null {
                continue;
            }
            let pos = fixed_start + off;
            values[i] = match col.type_id {
                TypeId::Int => Value::Int(i64::from_le_bytes(
                    body[pos..pos + 8].try_into().expect("8-byte"),
                )),
                TypeId::Float => Value::Float(f64::from_le_bytes(
                    body[pos..pos + 8].try_into().expect("8-byte"),
                )),
                TypeId::Bool => Value::Bool(body[pos] != 0),
                TypeId::DateTime => Value::DateTime(i64::from_le_bytes(
                    body[pos..pos + 8].try_into().expect("8-byte"),
                )),
                TypeId::Uuid => {
                    let mut v = [0u8; 16];
                    v.copy_from_slice(&body[pos..pos + 16]);
                    Value::Uuid(v)
                }
                _ => unreachable!(),
            };
        } else {
            let vi = layout.var_index[i].expect("var column");
            let is_null = (null_bitmap[i >> 3] >> (i & 7)) & 1 == 1;
            if is_null {
                continue;
            }
            let off_pos = offsets_start + vi * 2;
            let next_off_pos = offsets_start + (vi + 1) * 2;
            let var_offset =
                u16::from_le_bytes(body[off_pos..off_pos + 2].try_into().expect("2-byte")) as usize;
            let var_next = u16::from_le_bytes(
                body[next_off_pos..next_off_pos + 2]
                    .try_into()
                    .expect("2-byte"),
            ) as usize;
            let start = var_data_start + var_offset;
            let end = var_data_start + var_next;

            let is_spilled = (ovf_bitmap[vi >> 3] >> (vi & 7)) & 1 == 1;
            let bytes: Vec<u8> = if is_spilled {
                let stub = OverflowStub::from_bytes(&body[start..end]).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "malformed overflow stub")
                })?;
                fetch(&stub)?
            } else {
                body[start..end].to_vec()
            };
            values[i] = match col.type_id {
                TypeId::Str => Value::Str(String::from_utf8_lossy(&bytes).into_owned()),
                TypeId::Bytes => Value::Bytes(bytes),
                _ => unreachable!(),
            };
        }
    }
    Ok(values)
}

/// Reassemble a v2 row into an equivalent v1 (fully inline) row by fetching
/// its spilled columns through `fetch`. The result decodes identically under
/// the v1 fast paths, which is how the executor's per-row v2 routing rejoins
/// the generic decode path. A v0/v1 row is returned unchanged.
pub fn rehydrate_v2_to_v1<F>(
    schema: &Schema,
    layout: &RowLayout,
    data: &[u8],
    fetch: F,
) -> io::Result<Vec<u8>>
where
    F: FnMut(&OverflowStub) -> io::Result<Vec<u8>>,
{
    if !row_is_v2(data) {
        return Ok(data.to_vec());
    }
    let values = decode_row_v2(schema, layout, data, fetch)?;
    let mut out = Vec::new();
    encode_row_into_with_layout(schema, layout, &values, &mut out);
    Ok(out)
}

/// Decide which columns must move out of line for a row to fit in one page
/// (spill policy, design 3.4). `v1_len` is the length of the row's plain v1
/// encoding (prefix included). Returns the COLUMN indices to spill, chosen
/// deterministically:
///
/// 1. Fixed types never spill (not candidates).
/// 2. If the v1 row already fits `MAX_ROW_DATA_SIZE`, returns empty (encode v1).
/// 3. Otherwise evict var values largest-first, preferring values >=
///    `OVERFLOW_MIN`; only descend into sub-`OVERFLOW_MIN` values if evicting
///    every large value still does not make the row fit.
///
/// If even spilling every eligible value cannot fit, the full list is returned
/// and the heap insert boundary surfaces `RowTooLarge` (unreachable with sane
/// schemas). The caller enforces `MAX_VALUE_SIZE` separately, per value.
/// Compute the length of the plain v1 encoding of `values` WITHOUT encoding
/// it. Saturating, so a value larger than the u16 inline cap does not panic
/// the way [`encode_row_into`] would in debug builds — the spill planner needs
/// the size precisely for over-cap rows.
pub fn v1_encoded_len(layout: &RowLayout, values: &[Value]) -> usize {
    let n_offsets = layout.n_var + 1;
    let mut var_data: usize = 0;
    for val in values {
        match val {
            Value::Str(s) => var_data = var_data.saturating_add(s.len()),
            Value::Bytes(b) => var_data = var_data.saturating_add(b.len()),
            _ => {}
        }
    }
    ROW_PREFIX_SIZE + 2 + layout.bitmap_size + layout.fixed_region_size + n_offsets * 2 + var_data
}

pub fn plan_spill(layout: &RowLayout, values: &[Value], v1_len: usize) -> Vec<usize> {
    if v1_len <= crate::page::MAX_ROW_DATA_SIZE {
        return Vec::new();
    }
    // Candidate columns: variable-length, non-empty. (col_idx, len).
    let mut cands: Vec<(usize, usize)> = values
        .iter()
        .enumerate()
        .filter_map(|(i, v)| {
            layout.var_index(i)?;
            let len = match v {
                Value::Str(s) => s.len(),
                Value::Bytes(b) => b.len(),
                _ => return None,
            };
            if len == 0 {
                None
            } else {
                Some((i, len))
            }
        })
        .collect();
    // Prefer big values (>= OVERFLOW_MIN) first, then by descending length.
    cands.sort_by(|a, b| {
        let a_big = a.1 >= OVERFLOW_MIN;
        let b_big = b.1 >= OVERFLOW_MIN;
        b_big.cmp(&a_big).then_with(|| b.1.cmp(&a.1))
    });

    // Spilling a value shrinks the row by (len - stub) and the row gains a
    // fixed overflow bitmap the moment it becomes v2.
    let mut est = v1_len + layout.overflow_bitmap_size();
    let mut chosen = Vec::new();
    for (col_idx, len) in cands {
        if est <= crate::page::MAX_ROW_DATA_SIZE {
            break;
        }
        est -= len.saturating_sub(OVERFLOW_STUB_SIZE);
        chosen.push(col_idx);
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_schema() -> Schema {
        Schema {
            table_name: "users".into(),
            columns: vec![
                ColumnDef {
                    name: "name".into(),
                    type_id: TypeId::Str,
                    required: true,
                    position: 0,
                },
                ColumnDef {
                    name: "email".into(),
                    type_id: TypeId::Str,
                    required: true,
                    position: 1,
                },
                ColumnDef {
                    name: "age".into(),
                    type_id: TypeId::Int,
                    required: false,
                    position: 2,
                },
                ColumnDef {
                    name: "active".into(),
                    type_id: TypeId::Bool,
                    required: true,
                    position: 3,
                },
            ],
        }
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let schema = user_schema();
        let row = vec![
            Value::Str("Alice".into()),
            Value::Str("alice@example.com".into()),
            Value::Int(30),
            Value::Bool(true),
        ];
        let encoded = encode_row(&schema, &row);
        let decoded = decode_row(&schema, &encoded);
        assert_eq!(decoded.len(), 4);
        assert_eq!(decoded[0], Value::Str("Alice".into()));
        assert_eq!(decoded[1], Value::Str("alice@example.com".into()));
        assert_eq!(decoded[2], Value::Int(30));
        assert_eq!(decoded[3], Value::Bool(true));
    }

    #[test]
    fn test_encode_with_empty_optional() {
        let schema = user_schema();
        let row = vec![
            Value::Str("Bob".into()),
            Value::Str("bob@example.com".into()),
            Value::Empty,
            Value::Bool(false),
        ];
        let encoded = encode_row(&schema, &row);
        let decoded = decode_row(&schema, &encoded);
        assert_eq!(decoded[2], Value::Empty);
        assert_eq!(decoded[3], Value::Bool(false));
        assert_eq!(decoded[0], Value::Str("Bob".into()));
    }

    #[test]
    fn test_all_empty() {
        let schema = Schema {
            table_name: "t".into(),
            columns: vec![
                ColumnDef {
                    name: "a".into(),
                    type_id: TypeId::Int,
                    required: false,
                    position: 0,
                },
                ColumnDef {
                    name: "b".into(),
                    type_id: TypeId::Str,
                    required: false,
                    position: 1,
                },
            ],
        };
        let row = vec![Value::Empty, Value::Empty];
        let encoded = encode_row(&schema, &row);
        let decoded = decode_row(&schema, &encoded);
        assert_eq!(decoded[0], Value::Empty);
        assert_eq!(decoded[1], Value::Empty);
    }

    #[test]
    fn test_compact_overhead() {
        let schema = user_schema();
        let row = vec![
            Value::Str("Alice".into()),
            Value::Str("alice@example.com".into()),
            Value::Int(30),
            Value::Bool(true),
        ];
        let encoded = encode_row(&schema, &row);
        let pure_data = 5 + 17 + 8 + 1; // "Alice" + "alice@example.com" + i64 + bool = 31
        let overhead = encoded.len() - pure_data;
        // 6B row format prefix + 2B length + 1B bitmap + 6B var offset table
        // (3 entries * 2B) = 15B overhead. The explicit prefix is the
        // v0.5.0 format guard that prevents silent row misdecode.
        assert!(overhead <= 16, "overhead was {overhead}, expected <= 16");
    }

    #[test]
    fn test_multiple_roundtrips() {
        let schema = Schema {
            table_name: "t".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    type_id: TypeId::Int,
                    required: true,
                    position: 0,
                },
                ColumnDef {
                    name: "name".into(),
                    type_id: TypeId::Str,
                    required: true,
                    position: 1,
                },
                ColumnDef {
                    name: "score".into(),
                    type_id: TypeId::Float,
                    required: false,
                    position: 2,
                },
                ColumnDef {
                    name: "uuid".into(),
                    type_id: TypeId::Uuid,
                    required: false,
                    position: 3,
                },
            ],
        };
        for i in 0..100 {
            let row = vec![
                Value::Int(i),
                Value::Str(format!("name_{i}")),
                if i % 3 == 0 {
                    Value::Empty
                } else {
                    Value::Float(i as f64 * 1.5)
                },
                if i % 5 == 0 {
                    Value::Uuid([i as u8; 16])
                } else {
                    Value::Empty
                },
            ];
            let encoded = encode_row(&schema, &row);
            let decoded = decode_row(&schema, &encoded);
            assert_eq!(decoded, row, "roundtrip failed for i={i}");
        }
    }

    #[test]
    fn test_patch_var_column_same_size() {
        let schema = user_schema();
        let row = vec![
            Value::Str("Alice".into()),
            Value::Str("alice@example.com".into()),
            Value::Int(30),
            Value::Bool(true),
        ];
        let mut encoded = encode_row(&schema, &row);
        let layout = RowLayout::new(&schema);
        // name: "Alice" (5) → "Bobby" (5) — same size, trivial overwrite.
        let new_len = patch_var_column_in_place(&mut encoded, &layout, 0, Some(b"Bobby")).unwrap();
        encoded.truncate(new_len as usize);
        let decoded = decode_row(&schema, &encoded);
        assert_eq!(decoded[0], Value::Str("Bobby".into()));
        assert_eq!(decoded[1], Value::Str("alice@example.com".into()));
        assert_eq!(decoded[2], Value::Int(30));
        assert_eq!(decoded[3], Value::Bool(true));
    }

    #[test]
    fn test_patch_var_column_shrink_first() {
        let schema = user_schema();
        let row = vec![
            Value::Str("Alexandra".into()), // 9 bytes
            Value::Str("alice@example.com".into()),
            Value::Int(42),
            Value::Bool(false),
        ];
        let mut encoded = encode_row(&schema, &row);
        let layout = RowLayout::new(&schema);
        // Patch `name` from 9 bytes → 3 bytes; trailing var data must shift back.
        let new_len = patch_var_column_in_place(&mut encoded, &layout, 0, Some(b"Eve")).unwrap();
        encoded.truncate(new_len as usize);
        let decoded = decode_row(&schema, &encoded);
        assert_eq!(decoded[0], Value::Str("Eve".into()));
        assert_eq!(decoded[1], Value::Str("alice@example.com".into()));
        assert_eq!(decoded[2], Value::Int(42));
        assert_eq!(decoded[3], Value::Bool(false));
    }

    #[test]
    fn test_patch_var_column_shrink_middle() {
        // Mirrors the Mission A bench: middle var col changes, trailing var
        // col must stay intact and its offset must slide back by `delta`.
        let schema = Schema {
            table_name: "U".into(),
            columns: vec![
                ColumnDef {
                    name: "name".into(),
                    type_id: TypeId::Str,
                    required: true,
                    position: 0,
                },
                ColumnDef {
                    name: "status".into(),
                    type_id: TypeId::Str,
                    required: true,
                    position: 1,
                },
                ColumnDef {
                    name: "email".into(),
                    type_id: TypeId::Str,
                    required: true,
                    position: 2,
                },
                ColumnDef {
                    name: "age".into(),
                    type_id: TypeId::Int,
                    required: false,
                    position: 3,
                },
            ],
        };
        let row = vec![
            Value::Str("user_42".into()),
            Value::Str("inactive".into()), // 8 bytes
            Value::Str("user_42@example.com".into()),
            Value::Int(55),
        ];
        let mut encoded = encode_row(&schema, &row);
        let layout = RowLayout::new(&schema);
        let new_len = patch_var_column_in_place(&mut encoded, &layout, 1, Some(b"senior")).unwrap();
        encoded.truncate(new_len as usize);
        let decoded = decode_row(&schema, &encoded);
        assert_eq!(decoded[0], Value::Str("user_42".into()));
        assert_eq!(decoded[1], Value::Str("senior".into()));
        assert_eq!(decoded[2], Value::Str("user_42@example.com".into()));
        assert_eq!(decoded[3], Value::Int(55));
    }

    #[test]
    fn test_patch_var_column_grow_rejects() {
        let schema = user_schema();
        let row = vec![
            Value::Str("Al".into()), // 2 bytes
            Value::Str("alice@example.com".into()),
            Value::Int(30),
            Value::Bool(true),
        ];
        let mut encoded = encode_row(&schema, &row);
        let layout = RowLayout::new(&schema);
        assert!(patch_var_column_in_place(&mut encoded, &layout, 0, Some(b"Alexandra")).is_none());
    }

    #[test]
    fn test_patch_var_column_to_null() {
        let schema = user_schema();
        let row = vec![
            Value::Str("Alice".into()),
            Value::Str("alice@example.com".into()),
            Value::Int(30),
            Value::Bool(true),
        ];
        let mut encoded = encode_row(&schema, &row);
        let layout = RowLayout::new(&schema);
        // Set `name` to null.
        let new_len = patch_var_column_in_place(&mut encoded, &layout, 0, None).unwrap();
        encoded.truncate(new_len as usize);
        let decoded = decode_row(&schema, &encoded);
        assert_eq!(decoded[0], Value::Empty);
        assert_eq!(decoded[1], Value::Str("alice@example.com".into()));
    }

    #[test]
    fn test_patch_var_column_clears_null_bit() {
        let schema = Schema {
            table_name: "U".into(),
            columns: vec![
                ColumnDef {
                    name: "label".into(),
                    type_id: TypeId::Str,
                    required: false,
                    position: 0,
                },
                ColumnDef {
                    name: "fill".into(),
                    type_id: TypeId::Str,
                    required: false,
                    position: 1,
                },
            ],
        };
        // Start with label = null; we need enough room in the (currently
        // 0-length) label slot to fit new content — which we don't have.
        // So this should reject.
        let row = vec![Value::Empty, Value::Str("data".into())];
        let mut encoded = encode_row(&schema, &row);
        let layout = RowLayout::new(&schema);
        // Attempting to write "x" into a currently 0-length var col should
        // be a grow → rejected.
        assert!(patch_var_column_in_place(&mut encoded, &layout, 0, Some(b"x")).is_none());
    }

    #[test]
    fn test_empty_string_vs_empty_set() {
        let schema = Schema {
            table_name: "t".into(),
            columns: vec![ColumnDef {
                name: "s".into(),
                type_id: TypeId::Str,
                required: false,
                position: 0,
            }],
        };
        // Empty string is a real value, not Empty
        let row_str = vec![Value::Str("".into())];
        let row_empty = vec![Value::Empty];

        let enc_str = encode_row(&schema, &row_str);
        let enc_empty = encode_row(&schema, &row_empty);

        let dec_str = decode_row(&schema, &enc_str);
        let dec_empty = decode_row(&schema, &enc_empty);

        assert_eq!(dec_str[0], Value::Str("".into()));
        assert_eq!(dec_empty[0], Value::Empty);
        assert_ne!(dec_str[0], dec_empty[0]); // "" is NOT the same as {}
    }

    #[test]
    fn test_try_encode_row_rejects_oversized_row() {
        let schema = Schema {
            table_name: "t".into(),
            columns: vec![ColumnDef {
                name: "big".into(),
                type_id: TypeId::Str,
                required: true,
                position: 0,
            }],
        };
        // A string larger than 64KB should be rejected.
        let big_string = "x".repeat(70_000);
        let row = vec![Value::Str(big_string)];
        let result = try_encode_row(&schema, &row);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let msg = err.to_string();
        assert!(
            msg.contains("64KB") || msg.contains("too large"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn test_try_encode_row_accepts_normal_row() {
        let schema = user_schema();
        let row = vec![
            Value::Str("Alice".into()),
            Value::Str("alice@example.com".into()),
            Value::Int(30),
            Value::Bool(true),
        ];
        let result = try_encode_row(&schema, &row);
        assert!(result.is_ok());
        let encoded = result.unwrap();
        let decoded = decode_row(&schema, &encoded);
        assert_eq!(decoded[0], Value::Str("Alice".into()));
    }

    #[test]
    fn test_safe_utf8_decode_handles_invalid_bytes() {
        // Manually construct a row with invalid UTF-8 in a Str column
        // to verify we don't crash/UB.
        let schema = Schema {
            table_name: "t".into(),
            columns: vec![ColumnDef {
                name: "s".into(),
                type_id: TypeId::Str,
                required: true,
                position: 0,
            }],
        };
        // Encode a valid row first, then corrupt the string bytes.
        let mut encoded = encode_row(&schema, &[Value::Str("hello".into())]);
        // The var data starts after: 2 (len) + 1 (bitmap) + 2*2 (offset table)
        // = 7 bytes. Write invalid UTF-8 sequence.
        let var_data_start = 2 + 1 + 4; // len_prefix + bitmap + offset_table(2 entries * 2 bytes)
        if var_data_start + 2 <= encoded.len() {
            encoded[var_data_start] = 0xFF;
            encoded[var_data_start + 1] = 0xFE;
        }
        // Should not panic — lossy decoding replaces invalid bytes.
        let decoded = decode_row(&schema, &encoded);
        // The value should be a Str (not crash), contents may have
        // replacement characters.
        matches!(decoded[0], Value::Str(_));
    }

    #[test]
    fn test_overflow_stub_roundtrip() {
        let stub = OverflowStub::new(1_234_567_890, 42, 0xDEAD_BEEF);
        let bytes = stub.to_bytes();
        assert_eq!(bytes.len(), OVERFLOW_STUB_SIZE);
        assert_eq!(bytes[18..24], [0u8; 6], "reserved bytes must be zeroed");
        assert_eq!(bytes[17], OVERFLOW_STUB_VERSION);
        let back = OverflowStub::from_bytes(&bytes).unwrap();
        assert_eq!(back, stub);
    }

    #[test]
    fn test_overflow_stub_rejects_unknown_version() {
        let mut bytes = OverflowStub::new(10, 1, 2).to_bytes();
        bytes[17] = 9; // unknown stub_version
        assert!(OverflowStub::from_bytes(&bytes).is_none());
    }

    #[test]
    fn test_row_format_gate_accepts_v2_rejects_v3() {
        // A well-formed v2 row passes the gate.
        let schema = user_schema();
        let layout = RowLayout::new(&schema);
        let values = vec![
            Value::Str("Alice".into()),
            Value::Str("alice@example.com".into()),
            Value::Int(30),
            Value::Bool(true),
        ];
        let spilled = vec![None; layout.n_var()];
        let mut out = Vec::new();
        encode_row_v2_into(&schema, &layout, &values, &spilled, &mut out);
        assert_eq!(row_format_version(&out).unwrap(), 2);
        assert!(row_is_v2(&out));

        // Version 3 must be refused (old-gate simulation for a future format).
        let mut v3 = out.clone();
        v3[4..6].copy_from_slice(&3u16.to_le_bytes());
        assert!(row_format_version(&v3).is_err());
    }

    #[test]
    fn test_v2_all_inline_decodes_same_as_v1() {
        // With no spilled columns, a v2 row's logical values must equal the
        // v1 encoding's values, and rehydrate must produce byte-identical v1.
        let schema = user_schema();
        let layout = RowLayout::new(&schema);
        let values = vec![
            Value::Str("Alice".into()),
            Value::Str("alice@example.com".into()),
            Value::Int(30),
            Value::Bool(true),
        ];
        let spilled = vec![None; layout.n_var()];
        let mut v2 = Vec::new();
        encode_row_v2_into(&schema, &layout, &values, &spilled, &mut v2);

        // decode_row_v2 with a fetch that must never be called (nothing spilled).
        let decoded = decode_row_v2(&schema, &layout, &v2, |_| {
            panic!("no column is spilled; fetch must not run")
        })
        .unwrap();
        assert_eq!(decoded, values);

        // rehydrate to v1 must be byte-identical to the plain v1 encoding.
        let rehydrated = rehydrate_v2_to_v1(&schema, &layout, &v2, |_| {
            panic!("no column is spilled; fetch must not run")
        })
        .unwrap();
        let v1 = encode_row(&schema, &values);
        assert_eq!(rehydrated, v1, "rehydrated v2 must equal the v1 encoding");
    }

    #[test]
    fn test_v2_spilled_column_reassembles_via_fetch() {
        let schema = user_schema();
        let layout = RowLayout::new(&schema);
        // The big value that would spill (email column, var index 1).
        let big = "z".repeat(5000);
        let values = vec![
            Value::Str("Alice".into()),
            Value::Str(big.clone()),
            Value::Int(30),
            Value::Bool(true),
        ];
        let crc = crc32fast::hash(big.as_bytes());
        let stub = OverflowStub::new(big.len() as u64, 77, crc);
        // var index for the email column (position 1) is 1 (name=0, email=1).
        let email_vi = layout.var_index(1).unwrap();
        let mut spilled = vec![None; layout.n_var()];
        spilled[email_vi] = Some(stub);

        let mut v2 = Vec::new();
        encode_row_v2_into(&schema, &layout, &values, &spilled, &mut v2);
        assert!(row_is_v2(&v2));
        // v2 row is small — the 5000-byte value became a 24-byte stub.
        assert!(v2.len() < 200, "v2 row should be small, got {}", v2.len());

        // raw_stub reads back the pointer without reassembling.
        let read = raw_stub(&schema, &layout, &v2, 1).unwrap();
        assert_eq!(read, stub);
        assert!(
            raw_stub(&schema, &layout, &v2, 0).is_none(),
            "name is inline"
        );

        // for_each_stub visits exactly the spilled column.
        let mut seen = Vec::new();
        for_each_stub(&schema, &layout, &v2, |ci, s| seen.push((ci, s)));
        assert_eq!(seen, vec![(1usize, stub)]);

        // decode_row_v2 reassembles the value via the fetch closure.
        let big_bytes = big.clone().into_bytes();
        let decoded = decode_row_v2(&schema, &layout, &v2, |s| {
            assert_eq!(*s, stub);
            Ok(big_bytes.clone())
        })
        .unwrap();
        assert_eq!(decoded[0], Value::Str("Alice".into()));
        assert_eq!(decoded[1], Value::Str(big.clone()));
        assert_eq!(decoded[2], Value::Int(30));

        // rehydrate produces a v1 row that decodes identically.
        let v1 = rehydrate_v2_to_v1(&schema, &layout, &v2, |_| Ok(big_bytes.clone())).unwrap();
        assert_eq!(row_format_version(&v1).unwrap(), 1);
        let decoded_v1 = decode_row(&schema, &v1);
        assert_eq!(decoded_v1, values);
    }

    #[test]
    fn test_v2_multi_spill_and_null_mix() {
        // Two var columns both spilled, plus a null var column, on a wider
        // schema, verifies bitmap bookkeeping stays coherent.
        let schema = Schema {
            table_name: "docs".into(),
            columns: vec![
                ColumnDef {
                    name: "a".into(),
                    type_id: TypeId::Str,
                    required: false,
                    position: 0,
                },
                ColumnDef {
                    name: "n".into(),
                    type_id: TypeId::Int,
                    required: false,
                    position: 1,
                },
                ColumnDef {
                    name: "b".into(),
                    type_id: TypeId::Bytes,
                    required: false,
                    position: 2,
                },
                ColumnDef {
                    name: "c".into(),
                    type_id: TypeId::Str,
                    required: false,
                    position: 3,
                },
            ],
        };
        let layout = RowLayout::new(&schema);
        let a = "a".repeat(4000);
        let b = vec![7u8; 3000];
        let values = vec![
            Value::Str(a.clone()),
            Value::Int(99),
            Value::Bytes(b.clone()),
            Value::Empty, // null var column
        ];
        let a_vi = layout.var_index(0).unwrap();
        let b_vi = layout.var_index(2).unwrap();
        let mut spilled = vec![None; layout.n_var()];
        spilled[a_vi] = Some(OverflowStub::new(
            a.len() as u64,
            10,
            crc32fast::hash(a.as_bytes()),
        ));
        spilled[b_vi] = Some(OverflowStub::new(b.len() as u64, 20, crc32fast::hash(&b)));

        let mut v2 = Vec::new();
        encode_row_v2_into(&schema, &layout, &values, &spilled, &mut v2);

        let decoded = decode_row_v2(&schema, &layout, &v2, |s| {
            if s.first_page == 10 {
                Ok(a.clone().into_bytes())
            } else {
                Ok(b.clone())
            }
        })
        .unwrap();
        assert_eq!(decoded[0], Value::Str(a));
        assert_eq!(decoded[1], Value::Int(99));
        assert_eq!(decoded[2], Value::Bytes(b));
        assert_eq!(decoded[3], Value::Empty);
    }
}
