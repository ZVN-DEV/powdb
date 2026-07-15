use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use powdb_storage::catalog::CATALOG_VERSION;
use powdb_storage::create_data_dir_secure;
use powdb_storage::wal::{WalRecord, WAL_FORMAT_VERSION};

const SEGMENT_MAGIC: &[u8; 4] = b"PRUL";
const FOOTER_MAGIC: &[u8; 4] = b"RULF";
pub const RETAINED_SEGMENT_FORMAT_VERSION: u16 = 1;

const RESERVED: u16 = 0;
const HEADER_LEN: usize = 4 + 2 + 2 + 2 + 2 + 4 + 8 + 8 + 8 + 16;
const UNIT_HEADER_LEN: usize = 4 + 4 + 8 + 1 + 8;
const FOOTER_LEN: usize = 4 + 4;
const MAX_RETAINED_UNIT_DATA_SIZE: usize = 256 * 1024 * 1024;
const MAX_RETAINED_SEGMENT_FILE_SIZE: u64 = 1024 * 1024 * 1024;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Segment identity metadata used to reject forked or incompatible histories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentIdentity {
    pub database_id: [u8; 16],
    pub primary_generation: u64,
    pub wal_format_version: u16,
    pub catalog_version: u16,
}

impl SegmentIdentity {
    /// Stamp an identity with this binary's maximum supported catalog version.
    ///
    /// Use this at *consumer* sites that want "the newest catalog format this
    /// binary can read" as the expected identity. Producers that publish
    /// segments must instead stamp the database's *active* catalog version with
    /// [`Self::with_catalog_version`], so a database that has not yet activated
    /// v6 keeps stamping v5 (old-replica compatibility).
    pub fn current(database_id: [u8; 16], primary_generation: u64) -> Self {
        Self::with_catalog_version(database_id, primary_generation, CATALOG_VERSION)
    }

    /// Stamp an identity with an explicit (active) catalog version.
    ///
    /// The catalog version is a *compatibility annotation*, not lineage: it
    /// records the on-disk catalog format the producing database was running
    /// when the segment was published. It is deliberately excluded from
    /// [`Self::lineage_matches`]; consumers accept an older annotation and
    /// reject a newer one via [`validate_identity`].
    pub fn with_catalog_version(
        database_id: [u8; 16],
        primary_generation: u64,
        catalog_version: u16,
    ) -> Self {
        Self {
            database_id,
            primary_generation,
            wal_format_version: WAL_FORMAT_VERSION,
            catalog_version,
        }
    }

    /// True when the strict lineage fields (database id, primary generation,
    /// WAL format) match. `catalog_version` is intentionally ignored: it is a
    /// compatibility annotation that legitimately increases across a segment
    /// chain when a database activates a newer catalog format mid-stream.
    pub fn lineage_matches(self, other: SegmentIdentity) -> bool {
        self.database_id == other.database_id
            && self.primary_generation == other.primary_generation
            && self.wal_format_version == other.wal_format_version
    }

    pub fn validate(self) -> io::Result<()> {
        validate_identity(self)
    }
}

/// One retained replication unit.
///
/// V1 stores current WAL records as units. The segment envelope owns durability,
/// identity, range, and corruption checks; the future applier can convert the
/// record type back into storage-layer replay actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedUnit {
    pub tx_id: u64,
    pub record_type: u8,
    pub lsn: u64,
    pub data: Vec<u8>,
}

impl From<&WalRecord> for RetainedUnit {
    fn from(record: &WalRecord) -> Self {
        Self {
            tx_id: record.tx_id,
            record_type: record.record_type as u8,
            lsn: record.lsn,
            data: record.data.clone(),
        }
    }
}

/// Immutable retained-unit segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedSegment {
    pub identity: SegmentIdentity,
    pub start_lsn: u64,
    pub end_lsn: u64,
    pub units: Vec<RetainedUnit>,
}

impl RetainedSegment {
    pub fn new(identity: SegmentIdentity, units: Vec<RetainedUnit>) -> io::Result<Self> {
        validate_identity(identity)?;
        validate_units(&units)?;
        let start_lsn = units[0].lsn;
        let end_lsn = units[units.len() - 1].lsn;
        Ok(Self {
            identity,
            start_lsn,
            end_lsn,
            units,
        })
    }

    pub fn to_bytes(&self) -> io::Result<Vec<u8>> {
        self.validate()?;

        let mut out = Vec::with_capacity(HEADER_LEN + FOOTER_LEN);
        out.extend_from_slice(SEGMENT_MAGIC);
        out.extend_from_slice(&RETAINED_SEGMENT_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&RESERVED.to_le_bytes());
        out.extend_from_slice(&self.identity.wal_format_version.to_le_bytes());
        out.extend_from_slice(&self.identity.catalog_version.to_le_bytes());
        out.extend_from_slice(&checked_unit_count(self.units.len())?.to_le_bytes());
        out.extend_from_slice(&self.start_lsn.to_le_bytes());
        out.extend_from_slice(&self.end_lsn.to_le_bytes());
        out.extend_from_slice(&self.identity.primary_generation.to_le_bytes());
        out.extend_from_slice(&self.identity.database_id);

        for unit in &self.units {
            let data_len_u32 = checked_data_len(unit.data.len())?;
            let total_len = (UNIT_HEADER_LEN as u32)
                .checked_add(data_len_u32)
                .ok_or_else(|| invalid_input("retained unit is too large"))?;
            let crc = unit_crc(unit);
            out.extend_from_slice(&total_len.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&unit.tx_id.to_le_bytes());
            out.push(unit.record_type);
            out.extend_from_slice(&unit.lsn.to_le_bytes());
            out.extend_from_slice(&unit.data);
        }

        let file_crc = crc32fast::hash(&out);
        out.extend_from_slice(FOOTER_MAGIC);
        out.extend_from_slice(&file_crc.to_le_bytes());
        Ok(out)
    }

