//! WAL payload codecs for DDL records (create/drop table, add/drop column)
//! and the value-blob, defaults, and auto-column sections they embed.

use super::*;

// ─── DDL WAL payload codecs ─────────────────────────────────────────────────

pub(super) fn encode_ddl_create_table(
    schema: &Schema,
    defaults: &[Option<Value>],
    auto_cols: &[bool],
) -> Vec<u8> {
    let name = schema.table_name.as_bytes();
    let mut out = Vec::new();
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&(schema.columns.len() as u16).to_le_bytes());
    for col in &schema.columns {
        let cn = col.name.as_bytes();
        out.extend_from_slice(&(cn.len() as u32).to_le_bytes());
        out.extend_from_slice(cn);
        out.push(col.type_id as u8);
        out.push(col.required as u8);
        out.extend_from_slice(&col.position.to_le_bytes());
    }
    // Trailing sections. Records written before each feature existed simply
    // lack the corresponding trailing bytes, so the decoder treats their
    // absence as "none" (length-detected, append-only).
    encode_defaults_section(&mut out, defaults);
    encode_auto_section(&mut out, auto_cols);
    out
}

pub(super) fn decode_ddl_create_table(
    data: &[u8],
) -> Option<(Schema, Vec<Option<Value>>, Vec<bool>)> {
    let mut pos = 0usize;
    if data.len() < 4 {
        return None;
    }
    let name_len = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
    pos += 4;
    if pos + name_len > data.len() {
        return None;
    }
    let table_name = std::str::from_utf8(&data[pos..pos + name_len])
        .ok()?
        .to_string();
    pos += name_len;
    if pos + 2 > data.len() {
        return None;
    }
    let n_cols = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
    pos += 2;
    let mut columns = Vec::with_capacity(n_cols);
    for _ in 0..n_cols {
        if pos + 4 > data.len() {
            return None;
        }
        let cn_len = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        pos += 4;
        if pos + cn_len + 4 > data.len() {
            return None;
        }
        let col_name = std::str::from_utf8(&data[pos..pos + cn_len])
            .ok()?
            .to_string();
        pos += cn_len;
        let type_id = TypeId::from_u8(data[pos])?;
        pos += 1;
        let required = data[pos] != 0;
        pos += 1;
        if pos + 2 > data.len() {
            return None;
        }
        let position = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?);
        pos += 2;
        columns.push(ColumnDef {
            name: col_name,
            type_id,
            required,
            position,
        });
    }
    // Trailing sections are present on records written after each feature
    // landed; older records end early, decoding to "none".
    let defaults = if pos < data.len() {
        decode_defaults_section(data, &mut pos, columns.len())?
    } else {
        Vec::new()
    };
    let auto_cols = if pos < data.len() {
        decode_auto_section(data, &mut pos, columns.len())?
    } else {
        Vec::new()
    };
    Some((
        Schema {
            table_name,
            columns,
        },
        defaults,
        auto_cols,
    ))
}

pub(super) fn encode_ddl_drop_table(table_name: &str) -> Vec<u8> {
    let name = table_name.as_bytes();
    let mut out = Vec::with_capacity(4 + name.len());
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name);
    out
}

pub(super) fn encode_ddl_alter_add_column(table_name: &str, col: &ColumnDef) -> Vec<u8> {
    let name = table_name.as_bytes();
    let cn = col.name.as_bytes();
    let mut out = Vec::with_capacity(4 + name.len() + 4 + cn.len() + 4);
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&(cn.len() as u32).to_le_bytes());
    out.extend_from_slice(cn);
    out.push(col.type_id as u8);
    out.push(col.required as u8);
    out.extend_from_slice(&col.position.to_le_bytes());
    out
}

pub(super) fn encode_ddl_alter_drop_column(table_name: &str, col_name: &str) -> Vec<u8> {
    let name = table_name.as_bytes();
    let cn = col_name.as_bytes();
    let mut out = Vec::with_capacity(4 + name.len() + 4 + cn.len());
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&(cn.len() as u32).to_le_bytes());
    out.extend_from_slice(cn);
    out
}

pub(super) fn decode_ddl_table_name(data: &[u8]) -> Option<(String, usize)> {
    if data.len() < 4 {
        return None;
    }
    let name_len = u32::from_le_bytes(data[0..4].try_into().ok()?) as usize;
    if 4 + name_len > data.len() {
        return None;
    }
    let name = std::str::from_utf8(&data[4..4 + name_len])
        .ok()?
        .to_string();
    Some((name, 4 + name_len))
}

