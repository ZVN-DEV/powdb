//! The on-disk catalog file: section encoders, the versioned reader with
//! its ceiling check, and the little-endian field helpers.

use super::*;

pub(super) fn push_catalog_string(out: &mut Vec<u8>, value: &str) -> io::Result<()> {
    let len = u32::try_from(value.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "catalog string is too large"))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

pub(super) fn encode_expression_indexes(
    out: &mut Vec<u8>,
    indexes: &[ExpressionIndexMeta],
) -> io::Result<()> {
    let count = u16::try_from(indexes.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many expression indexes on one table",
        )
    })?;
    out.extend_from_slice(&count.to_le_bytes());
    for index in indexes {
        out.extend_from_slice(&index.index_id.to_le_bytes());
        out.push(u8::from(index.unique));
        out.extend_from_slice(&index.canonical_version.to_le_bytes());
        push_catalog_string(out, &index.canonical_text)?;
        push_catalog_string(out, &index.json_path.column)?;
        let segment_count = u16::try_from(index.json_path.segments.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "JSON path has too many segments",
            )
        })?;
        out.extend_from_slice(&segment_count.to_le_bytes());
        for segment in &index.json_path.segments {
            match segment {
                StoredJsonPathSegmentV1::Key(key) => {
                    out.push(1);
                    push_catalog_string(out, key)?;
                }
                StoredJsonPathSegmentV1::Index(position) => {
                    out.push(2);
                    out.extend_from_slice(&position.to_le_bytes());
                }
            }
        }
    }
    Ok(())
}

pub(super) fn decode_expression_indexes(
    data: &[u8],
    pos: &mut usize,
) -> io::Result<Vec<ExpressionIndexMeta>> {
    let count = read_u16(data, pos)? as usize;
    let mut indexes = Vec::with_capacity(count);
    for _ in 0..count {
        let index_id = read_u64(data, pos)?;
        if index_id == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expression index id must be non-zero",
            ));
        }
        let unique = read_u8(data, pos)? != 0;
        let canonical_version = read_u16(data, pos)?;
        let canonical_len = read_u32(data, pos)? as usize;
        let canonical_text = read_string(data, pos, canonical_len)?;
        let column_len = read_u32(data, pos)? as usize;
        let column = read_string(data, pos, column_len)?;
        let segment_count = read_u16(data, pos)? as usize;
        let mut segments = Vec::with_capacity(segment_count);
        for _ in 0..segment_count {
            match read_u8(data, pos)? {
                1 => {
                    let len = read_u32(data, pos)? as usize;
                    segments.push(StoredJsonPathSegmentV1::Key(read_string(data, pos, len)?));
                }
                2 => segments.push(StoredJsonPathSegmentV1::Index(read_u32(data, pos)?)),
                tag => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unknown stored JSON path segment tag: {tag}"),
                    ));
                }
            }
        }
        indexes.push(ExpressionIndexMeta {
            index_id,
            unique,
            canonical_version,
            canonical_text,
            json_path: StoredJsonPathV1 { column, segments },
        });
    }
    Ok(indexes)
}

/// Encode the v7 relationship-link section: a `u32` count followed by that many
/// records of five length-prefixed strings plus one `u8` kind, matching the
/// len-prefix conventions the table/column/expression-index codecs use.
pub(super) fn encode_links_section(out: &mut Vec<u8>, links: &[LinkDef]) -> io::Result<()> {
    let count = u32::try_from(links.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many catalog links"))?;
    out.extend_from_slice(&count.to_le_bytes());
    for link in links {
        push_catalog_string(out, &link.owner_type)?;
        push_catalog_string(out, &link.name)?;
        push_catalog_string(out, &link.target_type)?;
        push_catalog_string(out, &link.local_key)?;
        push_catalog_string(out, &link.target_key)?;
        out.push(link.kind.to_u8());
    }
    Ok(())
}

/// Inverse of [`encode_links_section`]. Bounds-checks the count against the
/// remaining buffer so a corrupt file fails closed rather than pre-allocating a
/// huge `Vec` (mirrors the table-count and btree node-count guards).
pub(super) fn decode_links_section(data: &[u8], pos: &mut usize) -> io::Result<Vec<LinkDef>> {
    let count = read_u32(data, pos)? as usize;
    if count > data.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("catalog file corrupt: implausible link count {count}"),
        ));
    }
    let mut links = Vec::with_capacity(count);
    for _ in 0..count {
        let owner_type = read_len_prefixed_string(data, pos)?;
        let name = read_len_prefixed_string(data, pos)?;
        let target_type = read_len_prefixed_string(data, pos)?;
        let local_key = read_len_prefixed_string(data, pos)?;
        let target_key = read_len_prefixed_string(data, pos)?;
        let kind = LinkKind::from_u8(read_u8(data, pos)?)?;
        links.push(LinkDef {
            owner_type,
            name,
            target_type,
            local_key,
            target_key,
            kind,
        });
    }
    Ok(links)
}