    pub fn validate(&self) -> io::Result<()> {
        validate_identity(self.identity)?;
        validate_units(&self.units)?;
        if self.start_lsn != self.units[0].lsn {
            return Err(invalid_input(
                "retained segment start LSN does not match first unit",
            ));
        }
        if self.end_lsn != self.units[self.units.len() - 1].lsn {
            return Err(invalid_input(
                "retained segment end LSN does not match last unit",
            ));
        }
        Ok(())
    }

    pub fn from_bytes(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < HEADER_LEN + FOOTER_LEN {
            return Err(invalid_data("retained segment is truncated"));
        }

        let footer_start = bytes.len() - FOOTER_LEN;
        if &bytes[footer_start..footer_start + 4] != FOOTER_MAGIC {
            return Err(invalid_data("retained segment footer magic mismatch"));
        }
        let expected_file_crc = read_u32(bytes, footer_start + 4, "segment footer crc")?;
        let actual_file_crc = crc32fast::hash(&bytes[..footer_start]);
        if expected_file_crc != actual_file_crc {
            return Err(invalid_data("retained segment footer CRC mismatch"));
        }

        let mut pos = 0usize;
        if read_exact(bytes, &mut pos, 4, "segment magic")? != SEGMENT_MAGIC {
            return Err(invalid_data("retained segment magic mismatch"));
        }
        let segment_version = read_u16_at_cursor(bytes, &mut pos, "segment version")?;
        if segment_version != RETAINED_SEGMENT_FORMAT_VERSION {
            return Err(invalid_data("unsupported retained segment format version"));
        }
        let reserved = read_u16_at_cursor(bytes, &mut pos, "reserved")?;
        if reserved != RESERVED {
            return Err(invalid_data("retained segment reserved field must be zero"));
        }
        let wal_format_version = read_u16_at_cursor(bytes, &mut pos, "WAL format version")?;
        let catalog_version = read_u16_at_cursor(bytes, &mut pos, "catalog version")?;
        let record_count = read_u32_at_cursor(bytes, &mut pos, "record count")? as usize;
        let start_lsn = read_u64_at_cursor(bytes, &mut pos, "start LSN")?;
        let end_lsn = read_u64_at_cursor(bytes, &mut pos, "end LSN")?;
        let primary_generation = read_u64_at_cursor(bytes, &mut pos, "primary generation")?;
        let database_id_slice = read_exact(bytes, &mut pos, 16, "database id")?;
        let mut database_id = [0u8; 16];
        database_id.copy_from_slice(database_id_slice);

        let identity = SegmentIdentity {
            database_id,
            primary_generation,
            wal_format_version,
            catalog_version,
        };
        validate_identity(identity)?;

        if record_count == 0 {
            return Err(invalid_data(
                "retained segment must contain at least one unit",
            ));
        }
        let max_possible_units = footer_start.saturating_sub(pos) / UNIT_HEADER_LEN;
        if record_count > max_possible_units {
            return Err(invalid_data(
                "retained segment record count exceeds file capacity",
            ));
        }

        let mut units = Vec::new();
        units
            .try_reserve(record_count)
            .map_err(|_| invalid_data("retained segment record count is too large"))?;
        for _ in 0..record_count {
            if pos >= footer_start {
                return Err(invalid_data("retained segment record count exceeds file"));
            }
            let total_len = read_u32(bytes, pos, "unit total length")? as usize;
            if total_len < UNIT_HEADER_LEN {
                return Err(invalid_data("retained unit length is smaller than header"));
            }
            if pos
                .checked_add(total_len)
                .is_none_or(|end| end > footer_start)
            {
                return Err(invalid_data("retained unit extends beyond segment"));
            }
            let unit_start = pos;
            pos += 4;
            let stored_crc = read_u32_at_cursor(bytes, &mut pos, "unit crc")?;
            let tx_id = read_u64_at_cursor(bytes, &mut pos, "unit tx id")?;
            let record_type = read_u8_at_cursor(bytes, &mut pos, "unit record type")?;
            let lsn = read_u64_at_cursor(bytes, &mut pos, "unit LSN")?;
            let data_len = total_len - UNIT_HEADER_LEN;
            if data_len > MAX_RETAINED_UNIT_DATA_SIZE {
                return Err(invalid_data("retained unit payload exceeds maximum size"));
            }
            let data = read_exact(bytes, &mut pos, data_len, "unit payload")?.to_vec();
            debug_assert_eq!(pos, unit_start + total_len);

            let unit = RetainedUnit {
                tx_id,
                record_type,
                lsn,
                data,
            };
            if !is_known_record_type(unit.record_type) {
                return Err(invalid_data("unknown retained unit record type"));
            }
            if stored_crc != unit_crc(&unit) {
                return Err(invalid_data("retained unit CRC mismatch"));
            }
            units.push(unit);
        }

        if pos != footer_start {
            return Err(invalid_data("retained segment has trailing bytes"));
        }

        let segment = Self::new(identity, units)?;
        if segment.start_lsn != start_lsn || segment.end_lsn != end_lsn {
            return Err(invalid_data(
                "retained segment LSN range does not match records",
            ));
        }
        Ok(segment)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentFile {
    pub path: PathBuf,
    pub start_lsn: u64,
    pub end_lsn: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedTailAvailability {
    pub since_lsn: u64,
    pub through_lsn: u64,
    pub units_available: usize,
    pub first_lsn: Option<u64>,
    pub last_lsn: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedTailProgress {
    pub since_lsn: u64,
    pub requested_through_lsn: u64,
    pub contiguous_through_lsn: u64,
    pub units_available: usize,
    pub first_lsn: Option<u64>,
    pub last_lsn: Option<u64>,
}

pub fn segment_file_name(start_lsn: u64, end_lsn: u64) -> String {
    format!("retained-{start_lsn:020}-{end_lsn:020}.prul")
}

pub fn write_segment_atomic(dir: &Path, segment: &RetainedSegment) -> io::Result<PathBuf> {
    create_data_dir_secure(dir)?;
    let final_path = dir.join(segment_file_name(segment.start_lsn, segment.end_lsn));

    let temp_path = temp_segment_path(dir, segment);
    let bytes = segment.to_bytes()?;
    let write_result: io::Result<()> = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);

        // `rename` is atomic but not no-clobber on Unix. A hard link in the
        // same directory creates the final name only if it does not exist.
        fs::hard_link(&temp_path, &final_path)?;
        fsync_dir(dir)?;
        let _ = fs::remove_file(&temp_path);
        let _ = fsync_dir(dir);
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result?;
    Ok(final_path)
}

pub fn read_segment_file(path: &Path) -> io::Result<RetainedSegment> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if len > MAX_RETAINED_SEGMENT_FILE_SIZE {
        return Err(invalid_data("retained segment file exceeds maximum size"));
    }
    let capacity = usize::try_from(len)
        .map_err(|_| invalid_data("retained segment file size exceeds platform capacity"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| invalid_data("retained segment file is too large to allocate"))?;
    let mut limited = file.take(MAX_RETAINED_SEGMENT_FILE_SIZE + 1);
    limited.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RETAINED_SEGMENT_FILE_SIZE {
        return Err(invalid_data("retained segment file exceeds maximum size"));
    }
    RetainedSegment::from_bytes(&bytes)
}

pub fn list_segment_files(dir: &Path) -> io::Result<Vec<SegmentFile>> {
    let mut files = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(files),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Some((start_lsn, end_lsn)) = parse_segment_file_name(name) {
            files.push(SegmentFile {
                path,
                start_lsn,
                end_lsn,
            });
        }
    }
    files.sort_by_key(|file| (file.start_lsn, file.end_lsn));
    Ok(files)
}

pub fn read_units_since(
    dir: &Path,
    expected_identity: SegmentIdentity,
    since_lsn: u64,
    max_units: usize,
) -> io::Result<Vec<RetainedUnit>> {
    read_units_through(dir, expected_identity, since_lsn, u64::MAX, max_units)
}

pub fn read_units_through(
    dir: &Path,
    expected_identity: SegmentIdentity,
    since_lsn: u64,
    through_lsn: u64,
    max_units: usize,
) -> io::Result<Vec<RetainedUnit>> {
    validate_identity(expected_identity)?;
    if max_units == 0 || through_lsn <= since_lsn {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    let Some(mut expected_next_lsn) = since_lsn.checked_add(1) else {
        return Ok(out);
    };
    let mut have_started = false;
    let mut chain_catalog_version = 0u16;

    for file in list_segment_files(dir)? {
        if file.start_lsn > through_lsn {
            if expected_next_lsn <= through_lsn {
                return Err(invalid_data(format!(
                    "retained segment gap: expected LSN {}, found {}",
                    expected_next_lsn, file.start_lsn
                )));
            }
            break;
        }
        let segment = read_segment_file(&file.path)?;
        if segment.start_lsn != file.start_lsn || segment.end_lsn != file.end_lsn {
            return Err(invalid_data(format!(
                "retained segment filename range {}-{} does not match header range {}-{}",
                file.start_lsn, file.end_lsn, segment.start_lsn, segment.end_lsn
            )));
        }
        if !segment.identity.lineage_matches(expected_identity) {
            return Err(invalid_data(
                "retained segment identity does not match expected database history",
            ));
        }
        if segment.identity.catalog_version < chain_catalog_version {
            return Err(invalid_data(format!(
                "retained segment catalog format decreased across chain (v{} then v{}); rebootstrap required",
                chain_catalog_version, segment.identity.catalog_version
            )));
        }
        chain_catalog_version = segment.identity.catalog_version;

        if segment.end_lsn <= since_lsn {
            continue;
        }

        if !have_started {
            if segment.start_lsn > expected_next_lsn {
                return Err(invalid_data(format!(
                    "retained segment gap: expected LSN {}, found {}",
                    expected_next_lsn, segment.start_lsn
                )));
            }
        } else if segment.start_lsn < expected_next_lsn {
            return Err(invalid_data(format!(
                "retained segment overlap: expected LSN {}, found {}",
                expected_next_lsn, segment.start_lsn
            )));
        } else if segment.start_lsn > expected_next_lsn {
            return Err(invalid_data(format!(
                "retained segment gap: expected LSN {}, found {}",
                expected_next_lsn, segment.start_lsn
            )));
        }

        for unit in segment.units {
            if unit.lsn < expected_next_lsn {
                if have_started {
                    return Err(invalid_data(format!(
                        "retained unit overlap: expected LSN {}, found {}",
                        expected_next_lsn, unit.lsn
                    )));
                }
                continue;
            }
            if unit.lsn > expected_next_lsn {
                return Err(invalid_data(format!(
                    "retained unit gap: expected LSN {}, found {}",
                    expected_next_lsn, unit.lsn
                )));
            }
            if unit.lsn > through_lsn {
                return Ok(out);
            }
            out.push(unit);
            have_started = true;
            expected_next_lsn = expected_next_lsn
                .checked_add(1)
                .ok_or_else(|| invalid_data("retained unit LSN overflow"))?;
            if out.len() == max_units {
                return Ok(out);
            }
        }
    }
    Ok(out)
}

pub fn validate_retained_tail_available(
    dir: &Path,
    expected_identity: SegmentIdentity,
    since_lsn: u64,
    through_lsn: u64,
) -> io::Result<RetainedTailAvailability> {
    validate_identity(expected_identity)?;
    if through_lsn <= since_lsn {
        return Ok(RetainedTailAvailability {
            since_lsn,
            through_lsn,
            units_available: 0,
            first_lsn: None,
            last_lsn: None,
        });
    }

    let mut expected_next_lsn = since_lsn
        .checked_add(1)
        .ok_or_else(|| invalid_data("retained tail LSN overflow"))?;
    let mut units_available = 0usize;
    let mut first_lsn = None;
    let mut have_started = false;
    let mut chain_catalog_version = 0u16;

    for file in list_segment_files(dir)? {
        let segment = read_segment_file(&file.path)?;
        if segment.start_lsn != file.start_lsn || segment.end_lsn != file.end_lsn {
            return Err(invalid_data(format!(
                "retained segment filename range {}-{} does not match header range {}-{}",
                file.start_lsn, file.end_lsn, segment.start_lsn, segment.end_lsn
            )));
        }
        if !segment.identity.lineage_matches(expected_identity) {
            return Err(invalid_data(
                "retained segment identity does not match expected database history",
            ));
        }
        if segment.identity.catalog_version < chain_catalog_version {
            return Err(invalid_data(format!(
                "retained segment catalog format decreased across chain (v{} then v{}); rebootstrap required",
                chain_catalog_version, segment.identity.catalog_version
            )));
        }
        chain_catalog_version = segment.identity.catalog_version;

        if segment.end_lsn < expected_next_lsn {
            continue;
        }
        if !have_started {
            if segment.start_lsn > expected_next_lsn {
                return Err(invalid_data(format!(
                    "retained segment gap: expected LSN {}, found {}",
                    expected_next_lsn, segment.start_lsn
                )));
            }
        } else if segment.start_lsn < expected_next_lsn {
            return Err(invalid_data(format!(
                "retained segment overlap: expected LSN {}, found {}",
                expected_next_lsn, segment.start_lsn
            )));
        } else if segment.start_lsn > expected_next_lsn {
            return Err(invalid_data(format!(
                "retained segment gap: expected LSN {}, found {}",
                expected_next_lsn, segment.start_lsn
            )));
        }

        for unit in segment.units {
            if unit.lsn < expected_next_lsn {
                continue;
            }
            if unit.lsn > expected_next_lsn {
                return Err(invalid_data(format!(
                    "retained unit gap: expected LSN {}, found {}",
                    expected_next_lsn, unit.lsn
                )));
            }
            first_lsn = Some(first_lsn.unwrap_or(unit.lsn));
            have_started = true;
            units_available = units_available
                .checked_add(1)
                .ok_or_else(|| invalid_data("retained tail unit count overflow"))?;
            if unit.lsn >= through_lsn {
                return Ok(RetainedTailAvailability {
                    since_lsn,
                    through_lsn,
                    units_available,
                    first_lsn,
                    last_lsn: Some(unit.lsn),
                });
            }
            expected_next_lsn = expected_next_lsn
                .checked_add(1)
                .ok_or_else(|| invalid_data("retained tail LSN overflow"))?;
        }
    }

    Err(invalid_data(format!(
        "retained tail missing required LSN {} through {}",
        expected_next_lsn, through_lsn
    )))
}

pub fn retained_tail_progress(
    dir: &Path,
    expected_identity: SegmentIdentity,
    since_lsn: u64,
    requested_through_lsn: u64,
) -> io::Result<RetainedTailProgress> {
    validate_identity(expected_identity)?;
    if requested_through_lsn <= since_lsn {
        return Ok(RetainedTailProgress {
            since_lsn,
            requested_through_lsn,
            contiguous_through_lsn: since_lsn,
            units_available: 0,
            first_lsn: None,
            last_lsn: None,
        });
    }

    let mut expected_next_lsn = since_lsn
        .checked_add(1)
        .ok_or_else(|| invalid_data("retained tail LSN overflow"))?;
    let mut units_available = 0usize;
    let mut first_lsn = None;
    let mut last_lsn = None;
    let mut have_started = false;
    let mut chain_catalog_version = 0u16;

    for file in list_segment_files(dir)? {
        if file.end_lsn <= since_lsn {
            continue;
        }
        if file.start_lsn > requested_through_lsn {
            break;
        }

        let segment = read_segment_file(&file.path)?;
        if segment.start_lsn != file.start_lsn || segment.end_lsn != file.end_lsn {
            return Err(invalid_data(format!(
                "retained segment filename range {}-{} does not match header range {}-{}",
                file.start_lsn, file.end_lsn, segment.start_lsn, segment.end_lsn
            )));
        }
        if !segment.identity.lineage_matches(expected_identity) {
            return Err(invalid_data(
                "retained segment identity does not match expected database history",
            ));
        }
        if segment.identity.catalog_version < chain_catalog_version {
            return Err(invalid_data(format!(
                "retained segment catalog format decreased across chain (v{} then v{}); rebootstrap required",
                chain_catalog_version, segment.identity.catalog_version
            )));
        }
        chain_catalog_version = segment.identity.catalog_version;
        if segment.end_lsn < expected_next_lsn {
            continue;
        }

        if !have_started {
            if segment.start_lsn > expected_next_lsn {
                return Err(invalid_data(format!(
                    "retained segment gap: expected LSN {}, found {}",
                    expected_next_lsn, segment.start_lsn
                )));
            }
        } else if segment.start_lsn < expected_next_lsn {
            return Err(invalid_data(format!(
                "retained segment overlap: expected LSN {}, found {}",
                expected_next_lsn, segment.start_lsn
            )));
        } else if segment.start_lsn > expected_next_lsn {
            return Err(invalid_data(format!(
                "retained segment gap: expected LSN {}, found {}",
                expected_next_lsn, segment.start_lsn
            )));
        }

        for unit in segment.units {
            if unit.lsn < expected_next_lsn {
                continue;
            }
            if unit.lsn > expected_next_lsn {
                return Err(invalid_data(format!(
                    "retained unit gap: expected LSN {}, found {}",
                    expected_next_lsn, unit.lsn
                )));
            }
            if unit.lsn > requested_through_lsn {
                return Ok(RetainedTailProgress {
                    since_lsn,
                    requested_through_lsn,
                    contiguous_through_lsn: last_lsn.unwrap_or(since_lsn),
                    units_available,
                    first_lsn,
                    last_lsn,
                });
            }

            first_lsn = Some(first_lsn.unwrap_or(unit.lsn));
            last_lsn = Some(unit.lsn);
            have_started = true;
            units_available = units_available
                .checked_add(1)
                .ok_or_else(|| invalid_data("retained tail unit count overflow"))?;
            expected_next_lsn = expected_next_lsn
                .checked_add(1)
                .ok_or_else(|| invalid_data("retained tail LSN overflow"))?;
            if unit.lsn >= requested_through_lsn {
                return Ok(RetainedTailProgress {
                    since_lsn,
                    requested_through_lsn,
                    contiguous_through_lsn: unit.lsn,
                    units_available,
                    first_lsn,
                    last_lsn,
                });
            }
        }
    }

    Ok(RetainedTailProgress {
        since_lsn,
        requested_through_lsn,
        contiguous_through_lsn: last_lsn.unwrap_or(since_lsn),
        units_available,
        first_lsn,
        last_lsn,
    })
}

fn parse_segment_file_name(name: &str) -> Option<(u64, u64)> {
    let rest = name.strip_prefix("retained-")?;
    let rest = rest.strip_suffix(".prul")?;
    let (start, end) = rest.split_once('-')?;
    let start_lsn = start.parse().ok()?;
    let end_lsn = end.parse().ok()?;
    Some((start_lsn, end_lsn))
}

fn temp_segment_path(dir: &Path, segment: &RetainedSegment) -> PathBuf {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    dir.join(format!(
        ".{}.tmp.{}.{}.{}",
        segment_file_name(segment.start_lsn, segment.end_lsn),
        std::process::id(),
        nanos,
        counter
    ))
}

fn validate_identity(identity: SegmentIdentity) -> io::Result<()> {
    if identity.primary_generation == 0 {
        return Err(invalid_data(
            "retained segment primary generation must be non-zero",
        ));
    }
    if identity.database_id == [0; 16] {
        return Err(invalid_data(
            "retained segment database id must be non-zero",
        ));
    }
    if identity.wal_format_version != WAL_FORMAT_VERSION {
        return Err(invalid_data("retained segment WAL format is unsupported"));
    }
    // Catalog version is a compatibility annotation, not a strict-equality
    // identity field. A producer stamps its *active* catalog version, which may
    // legitimately be older than this binary's maximum (a database that never
    // activated v6 still stamps v5). Accept any version this binary can read
    // (1..=CATALOG_VERSION); reject only a version newer than we understand.
    if identity.catalog_version == 0 {
        return Err(invalid_data(
            "retained segment catalog format v0 is invalid",
        ));
    }
    if identity.catalog_version > CATALOG_VERSION {
        return Err(invalid_data(format!(
            "retained segment catalog format v{} is newer than this binary supports (max v{})",
            identity.catalog_version, CATALOG_VERSION
        )));
    }
    Ok(())
}

fn validate_units(units: &[RetainedUnit]) -> io::Result<()> {
    if units.is_empty() {
        return Err(invalid_input(
            "retained segment must contain at least one unit",
        ));
    }
    if units.len() > u32::MAX as usize {
        return Err(invalid_input("retained segment has too many units"));
    }

    let mut expected_lsn = units[0].lsn;
    if expected_lsn == 0 {
        return Err(invalid_input("retained unit LSN must be non-zero"));
    }
    for unit in units {
        if unit.lsn != expected_lsn {
            return Err(invalid_input("retained unit LSNs must be contiguous"));
        }
        if !is_known_record_type(unit.record_type) {
            return Err(invalid_input("unknown retained unit record type"));
        }
        checked_data_len(unit.data.len())?;
        expected_lsn = expected_lsn
            .checked_add(1)
            .ok_or_else(|| invalid_input("retained unit LSN overflow"))?;
    }
    Ok(())
}

fn is_known_record_type(record_type: u8) -> bool {
    matches!(record_type, 1..=10)
}

fn unit_crc(unit: &RetainedUnit) -> u32 {
    let mut crc_input = Vec::with_capacity(17 + unit.data.len());
    crc_input.extend_from_slice(&unit.tx_id.to_le_bytes());
    crc_input.push(unit.record_type);
    crc_input.extend_from_slice(&unit.lsn.to_le_bytes());
    crc_input.extend_from_slice(&unit.data);
    crc32fast::hash(&crc_input)
}

fn checked_unit_count(count: usize) -> io::Result<u32> {
    u32::try_from(count).map_err(|_| invalid_input("retained segment has too many units"))
}

fn checked_data_len(len: usize) -> io::Result<u32> {
    if len > MAX_RETAINED_UNIT_DATA_SIZE {
        return Err(invalid_input("retained unit payload exceeds maximum size"));
    }
    u32::try_from(len).map_err(|_| invalid_input("retained unit payload is too large"))
}

fn read_exact<'a>(
    bytes: &'a [u8],
    pos: &mut usize,
    len: usize,
    field: &str,
) -> io::Result<&'a [u8]> {
    let end = pos
        .checked_add(len)
        .ok_or_else(|| invalid_data(format!("{field} offset overflow")))?;
    if end > bytes.len() {
        return Err(invalid_data(format!("truncated {field}")));
    }
    let slice = &bytes[*pos..end];
    *pos = end;
    Ok(slice)
}

fn read_u8_at_cursor(bytes: &[u8], pos: &mut usize, field: &str) -> io::Result<u8> {
    let slice = read_exact(bytes, pos, 1, field)?;
    Ok(slice[0])
}

fn read_u16_at_cursor(bytes: &[u8], pos: &mut usize, field: &str) -> io::Result<u16> {
    let slice = read_exact(bytes, pos, 2, field)?;
    let arr: [u8; 2] = slice
        .try_into()
        .map_err(|_| invalid_data(format!("invalid {field}")))?;
    Ok(u16::from_le_bytes(arr))
}

fn read_u32_at_cursor(bytes: &[u8], pos: &mut usize, field: &str) -> io::Result<u32> {
    let slice = read_exact(bytes, pos, 4, field)?;
    let arr: [u8; 4] = slice
        .try_into()
        .map_err(|_| invalid_data(format!("invalid {field}")))?;
    Ok(u32::from_le_bytes(arr))
}

fn read_u64_at_cursor(bytes: &[u8], pos: &mut usize, field: &str) -> io::Result<u64> {
    let slice = read_exact(bytes, pos, 8, field)?;
    let arr: [u8; 8] = slice
        .try_into()
        .map_err(|_| invalid_data(format!("invalid {field}")))?;
    Ok(u64::from_le_bytes(arr))
}

fn read_u32(bytes: &[u8], pos: usize, field: &str) -> io::Result<u32> {
    let end = pos
        .checked_add(4)
        .ok_or_else(|| invalid_data(format!("{field} offset overflow")))?;
    if end > bytes.len() {
        return Err(invalid_data(format!("truncated {field}")));
    }
    let arr: [u8; 4] = bytes[pos..end]
        .try_into()
        .map_err(|_| invalid_data(format!("invalid {field}")))?;
    Ok(u32::from_le_bytes(arr))
}

#[cfg(unix)]
fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use powdb_storage::catalog::LEGACY_CATALOG_VERSION;
    use std::sync::{Arc, Barrier};