pub(super) fn decode_ddl_alter_add_column(data: &[u8]) -> Option<(String, ColumnDef)> {
    let (table_name, mut pos) = decode_ddl_table_name(data)?;
    if pos + 4 > data.len() {
        return None;
    }
    let cn_len = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
    pos += 4;
    if pos + cn_len + 4 > data.len() {
        return None;
    }
    let col_name = std::str::from_utf8(&data[pos..pos + cn_len])
        .ok()?
        .to_string();
    pos += cn_len;
    let type_id = TypeId::from_u8(data[pos])?;
    pos += 1;
    let required = data[pos] != 0;
    pos += 1;
    if pos + 2 > data.len() {
        return None;
    }
    let position = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?);
    Some((
        table_name,
        ColumnDef {
            name: col_name,
            type_id,
            required,
            position,
        },
    ))
}

pub(super) fn decode_ddl_alter_drop_column(data: &[u8]) -> Option<(String, String)> {
    let (table_name, pos) = decode_ddl_table_name(data)?;
    if pos + 4 > data.len() {
        return None;
    }
    let cn_len = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
    if pos + 4 + cn_len > data.len() {
        return None;
    }
    let col_name = std::str::from_utf8(&data[pos + 4..pos + 4 + cn_len])
        .ok()?
        .to_string();
    Some((table_name, col_name))
}

// ─── Catalog file format ────────────────────────────────────────────────────
//
// Layout (version 2):
//   magic     [4]      = "BCAT"
//   version   u16
//   n_tables  u32
//   for each table:
//     table_name_len  u32
//     table_name      utf8 bytes
//     n_columns       u16
//     for each column:
//       name_len      u32
//       name          utf8 bytes
//       type_id       u8
//       required      u8
//       position      u16
//     ── version 2 appends: ──
//     n_indexed_cols  u16
//     for each indexed column:
//       name_len      u32
//       name          utf8 bytes
//
// Version 1 files are accepted by the reader (same shape minus the
// trailing indexed-column block) and treated as having zero indexed
// columns. Writers always emit version 2 from Mission 3 onwards.

/// Per-indexed-column metadata persisted in the catalog file.
pub(crate) struct IndexedColMeta {
    pub name: String,
    pub unique: bool,
}

/// In-memory catalog entry pairing a schema with its indexed column list.
/// Produced by the reader; the writer takes the borrowed counterpart below.
pub(crate) struct CatalogEntry {
    pub schema: Schema,
    pub indexed_cols: Vec<IndexedColMeta>,
    pub expression_indexes: Vec<ExpressionIndexMeta>,
    /// Per-column defaults aligned to `schema.columns` by position. Empty when
    /// no column has a default (v1–v3 files always decode to empty).
    pub defaults: Vec<Option<Value>>,
    /// Which columns are `auto`, aligned to `schema.columns`. Empty when none
    /// (v1–v4 files always decode to empty).
    pub auto_cols: Vec<bool>,
}

/// Borrowed view passed to the writer.
pub(crate) struct CatalogEntryRef<'a> {
    pub schema: &'a Schema,
    pub indexed_cols: Vec<IndexedColMeta>,
    pub expression_indexes: Vec<ExpressionIndexMeta>,
    pub defaults: &'a [Option<Value>],
    pub auto_cols: &'a [bool],
}

// ─── Column-default codecs (shared by catalog.bin and the WAL DDL record) ────