pub(super) fn read_len_prefixed_string(data: &[u8], pos: &mut usize) -> io::Result<String> {
    let len = read_u32(data, pos)? as usize;
    read_string(data, pos, len)
}

pub(super) fn write_catalog_file(
    path: &Path,
    version: u16,
    next_index_id: u64,
    entries: &[CatalogEntryRef<'_>],
    links: &[LinkDef],
) -> io::Result<()> {
    if !(1..=CATALOG_VERSION).contains(&version) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported catalog write version: {version}"),
        ));
    }
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    buf.extend_from_slice(CATALOG_MAGIC);
    buf.extend_from_slice(&version.to_le_bytes());
    buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    if version >= 6 {
        buf.extend_from_slice(&next_index_id.to_le_bytes());
    }

    for entry in entries {
        let schema = entry.schema;
        let name = schema.table_name.as_bytes();
        buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
        buf.extend_from_slice(name);
        buf.extend_from_slice(&(schema.columns.len() as u16).to_le_bytes());
        for col in &schema.columns {
            let cn = col.name.as_bytes();
            buf.extend_from_slice(&(cn.len() as u32).to_le_bytes());
            buf.extend_from_slice(cn);
            buf.push(col.type_id as u8);
            buf.push(if col.required { 1 } else { 0 });
            buf.extend_from_slice(&col.position.to_le_bytes());
        }
        // Per-table indexed column list with uniqueness flags (version 3).
        buf.extend_from_slice(&(entry.indexed_cols.len() as u16).to_le_bytes());
        for meta in &entry.indexed_cols {
            let cn = meta.name.as_bytes();
            buf.extend_from_slice(&(cn.len() as u32).to_le_bytes());
            buf.extend_from_slice(cn);
            buf.push(if meta.unique { 1 } else { 0 });
        }
        // Per-table column defaults (version 4).
        encode_defaults_section(&mut buf, entry.defaults);
        // Per-table auto-increment columns (version 5).
        encode_auto_section(&mut buf, entry.auto_cols);
        if version >= 6 {
            encode_expression_indexes(&mut buf, &entry.expression_indexes)?;
        }
    }

    // Version 7 appends the relationship-link section after every table entry
    // and before the CRC. A v6-or-older file omits it entirely (the reader
    // defaults n_links = 0), so a link-free database stays byte-for-byte
    // unchanged.
    if version >= 7 {
        encode_links_section(&mut buf, links)?;
    }

    // Append a CRC32 checksum of the entire payload so the reader can
    // detect corruption (the WAL and btree .idx files already do this;
    // catalog.bin was the one file missing a checksum).
    let crc = crc32fast::hash(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());

    let mut f = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    f.write_all(&buf)?;
    f.sync_data()?;
    Ok(())
}

pub(super) struct CatalogFile {
    pub(super) version: u16,
    pub(super) next_index_id: u64,
    pub(super) entries: Vec<CatalogEntry>,
    pub(super) links: Vec<LinkDef>,
}

pub(super) fn read_catalog_file(path: &Path) -> io::Result<CatalogFile> {
    read_catalog_file_with_max_version(path, CATALOG_VERSION)
}

/// Read the catalog format version currently persisted on disk for `data_dir`
/// without rehydrating tables. This is the database's *active* catalog version:
/// a database that has never activated an expression index stays at
/// [`LEGACY_CATALOG_VERSION`]. Sync producers use it to stamp published segments
/// with the active version rather than this binary's compile-time maximum.
pub fn read_active_catalog_version(data_dir: &Path) -> io::Result<u16> {
    let cat_path = data_dir.join(CATALOG_FILE);
    Ok(read_catalog_file(&cat_path)?.version)
}

