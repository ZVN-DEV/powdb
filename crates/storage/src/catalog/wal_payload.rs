//! WAL payload codecs for row, overflow-chain, and overflow-free records:
//! the byte layouts `Catalog` logs and replays for data mutations.

use super::*;

pub(super) fn encode_wal_payload(table: &str, rid: RowId, row_bytes: &[u8]) -> Vec<u8> {
    let name = table.as_bytes();
    let mut out = Vec::with_capacity(4 + name.len() + 4 + 2 + 4 + row_bytes.len());
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&rid.page_id.to_le_bytes());
    out.extend_from_slice(&rid.slot_index.to_le_bytes());
    out.extend_from_slice(&(row_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(row_bytes);
    out
}

pub(super) fn decode_wal_payload(data: &[u8]) -> Option<(String, RowId, Vec<u8>)> {
    let mut pos = 0usize;
    if data.len() < 4 {
        return None;
    }
    let name_len = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
    pos += 4;
    if pos + name_len > data.len() {
        return None;
    }
    let name = std::str::from_utf8(&data[pos..pos + name_len])
        .ok()?
        .to_string();
    pos += name_len;
    if pos + 4 + 2 + 4 > data.len() {
        return None;
    }
    let page_id = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?);
    pos += 4;
    let slot_index = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?);
    pos += 2;
    let row_len = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
    pos += 4;
    if pos + row_len > data.len() {
        return None;
    }
    let row_bytes = data[pos..pos + row_len].to_vec();
    Some((
        name,
        RowId {
            page_id,
            slot_index,
        },
        row_bytes,
    ))
}

/// Write one out-of-line value's overflow chain to the heap (head-first,
/// singly linked) and log each chunk as a `WalRecordType::OverflowWrite`
/// record under `tx_id`, ordered BEFORE the row's Insert/Update record so the
/// stub the row carries always points at logged, replayable pages. Returns the
/// stub (u64 length, head page, whole-value CRC32). Enforces `MAX_VALUE_SIZE`.
pub(super) fn write_overflow_chain_logged(
    heap: &mut HeapFile,
    wal: &mut Wal,
    table: &str,
    tx_id: u64,
    value: &[u8],
) -> io::Result<OverflowStub> {
    if value.len() > MAX_VALUE_SIZE {
        return Err(StorageError::ValueTooLarge {
            size: value.len(),
            max: MAX_VALUE_SIZE,
        }
        .into());
    }
    let n = value.len().div_ceil(OVERFLOW_PAYLOAD_CAP).max(1);
    let mut pages = Vec::with_capacity(n);
    for _ in 0..n {
        pages.push(heap.allocate_overflow_page()?);
    }
    for i in 0..n {
        let start = i * OVERFLOW_PAYLOAD_CAP;
        let end = (start + OVERFLOW_PAYLOAD_CAP).min(value.len());
        let chunk = &value[start..end];
        let next = if i + 1 < n {
            pages[i + 1]
        } else {
            OVERFLOW_CHAIN_END
        };
        let payload = encode_overflow_write_payload(table, pages[i], next, chunk);
        wal.append(tx_id, WalRecordType::OverflowWrite, &payload)?;
        let lsn = wal.last_appended_lsn();
        heap.write_overflow_page(pages[i], next, chunk, lsn)?;
    }
    Ok(OverflowStub::new(
        value.len() as u64,
        pages[0],
        crc32fast::hash(value),
    ))
}