/// Encode a single scalar value: a `type_id` tag byte followed by a
/// type-specific, length-prefixed (for variable-width types) payload. Lossless
/// — used to persist literal column defaults.
pub(super) fn encode_value_blob(out: &mut Vec<u8>, v: &Value) {
    out.push(v.type_id() as u8);
    match v {
        Value::Int(n) => out.extend_from_slice(&n.to_le_bytes()),
        Value::Float(f) => out.extend_from_slice(&f.to_bits().to_le_bytes()),
        Value::Bool(b) => out.push(*b as u8),
        Value::Str(s) => {
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Value::DateTime(n) => out.extend_from_slice(&n.to_le_bytes()),
        Value::Uuid(u) => out.extend_from_slice(u),
        Value::Bytes(b) => {
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
        Value::Json(b) => {
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
        Value::Empty => {}
    }
}

/// Inverse of [`encode_value_blob`]. Returns `None` on any malformed/truncated
/// input so a corrupt record fails closed rather than panicking.
pub(super) fn decode_value_blob(data: &[u8], pos: &mut usize) -> Option<Value> {
    let tag = *data.get(*pos)?;
    *pos += 1;
    let type_id = TypeId::from_u8(tag)?;
    let take_fixed = |pos: &mut usize, n: usize| -> Option<Vec<u8>> {
        if *pos + n > data.len() {
            return None;
        }
        let slice = data[*pos..*pos + n].to_vec();
        *pos += n;
        Some(slice)
    };
    match type_id {
        TypeId::Empty => Some(Value::Empty),
        TypeId::Int => Some(Value::Int(i64::from_le_bytes(
            take_fixed(pos, 8)?.try_into().ok()?,
        ))),
        TypeId::Float => Some(Value::Float(f64::from_bits(u64::from_le_bytes(
            take_fixed(pos, 8)?.try_into().ok()?,
        )))),
        TypeId::Bool => Some(Value::Bool(take_fixed(pos, 1)?[0] != 0)),
        TypeId::DateTime => Some(Value::DateTime(i64::from_le_bytes(
            take_fixed(pos, 8)?.try_into().ok()?,
        ))),
        TypeId::Uuid => Some(Value::Uuid(take_fixed(pos, 16)?.try_into().ok()?)),
        TypeId::Str => {
            let len = u32::from_le_bytes(take_fixed(pos, 4)?.try_into().ok()?) as usize;
            Some(Value::Str(String::from_utf8(take_fixed(pos, len)?).ok()?))
        }
        TypeId::Bytes => {
            let len = u32::from_le_bytes(take_fixed(pos, 4)?.try_into().ok()?) as usize;
            Some(Value::Bytes(take_fixed(pos, len)?))
        }
        TypeId::Json => {
            let len = u32::from_le_bytes(take_fixed(pos, 4)?.try_into().ok()?) as usize;
            Some(Value::Json(take_fixed(pos, len)?.into()))
        }
    }
}

/// Encode the per-table defaults as a sparse list: a `u16` count of columns
/// that have a default, then `(position: u16, value blob)` pairs. The common
/// "no defaults" case costs two bytes.
pub(super) fn encode_defaults_section(out: &mut Vec<u8>, defaults: &[Option<Value>]) {
    let present: Vec<(u16, &Value)> = defaults
        .iter()
        .enumerate()
        .filter_map(|(i, d)| d.as_ref().map(|v| (i as u16, v)))
        .collect();
    out.extend_from_slice(&(present.len() as u16).to_le_bytes());
    for (pos, v) in present {
        out.extend_from_slice(&pos.to_le_bytes());
        encode_value_blob(out, v);
    }
}

/// Inverse of [`encode_defaults_section`]. Builds a `Vec` of length `n_cols`
/// with `None` for columns without a default. Returns `None` on truncation.
pub(super) fn decode_defaults_section(
    data: &[u8],
    pos: &mut usize,
    n_cols: usize,
) -> Option<Vec<Option<Value>>> {
    if *pos + 2 > data.len() {
        return None;
    }
    let count = u16::from_le_bytes(data[*pos..*pos + 2].try_into().ok()?) as usize;
    *pos += 2;
    let mut out = vec![None; n_cols];
    for _ in 0..count {
        if *pos + 2 > data.len() {
            return None;
        }
        let col = u16::from_le_bytes(data[*pos..*pos + 2].try_into().ok()?) as usize;
        *pos += 2;
        let value = decode_value_blob(data, pos)?;
        if col < n_cols {
            out[col] = Some(value);
        }
    }
    Some(out)
}

/// Encode the per-table `auto` columns as a sparse list: a `u16` count of auto
/// columns, then their positions (`u16` each). "No auto columns" costs two
/// bytes.
pub(super) fn encode_auto_section(out: &mut Vec<u8>, auto_cols: &[bool]) {
    let present: Vec<u16> = auto_cols
        .iter()
        .enumerate()
        .filter_map(|(i, &a)| if a { Some(i as u16) } else { None })
        .collect();
    out.extend_from_slice(&(present.len() as u16).to_le_bytes());
    for pos in present {
        out.extend_from_slice(&pos.to_le_bytes());
    }
}

/// Inverse of [`encode_auto_section`]. Builds a `bool` vec of length `n_cols`.
/// Returns `None` on truncation.
pub(super) fn decode_auto_section(
    data: &[u8],
    pos: &mut usize,
    n_cols: usize,
) -> Option<Vec<bool>> {
    if *pos + 2 > data.len() {
        return None;
    }
    let count = u16::from_le_bytes(data[*pos..*pos + 2].try_into().ok()?) as usize;
    *pos += 2;
    let mut out = vec![false; n_cols];
    for _ in 0..count {
        if *pos + 2 > data.len() {
            return None;
        }
        let col = u16::from_le_bytes(data[*pos..*pos + 2].try_into().ok()?) as usize;
        *pos += 2;
        if col < n_cols {
            out[col] = true;
        }
    }
    Some(out)
}