    fn identity() -> SegmentIdentity {
        SegmentIdentity::current(*b"0123456789abcdef", 7)
    }

    fn unit(lsn: u64) -> RetainedUnit {
        RetainedUnit {
            tx_id: 42,
            record_type: 1,
            lsn,
            data: format!("payload-{lsn}").into_bytes(),
        }
    }

    #[test]
    fn segment_round_trips() {
        let segment = RetainedSegment::new(identity(), vec![unit(10), unit(11)]).unwrap();
        let bytes = segment.to_bytes().unwrap();
        let decoded = RetainedSegment::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, segment);
    }

    #[test]
    fn rejects_empty_and_non_contiguous_segments() {
        assert!(RetainedSegment::new(identity(), Vec::new()).is_err());
        let err = RetainedSegment::new(identity(), vec![unit(10), unit(12)]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn public_segment_literal_is_validated_before_serialization() {
        let invalid = RetainedSegment {
            identity: identity(),
            start_lsn: 99,
            end_lsn: 100,
            units: vec![unit(1), unit(2)],
        };

        let err = invalid.to_bytes().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        let dir = tempfile::tempdir().unwrap();
        let err = write_segment_atomic(dir.path(), &invalid).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(
            list_segment_files(dir.path()).unwrap().is_empty(),
            "invalid public segment literals must not publish unreadable retained logs"
        );
    }

    #[test]
    fn rejects_corrupt_footer_checksum() {
        let segment = RetainedSegment::new(identity(), vec![unit(1), unit(2)]).unwrap();
        let mut bytes = segment.to_bytes().unwrap();
        bytes[HEADER_LEN + 3] ^= 0x55;
        let err = RetainedSegment::from_bytes(&bytes).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_truncated_segment() {
        let segment = RetainedSegment::new(identity(), vec![unit(1), unit(2)]).unwrap();
        let bytes = segment.to_bytes().unwrap();
        let truncated = &bytes[..bytes.len() - 3];
        let err = RetainedSegment::from_bytes(truncated).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn atomic_publish_and_range_read_work() {
        let dir = tempfile::tempdir().unwrap();
        let first = RetainedSegment::new(identity(), vec![unit(1), unit(2)]).unwrap();
        let second = RetainedSegment::new(identity(), vec![unit(3), unit(4)]).unwrap();

        let first_path = write_segment_atomic(dir.path(), &first).unwrap();
        let second_path = write_segment_atomic(dir.path(), &second).unwrap();
        assert!(first_path.exists());
        assert!(second_path.exists());

        let files = list_segment_files(dir.path()).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].start_lsn, 1);
        assert_eq!(files[1].start_lsn, 3);

        let units = read_units_since(dir.path(), identity(), 2, 10).unwrap();
        let lsns: Vec<u64> = units.into_iter().map(|unit| unit.lsn).collect();
        assert_eq!(lsns, vec![3, 4]);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_publish_creates_owner_only_segment_dir() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let segment_dir = dir.path().join("retained");
        let segment = RetainedSegment::new(identity(), vec![unit(1)]).unwrap();

        write_segment_atomic(&segment_dir, &segment).unwrap();

        let mode = std::fs::metadata(&segment_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn read_segment_file_rejects_oversized_file_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(segment_file_name(1, 1));
        let file = File::create(&path).unwrap();
        file.set_len(MAX_RETAINED_SEGMENT_FILE_SIZE + 1).unwrap();
        drop(file);

        let err = read_segment_file(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("maximum size"));
    }

    #[test]
    fn range_read_honors_limit() {
        let dir = tempfile::tempdir().unwrap();
        let segment = RetainedSegment::new(identity(), vec![unit(1), unit(2), unit(3)]).unwrap();
        write_segment_atomic(dir.path(), &segment).unwrap();

        let units = read_units_since(dir.path(), identity(), 0, 2).unwrap();
        let lsns: Vec<u64> = units.into_iter().map(|unit| unit.lsn).collect();
        assert_eq!(lsns, vec![1, 2]);
    }

    #[test]
    fn range_read_stops_at_through_lsn() {
        let dir = tempfile::tempdir().unwrap();
        let segment = RetainedSegment::new(
            identity(),
            vec![unit(1), unit(2), unit(3), unit(4), unit(5)],
        )
        .unwrap();
        write_segment_atomic(dir.path(), &segment).unwrap();

        let units = read_units_through(dir.path(), identity(), 1, 3, 10).unwrap();
        let lsns: Vec<u64> = units.into_iter().map(|unit| unit.lsn).collect();
        assert_eq!(lsns, vec![2, 3]);
    }

    #[test]
    fn retained_tail_progress_reports_contiguous_prefix_without_requiring_target() {
        let dir = tempfile::tempdir().unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity(), vec![unit(1), unit(2), unit(3)]).unwrap(),
        )
        .unwrap();

        let progress = retained_tail_progress(dir.path(), identity(), 0, 5).unwrap();
        assert_eq!(progress.since_lsn, 0);
        assert_eq!(progress.requested_through_lsn, 5);
        assert_eq!(progress.contiguous_through_lsn, 3);
        assert_eq!(progress.units_available, 3);
        assert_eq!(progress.first_lsn, Some(1));
        assert_eq!(progress.last_lsn, Some(3));
    }

    #[test]
    fn retained_tail_progress_rejects_gapped_retained_history() {
        let dir = tempfile::tempdir().unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity(), vec![unit(1), unit(2)]).unwrap(),
        )
        .unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity(), vec![unit(4), unit(5)]).unwrap(),
        )
        .unwrap();

        let err = retained_tail_progress(dir.path(), identity(), 0, 5).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("gap"));
    }

    #[test]
    fn concurrent_publish_same_range_is_no_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let segment = Arc::new(RetainedSegment::new(identity(), vec![unit(1), unit(2)]).unwrap());
        let barrier = Arc::new(Barrier::new(8));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let dir = dir.path().to_path_buf();
            let segment = Arc::clone(&segment);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                write_segment_atomic(&dir, &segment)
            }));
        }

        let mut successes = 0usize;
        let mut already_exists = 0usize;
        for handle in handles {
            match handle.join().unwrap() {
                Ok(_) => successes += 1,
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => already_exists += 1,
                Err(err) => panic!("unexpected publish error: {err}"),
            }
        }
        assert_eq!(successes, 1);
        assert_eq!(already_exists, 7);

        let files = list_segment_files(dir.path()).unwrap();
        assert_eq!(files.len(), 1);
        let temp_files = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(temp_files, 0);
    }

    #[test]
    fn read_units_since_rejects_missing_segment_gap() {
        let dir = tempfile::tempdir().unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity(), vec![unit(1), unit(2)]).unwrap(),
        )
        .unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity(), vec![unit(4), unit(5)]).unwrap(),
        )
        .unwrap();

        let err = read_units_since(dir.path(), identity(), 0, 10).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("gap"));
    }

    #[test]
    fn validate_retained_tail_available_accepts_contiguous_tail() {
        let dir = tempfile::tempdir().unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity(), vec![unit(1), unit(2)]).unwrap(),
        )
        .unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity(), vec![unit(3), unit(4), unit(5)]).unwrap(),
        )
        .unwrap();

        let availability = validate_retained_tail_available(dir.path(), identity(), 2, 5).unwrap();
        assert_eq!(availability.since_lsn, 2);
        assert_eq!(availability.through_lsn, 5);
        assert_eq!(availability.units_available, 3);
        assert_eq!(availability.first_lsn, Some(3));
        assert_eq!(availability.last_lsn, Some(5));
    }

    #[test]
    fn validate_retained_tail_available_allows_empty_target_range() {
        let dir = tempfile::tempdir().unwrap();
        let availability = validate_retained_tail_available(dir.path(), identity(), 7, 7).unwrap();
        assert_eq!(availability.units_available, 0);
        assert_eq!(availability.first_lsn, None);
        assert_eq!(availability.last_lsn, None);
    }

    #[test]
    fn validate_retained_tail_available_rejects_missing_tail() {
        let dir = tempfile::tempdir().unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity(), vec![unit(1), unit(2)]).unwrap(),
        )
        .unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity(), vec![unit(4), unit(5)]).unwrap(),
        )
        .unwrap();

        let err = validate_retained_tail_available(dir.path(), identity(), 2, 5).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("gap"));
    }

    #[test]
    fn validate_retained_tail_available_rejects_overlapping_segments() {
        let dir = tempfile::tempdir().unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity(), vec![unit(1), unit(2), unit(3)]).unwrap(),
        )
        .unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity(), vec![unit(3), unit(4)]).unwrap(),
        )
        .unwrap();

        let err = validate_retained_tail_available(dir.path(), identity(), 0, 4).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("overlap"));
    }

    #[test]
    fn read_units_since_rejects_overlapping_segments() {
        let dir = tempfile::tempdir().unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity(), vec![unit(1), unit(2)]).unwrap(),
        )
        .unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity(), vec![unit(2), unit(3)]).unwrap(),
        )
        .unwrap();

        let err = read_units_since(dir.path(), identity(), 0, 10).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("overlap"));
    }

    #[test]
    fn read_units_since_rejects_filename_header_range_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let segment = RetainedSegment::new(identity(), vec![unit(1), unit(2)]).unwrap();
        std::fs::write(
            dir.path().join(segment_file_name(10, 11)),
            segment.to_bytes().unwrap(),
        )
        .unwrap();

        let err = read_units_since(dir.path(), identity(), 0, 10).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("filename range"));
    }

    #[test]
    fn read_units_since_rejects_mixed_database_identity() {
        let dir = tempfile::tempdir().unwrap();
        let wrong_identity = SegmentIdentity::current(*b"fedcba9876543210", 7);
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(wrong_identity, vec![unit(1), unit(2)]).unwrap(),
        )
        .unwrap();

        let err = read_units_since(dir.path(), identity(), 0, 10).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("identity"));
    }

    #[test]
    fn read_units_since_rejects_mixed_primary_generation() {
        let dir = tempfile::tempdir().unwrap();
        let wrong_identity = SegmentIdentity::current(*b"0123456789abcdef", 8);
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(wrong_identity, vec![unit(1), unit(2)]).unwrap(),
        )
        .unwrap();

        let err = read_units_since(dir.path(), identity(), 0, 10).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("identity"));
    }

    #[test]
    fn rejects_zero_database_id_and_primary_generation() {
        assert!(RetainedSegment::new(SegmentIdentity::current([0; 16], 1), vec![unit(1)]).is_err());
        assert!(RetainedSegment::new(
            SegmentIdentity::current(*b"0123456789abcdef", 0),
            vec![unit(1)]
        )
        .is_err());
    }

    #[test]
    fn rejects_impossible_record_count_before_allocating() {
        let segment = RetainedSegment::new(identity(), vec![unit(1)]).unwrap();
        let mut bytes = segment.to_bytes().unwrap();
        bytes[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        rewrite_footer_crc(&mut bytes);

        let err = RetainedSegment::from_bytes(&bytes).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("record count"));
    }

    fn identity_v(catalog_version: u16) -> SegmentIdentity {
        SegmentIdentity::with_catalog_version(*b"0123456789abcdef", 7, catalog_version)
    }

    #[test]
    fn validate_identity_accepts_older_and_rejects_newer_catalog_version() {
        // Legacy (v5) and current (v6) formats both validate on this binary.
        assert!(identity_v(LEGACY_CATALOG_VERSION).validate().is_ok());
        assert!(identity_v(CATALOG_VERSION).validate().is_ok());
        // v0 is invalid.
        assert!(identity_v(0).validate().is_err());
        // A version newer than this binary is rejected, naming both versions.
        let err = identity_v(CATALOG_VERSION + 1).validate().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let msg = err.to_string();
        assert!(msg.contains(&format!("v{}", CATALOG_VERSION + 1)), "{msg}");
        assert!(msg.contains(&format!("max v{CATALOG_VERSION}")), "{msg}");
    }

    #[test]
    fn v5_stamped_segment_reads_on_v6_binary() {
        let dir = tempfile::tempdir().unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity_v(LEGACY_CATALOG_VERSION), vec![unit(1), unit(2)])
                .unwrap(),
        )
        .unwrap();
        // Expected identity stamps this binary's max (v6); lineage matches and
        // the older annotation is accepted.
        let units = read_units_since(dir.path(), identity(), 0, 10).unwrap();
        let lsns: Vec<u64> = units.into_iter().map(|unit| unit.lsn).collect();
        assert_eq!(lsns, vec![1, 2]);
    }

    #[test]
    fn catalog_version_increase_across_chain_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity_v(LEGACY_CATALOG_VERSION), vec![unit(1), unit(2)])
                .unwrap(),
        )
        .unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity_v(CATALOG_VERSION), vec![unit(3), unit(4)]).unwrap(),
        )
        .unwrap();
        // Activation mid-stream (v5 then v6) is a valid, monotonic chain.
        let units = read_units_since(dir.path(), identity(), 0, 10).unwrap();
        let lsns: Vec<u64> = units.into_iter().map(|unit| unit.lsn).collect();
        assert_eq!(lsns, vec![1, 2, 3, 4]);
    }

    #[test]
    fn catalog_version_decrease_across_chain_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity_v(CATALOG_VERSION), vec![unit(1), unit(2)]).unwrap(),
        )
        .unwrap();
        write_segment_atomic(
            dir.path(),
            &RetainedSegment::new(identity_v(LEGACY_CATALOG_VERSION), vec![unit(3), unit(4)])
                .unwrap(),
        )
        .unwrap();
        let err = read_units_since(dir.path(), identity(), 0, 10).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("decreased across chain"), "{err}");
    }

    fn rewrite_footer_crc(bytes: &mut [u8]) {
        let footer_start = bytes.len() - FOOTER_LEN;
        let crc = crc32fast::hash(&bytes[..footer_start]);
        bytes[footer_start + 4..footer_start + 8].copy_from_slice(&crc.to_le_bytes());
    }
}