pub(super) fn read_catalog_file_with_max_version(
    path: &Path,
    max_supported_version: u16,
) -> io::Result<CatalogFile> {
    let mut f = fs::File::open(path)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;

    let mut pos = 0usize;
    // Minimum: 4 (magic) + 2 (version) + 4 (n_tables) + 4 (crc) = 14
    if buf.len() < 14 || &buf[0..4] != CATALOG_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad catalog magic",
        ));
    }

    // Verify the trailing CRC32 checksum.
    let payload = &buf[..buf.len() - 4];
    let stored_crc = u32::from_le_bytes(
        buf[buf.len() - 4..]
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "truncated catalog CRC"))?,
    );
    let computed_crc = crc32fast::hash(payload);
    if stored_crc != computed_crc {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "catalog CRC32 mismatch: expected {stored_crc:#010x}, got {computed_crc:#010x}"
            ),
        ));
    }
    // Strip the CRC suffix so the parsing loop below doesn't walk into it.
    let buf = &buf[..buf.len() - 4];
    pos += 4;
    let version = u16::from_le_bytes(
        buf[pos..pos + 2]
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "truncated catalog header"))?,
    );
    pos += 2;
    // Accept every version from 1 up to the current CATALOG_VERSION: the
    // field-reading staircase below fills in fields a newer version added
    // (indexed-col uniqueness at v3, defaults at v4, auto columns at v5) and
    // defaults them for older files, so any 1..=CATALOG_VERSION file loads.
    // A range check (not an enumerated list) is what makes this back-compat
    // hold automatically on the next bump — the previous `version != 1 &&
    // version != 2 && version != CATALOG_VERSION` form silently rejected the
    // intermediate v3/v4 files when the constant moved to 5, which would have
    // failed to open a v0.6.x database on upgrade (data loss).
    if version == 0 || version > max_supported_version {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported catalog version: {version}"),
        ));
    }
    let n_tables = u32::from_le_bytes(
        buf[pos..pos + 4]
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "truncated catalog header"))?,
    ) as usize;
    pos += 4;
    // Additive legacy read branches. Version history: v1 (no index list),
    // v2 (index names), v3 (uniqueness flag), all written by pre-v0.5.0
    // builds; v4 (column defaults) and v5 (auto-increment columns), both
    // introduced in v0.7.0; v6 (expression indexes + next-index-id header),
    // activated lazily since v0.13.0. v5 is still written today by databases
    // that never activate an expression index, so only v1-v4 are legacy.
    // Per the support policy in docs/FORMAT.md (4 minor versions after
    // superseded), the v1-v4 branches became removable in v0.11.0 at the
    // earliest; they are retained deliberately.
    let next_index_id = if version >= 6 {
        let id = read_u64(buf, &mut pos)?;
        if id == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "catalog next index id must be non-zero",
            ));
        }
        id
    } else {
        // Legacy v1-v5 (pre-v0.13.0, or v6 never activated): no
        // next-index-id header field.
        1
    };

    // Don't size an allocation from an unvalidated count: a corrupt or hostile
    // catalog could claim billions of tables and make the `Vec::with_capacity`
    // below attempt a huge allocation (host abort — fatal in embedded mode). A
    // file of `buf.len()` bytes can describe at most that many tables (each
    // needs several header bytes), so a larger count is corrupt. Mirrors the
    // btree's node-count guard.
    if n_tables > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("catalog file corrupt: implausible table count {n_tables}"),
        ));
    }

    let mut entries = Vec::with_capacity(n_tables);
    for _ in 0..n_tables {
        let name_len = read_u32(buf, &mut pos)? as usize;
        let table_name = read_string(buf, &mut pos, name_len)?;
        let n_cols = read_u16(buf, &mut pos)? as usize;

        let mut columns = Vec::with_capacity(n_cols);
        for _ in 0..n_cols {
            let cname_len = read_u32(buf, &mut pos)? as usize;
            let name = read_string(buf, &mut pos, cname_len)?;
            let type_id_raw = read_u8(buf, &mut pos)?;
            let type_id = type_id_from_u8(type_id_raw)?;
            let required = read_u8(buf, &mut pos)? != 0;
            let position = read_u16(buf, &mut pos)?;
            columns.push(ColumnDef {
                name,
                type_id,
                required,
                position,
            });
        }

        // Version 3 appends indexed column list with uniqueness flag.
        // Version 2 has indexed column names without uniqueness (default
        // to non-unique). Version 1 has no index info at all. v1/v2 files
        // (pre-v0.5.0 writers) are legacy; removable per the docs/FORMAT.md
        // policy (floor long passed), kept deliberately.
        let indexed_cols: Vec<IndexedColMeta> = if version >= 3 {
            let n = read_u16(buf, &mut pos)? as usize;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                let l = read_u32(buf, &mut pos)? as usize;
                let name = read_string(buf, &mut pos, l)?;
                let unique = read_u8(buf, &mut pos)? != 0;
                v.push(IndexedColMeta { name, unique });
            }
            v
        } else if version >= 2 {
            let n = read_u16(buf, &mut pos)? as usize;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                let l = read_u32(buf, &mut pos)? as usize;
                let name = read_string(buf, &mut pos, l)?;
                v.push(IndexedColMeta {
                    name,
                    unique: false,
                });
            }
            v
        } else {
            Vec::new()
        };

        // Version 4 appends a column-defaults section after the index list
        // (v0.7.0). Legacy v1-v3 files have none; that branch became
        // removable in v0.11.0 per docs/FORMAT.md, kept deliberately.
        let defaults = if version >= 4 {
            decode_defaults_section(buf, &mut pos, columns.len()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "truncated catalog defaults")
            })?
        } else {
            Vec::new()
        };

        // Version 5 appends an auto-increment column section after that
        // (v0.7.0). Legacy v1-v4 files have none; that branch became
        // removable in v0.11.0 per docs/FORMAT.md, kept deliberately.
        let auto_cols = if version >= 5 {
            decode_auto_section(buf, &mut pos, columns.len()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "truncated catalog auto columns")
            })?
        } else {
            Vec::new()
        };

        // Version 6 appends an expression-index section (lazily activated
        // since v0.13.0). v5 is still an active writer version, so the
        // below-6 branch is NOT legacy and is not removal-eligible.
        let expression_indexes = if version >= 6 {
            decode_expression_indexes(buf, &mut pos)?
        } else {
            Vec::new()
        };

        entries.push(CatalogEntry {
            schema: Schema {
                table_name,
                columns,
            },
            indexed_cols,
            expression_indexes,
            defaults,
            auto_cols,
        });
    }

    // Version 7 appends a relationship-link section after the table entries.
    // Any pre-v7 file stops here; the reader defaults n_links = 0 (staircase
    // contract). v6 remains an active writer version, so the below-7 branch is
    // NOT legacy and is not removal-eligible.
    let links = if version >= 7 {
        decode_links_section(buf, &mut pos)?
    } else {
        Vec::new()
    };

    let mut seen_index_ids = FxHashMap::default();
    let mut max_index_id = 0;
    for entry in &entries {
        for index in &entry.expression_indexes {
            if index.canonical_version == 0 || index.canonical_text.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expression index has invalid canonical identity",
                ));
            }
            if index.canonical_version == 1
                && index.canonical_text != index.json_path.canonical_text()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expression index canonical identity does not match its JSON path",
                ));
            }
            let Some(root) = entry
                .schema
                .columns
                .iter()
                .find(|column| column.name == index.json_path.column)
            else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expression index JSON root is absent from its table",
                ));
            };
            if root.type_id != TypeId::Json {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expression index root column is not JSON",
                ));
            }
            if seen_index_ids.insert(index.index_id, ()).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "duplicate expression index id in catalog",
                ));
            }
            max_index_id = max_index_id.max(index.index_id);
        }
    }
    if next_index_id <= max_index_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "catalog next index id does not exceed persisted index ids",
        ));
    }
    Ok(CatalogFile {
        version,
        next_index_id,
        entries,
        links,
    })
}