/// Spill-aware encode for the WAL path. If the row fits inline, returns its v1
/// bytes untouched. Otherwise writes each spilled value's chain (with WAL
/// logging under `tx_id`) and returns the v2 stub-row bytes to be inserted and
/// logged in the row's Insert/Update record.
pub(super) fn encode_row_with_spill_logged(
    tbl: &mut Table,
    wal: &mut Wal,
    tx_id: u64,
    values: &Row,
) -> io::Result<Vec<u8>> {
    // Size the v1 encoding WITHOUT encoding it (a >64KB value would panic the
    // debug-mode v1 encoder). Only actually encode v1 when the row fits inline.
    let v1_len = crate::row::v1_encoded_len(tbl.row_layout(), values);
    let is_indexed = tbl.indexed_col_mask();
    let chosen = plan_spill(tbl.row_layout(), values, v1_len, &is_indexed);
    if chosen.is_empty() {
        let mut v1 = Vec::new();
        encode_row_into(&tbl.schema, values, &mut v1);
        return Ok(v1);
    }
    let table_name = tbl.schema.table_name.clone();
    let n_var = tbl.row_layout().n_var();
    let mut spilled: Vec<Option<OverflowStub>> = vec![None; n_var];
    for col_idx in chosen {
        let var_idx = tbl
            .row_layout()
            .var_index(col_idx)
            .expect("plan_spill only returns var columns");
        let bytes: Vec<u8> = match &values[col_idx] {
            Value::Str(s) => s.as_bytes().to_vec(),
            Value::Bytes(b) => b.to_vec(),
            Value::Json(b) => b.to_vec(),
            _ => continue,
        };
        let stub = write_overflow_chain_logged(&mut tbl.heap, wal, &table_name, tx_id, &bytes)?;
        spilled[var_idx] = Some(stub);
    }
    let mut out = Vec::new();
    encode_row_v2_into(&tbl.schema, tbl.row_layout(), values, &spilled, &mut out);
    Ok(out)
}

/// `OverflowWrite` payload: `table_len u16 | table | page_id u32 |
/// next_page u32 | chunk_len u16 | chunk bytes`.
///
/// NOTE (deviation from design 3.5): the design lists the payload as
/// `page_id | next_page | chunk_len | chunk`, but overflow pages live in
/// per-table heap files with independent page-id spaces, so replay needs the
/// table identity to route the write. The table name is length-prefixed
/// exactly like [`encode_wal_payload`]. The chunk-level fields are unchanged.
pub(super) fn encode_overflow_write_payload(
    table: &str,
    page_id: u32,
    next_page: u32,
    chunk: &[u8],
) -> Vec<u8> {
    let name = table.as_bytes();
    let mut out = Vec::with_capacity(2 + name.len() + 4 + 4 + 2 + chunk.len());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&page_id.to_le_bytes());
    out.extend_from_slice(&next_page.to_le_bytes());
    out.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
    out.extend_from_slice(chunk);
    out
}

pub(super) fn decode_overflow_write_payload(data: &[u8]) -> Option<(String, u32, u32, Vec<u8>)> {
    let mut pos = 0usize;
    if data.len() < 2 {
        return None;
    }
    let name_len = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
    pos += 2;
    if pos + name_len + 4 + 4 + 2 > data.len() {
        return None;
    }
    let name = std::str::from_utf8(&data[pos..pos + name_len])
        .ok()?
        .to_string();
    pos += name_len;
    let page_id = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?);
    pos += 4;
    let next_page = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?);
    pos += 4;
    let chunk_len = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
    pos += 2;
    if pos + chunk_len > data.len() {
        return None;
    }
    Some((
        name,
        page_id,
        next_page,
        data[pos..pos + chunk_len].to_vec(),
    ))
}

/// `OverflowFree` payload: `table_len u16 | table | count u32 |
/// page_id u32 x count`. Table name added for the same routing reason as
/// [`encode_overflow_write_payload`].
pub(super) fn encode_overflow_free_payload(table: &str, pages: &[u32]) -> Vec<u8> {
    let name = table.as_bytes();
    let mut out = Vec::with_capacity(2 + name.len() + 4 + pages.len() * 4);
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&(pages.len() as u32).to_le_bytes());
    for p in pages {
        out.extend_from_slice(&p.to_le_bytes());
    }
    out
}

pub(super) fn decode_overflow_free_payload(data: &[u8]) -> Option<(String, Vec<u32>)> {
    let mut pos = 0usize;
    if data.len() < 2 {
        return None;
    }
    let name_len = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
    pos += 2;
    if pos + name_len + 4 > data.len() {
        return None;
    }
    let name = std::str::from_utf8(&data[pos..pos + name_len])
        .ok()?
        .to_string();
    pos += name_len;
    let count = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
    pos += 4;
    if pos + count * 4 > data.len() {
        return None;
    }
    let mut pages = Vec::with_capacity(count);
    for _ in 0..count {
        pages.push(u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?));
        pos += 4;
    }
    Some((name, pages))
}