pub(super) fn read_u8(buf: &[u8], pos: &mut usize) -> io::Result<u8> {
    if *pos >= buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated catalog",
        ));
    }
    let v = buf[*pos];
    *pos += 1;
    Ok(v)
}
pub(super) fn read_u16(buf: &[u8], pos: &mut usize) -> io::Result<u16> {
    if *pos + 2 > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated catalog",
        ));
    }
    let v = u16::from_le_bytes(
        buf[*pos..*pos + 2]
            .try_into()
            .expect("bounds checked above"),
    );
    *pos += 2;
    Ok(v)
}
pub(super) fn read_u32(buf: &[u8], pos: &mut usize) -> io::Result<u32> {
    if *pos + 4 > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated catalog",
        ));
    }
    let v = u32::from_le_bytes(
        buf[*pos..*pos + 4]
            .try_into()
            .expect("bounds checked above"),
    );
    *pos += 4;
    Ok(v)
}
pub(super) fn read_u64(buf: &[u8], pos: &mut usize) -> io::Result<u64> {
    if *pos + 8 > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated catalog",
        ));
    }
    let value = u64::from_le_bytes(
        buf[*pos..*pos + 8]
            .try_into()
            .expect("bounds checked above"),
    );
    *pos += 8;
    Ok(value)
}
pub(super) fn read_string(buf: &[u8], pos: &mut usize, len: usize) -> io::Result<String> {
    if *pos + len > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated catalog string",
        ));
    }
    let s = std::str::from_utf8(&buf[*pos..*pos + len])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 in catalog"))?
        .to_string();
    *pos += len;
    Ok(s)
}
pub(super) fn type_id_from_u8(v: u8) -> io::Result<TypeId> {
    match v {
        0 => Ok(TypeId::Empty),
        1 => Ok(TypeId::Int),
        2 => Ok(TypeId::Float),
        3 => Ok(TypeId::Bool),
        4 => Ok(TypeId::Str),
        5 => Ok(TypeId::DateTime),
        6 => Ok(TypeId::Uuid),
        7 => Ok(TypeId::Bytes),
        8 => Ok(TypeId::Json),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown type id: {v}"),
        )),
    }
}
