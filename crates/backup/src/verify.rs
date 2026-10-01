//! Read-only integrity verification for data directories and backup snapshots.
//!
//! The verifier intentionally performs no repair. It opens data directories
//! through the storage read-only path, holds the shared reader lock for the
//! normal lifetime of that open, and reports operator remedies rather than
//! modifying bytes in place.

use crate::manifest::{active_durable_file_names, durable_file_is_optional, BackupManifest};
use crate::restore::{
    ensure_empty_dir, restore_with_sync_mode, validate_backup_file_entry, validate_backup_file_name,
};
use crate::RestoreSyncMode;
use powdb_storage::btree::BTree;
use powdb_storage::catalog::{expression_index_file_name, Catalog, IndexKeySource};
use powdb_storage::data_dir::{CATALOG_FILE, READERS_DIR, WRITER_LOCK_FILE};
use powdb_storage::dir_lock::DirLock;
use powdb_storage::pj1::{pj1_get, pj1_scalar, PathSeg, Pj1Scalar};
use powdb_storage::row::validate_row_format;
use powdb_storage::stored_json_path::StoredJsonPathSegmentV1;
use powdb_storage::types::{Row, RowId, Value};
use powdb_storage::view::ViewRegistry;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Options shared by database, backup, and restore-drill verification.
#[derive(Debug, Clone, Default)]
pub struct VerifyOptions {
    /// Restore the backup into this fresh destination and verify the result.
    pub restore_drill_dir: Option<PathBuf>,
    /// When set, compare the restored drill directory against this source.
    pub compare_source_dir: Option<PathBuf>,
}

/// Overall verifier status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyStatus {
    /// The check completed successfully.
    Ok,
    /// The check detected a problem or could not complete safely.
    Failed,
}

/// One completed check, suitable for both human and machine output.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyCheck {
    /// Stable-ish check name, such as `catalog_open_read_only` or `table:T:rows`.
    pub name: String,
    /// Whether this check succeeded or failed.
    pub status: VerifyStatus,
    /// Human-readable evidence for the check result.
    pub detail: String,
}

/// One verifier finding.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyFinding {
    /// Stable short machine-readable category.
    pub code: String,
    /// Human-readable finding summary.
    pub message: String,
    /// Operator-oriented next step; verification never repairs in place.
    pub remedy: String,
}

/// Stable machine-readable verification report.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    /// Version of this report schema. Increment only for incompatible JSON changes.
    pub schema_version: u16,
    /// The verified target, prefixed by report type.
    pub target: String,
    /// True only when every required check succeeded. Warnings do not clear this flag.
    pub ok: bool,
    /// Ordered check coverage and status rows.
    pub checks: Vec<VerifyCheck>,
    /// Fatal findings that made `ok` false.
    pub errors: Vec<VerifyFinding>,
    /// Non-fatal findings operators may still need to address.
    pub warnings: Vec<VerifyFinding>,
}

impl VerifyReport {
    /// Current stable JSON report schema version.
    pub const SCHEMA_VERSION: u16 = 1;

    fn new(target: impl Into<String>) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            target: target.into(),
            ok: true,
            checks: Vec::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn check_ok(&mut self, name: impl Into<String>, detail: impl Into<String>) {
        self.checks.push(VerifyCheck {
            name: name.into(),
            status: VerifyStatus::Ok,
            detail: detail.into(),
        });
    }

    fn check_failed(&mut self, name: impl Into<String>, detail: impl Into<String>) {
        self.ok = false;
        self.checks.push(VerifyCheck {
            name: name.into(),
            status: VerifyStatus::Failed,
            detail: detail.into(),
        });
    }

    fn error(
        &mut self,
        code: impl Into<String>,
        message: impl Into<String>,
        remedy: impl Into<String>,
    ) {
        self.ok = false;
        self.errors.push(VerifyFinding {
            code: code.into(),
            message: message.into(),
            remedy: remedy.into(),
        });
    }

    fn warning(
        &mut self,
        code: impl Into<String>,
        message: impl Into<String>,
        remedy: impl Into<String>,
    ) {
        self.warnings.push(VerifyFinding {
            code: code.into(),
            message: message.into(),
            remedy: remedy.into(),
        });
    }

    /// Render one stable JSON object.
    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self)
            .expect("VerifyReport serialization is infallible for owned strings")
    }

    /// Render compact operator text.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "verify {}: {}\n",
            self.target,
            if self.ok { "OK" } else { "FAILED" }
        ));
        for check in &self.checks {
            out.push_str(&format!(
                "  [{}] {} — {}\n",
                match check.status {
                    VerifyStatus::Ok => "ok",
                    VerifyStatus::Failed => "failed",
                },
                check.name,
                check.detail
            ));
        }
        for warning in &self.warnings {
            out.push_str(&format!(
                "  [warning:{}] {}\n    remedy: {}\n",
                warning.code, warning.message, warning.remedy
            ));
        }
        for error in &self.errors {
            out.push_str(&format!(
                "  [error:{}] {}\n    remedy: {}\n",
                error.code, error.message, error.remedy
            ));
        }
        out
    }
}

/// Verify an offline data directory without replaying WAL or writing repair
/// bytes.
pub fn verify_database(data_dir: &Path) -> VerifyReport {
    verify_database_inner(data_dir, false)
}

fn verify_database_inner(data_dir: &Path, cleanup_created_reader_dir: bool) -> VerifyReport {
    let mut report = VerifyReport::new(format!("database:{}", data_dir.display()));
    if let Err(error) = powdb_storage::validate_data_dir_read_only(data_dir) {
        report.check_failed("directory", error.to_string());
        report.error(
            "directory_unavailable",
            format!(
                "cannot verify {} as an existing data directory: {error}",
                data_dir.display()
            ),
            "point verify at an existing PowDB data directory; the verifier will not create missing paths",
        );
        return report;
    }
    if let Err(error) = refuse_current_process_writer_lock(data_dir) {
        report.check_failed("writer_lock", error.to_string());
        report.error(
            "live_writer",
            format!("cannot verify while this process owns {}: {error}", data_dir.display()),
            "drop the write engine or run verification from a separate quiescent process after the writer exits",
        );
        return report;
    }
    let readers_dir = data_dir.join(READERS_DIR);
    let readers_dir_existed = readers_dir.exists();
    let reader =
        match DirLock::acquire_reader(data_dir) {
            Ok(lock) => {
                report.check_ok(
                    "reader_lock",
                    "shared administrative reader lock acquired for verifier lifetime",
                );
                lock
            }
            Err(error) => {
                report.check_failed("reader_lock", error.to_string());
                report.error(
                "live_writer",
                format!("cannot verify while a writer owns {}: {error}", data_dir.display()),
                "stop the writer and ensure wal.log is checkpointed before retrying verification",
            );
                return report;
            }
        };

    let catalog = match Catalog::open_read_only(data_dir) {
        Ok(catalog) => {
            report.check_ok(
                "catalog_open_read_only",
                "catalog opened read-only and WAL was clean",
            );
            catalog
        }
        Err(error) => {
            report.check_failed("catalog_open_read_only", error.to_string());
            report.error(
                "catalog_open_failed",
                format!("read-only catalog open failed: {error}"),
                "open the directory with a read-write engine to recover pending WAL, or restore from a known-good backup",
            );
            drop(reader);
            cleanup_reader_dir_if_created(
                data_dir,
                cleanup_created_reader_dir,
                readers_dir_existed,
                &mut report,
            );
            return report;
        }
    };

    verify_catalog_contents(data_dir, &catalog, &mut report);
    drop(catalog);
    drop(reader);
    cleanup_reader_dir_if_created(
        data_dir,
        cleanup_created_reader_dir,
        readers_dir_existed,
        &mut report,
    );
    report
}

fn refuse_current_process_writer_lock(data_dir: &Path) -> io::Result<()> {
    let lock_path = data_dir.join(WRITER_LOCK_FILE);
    match fs::read_to_string(&lock_path) {
        Ok(contents) if contents.trim().parse::<u32>().ok() == Some(std::process::id()) => {
            Err(io::Error::other(format!(
                "{} names the current process as writer",
                lock_path.display()
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn cleanup_reader_dir_if_created(
    data_dir: &Path,
    cleanup_created_reader_dir: bool,
    readers_dir_existed: bool,
    report: &mut VerifyReport,
) {
    if !cleanup_created_reader_dir {
        return;
    }
    let readers_dir = data_dir.join(READERS_DIR);
    if readers_dir_existed || !readers_dir.exists() {
        return;
    }
    match fs::remove_dir(&readers_dir) {
        Ok(()) => report.check_ok(
            "reader_lock_cleanup",
            "removed empty verifier-created administrative readers directory",
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => report.warning(
            "reader_lock_metadata_left",
            format!(
                "verifier-created administrative readers directory remains at {}: {error}",
                readers_dir.display()
            ),
            "this is reader-lock metadata, not table/catalog data; remove it only after all readers exit",
        ),
    }
}

fn verify_catalog_contents(data_dir: &Path, catalog: &Catalog, report: &mut VerifyReport) {
    let tables = catalog.list_tables();
    report.check_ok(
        "catalog_schema",
        format!(
            "{} table(s), catalog format {}",
            tables.len(),
            catalog.active_catalog_version()
        ),
    );

    match verify_views(data_dir, catalog) {
        Ok(ViewCheck {
            total,
            dirty,
            warnings,
        }) => {
            report.check_ok(
                "views",
                format!("{total} registered view(s), {dirty} dirty/needs refresh"),
            );
            for warning in warnings {
                report.warning(
                    "dirty_view",
                    warning,
                    "refresh the materialized view with an operator-approved read-write engine when fresh cached results are required",
                );
            }
        }
        Err(error) => {
            report.check_failed("views", error.to_string());
            report.error(
                "view_metadata_invalid",
                format!("materialized view metadata is invalid: {error}"),
                "restore views.bin and the referenced backing table from backup, or recreate the affected view after copying the directory",
            );
        }
    }

    match verify_links(catalog) {
        Ok(count) => report.check_ok("links", format!("{count} link definition(s) valid")),
        Err(error) => {
            report.check_failed("links", error.to_string());
            report.error(
                "link_metadata_invalid",
                format!("relationship link metadata is invalid: {error}"),
                "restore the catalog from backup or drop/recreate the invalid link after preserving a copy",
            );
        }
    }

    for table_name in tables {
        let Some(table) = catalog.get_table(table_name) else {
            report.check_failed(
                format!("table:{table_name}:open"),
                "catalog listed missing table",
            );
            report.error(
                "table_missing",
                format!("catalog listed table '{table_name}' but it cannot be opened"),
                "restore the table heap/catalog entry from backup",
            );
            continue;
        };

        if let Err(error) = table.heap.verify_integrity() {
            report.check_failed(format!("table:{table_name}:heap_crc"), error.to_string());
            report.error(
                "heap_crc_failed",
                format!(
                    "heap checksum/layout verification failed for table '{table_name}': {error}"
                ),
                "restore this table heap from backup; do not run repair against the source",
            );
            continue;
        }
        report.check_ok(
            format!("table:{table_name}:heap_crc"),
            "heap checksums verified where present; strict page and slot layout verified",
        );

        let rows = match strict_table_rows(table_name, table) {
            Ok(rows) => rows,
            Err(error) => {
                report.check_failed(format!("table:{table_name}:rows"), error.to_string());
                report.error(
                    "row_decode_failed",
                    format!("strict row decode failed for table '{table_name}': {error}"),
                    "restore the damaged heap/overflow pages from backup; scans may hide this class of corruption",
                );
                continue;
            }
        };
        report.check_ok(
            format!("table:{table_name}:rows"),
            format!("{} row(s) decoded through strict RowId get", rows.len()),
        );

        match verify_table_indexes(catalog, table_name, &rows) {
            Ok(count) => report.check_ok(
                format!("table:{table_name}:indexes"),
                format!("{count} index artifact(s) match reconstructed rows"),
            ),
            Err(error) => {
                report.check_failed(format!("table:{table_name}:indexes"), error.to_string());
                report.error(
                    "index_mismatch",
                    format!("index verification failed for table '{table_name}': {error}"),
                    "restore from backup or rebuild the affected index with a read-write engine after copying the directory",
                );
            }
        }
    }
}

struct ViewCheck {
    total: usize,
    dirty: usize,
    warnings: Vec<String>,
}

fn verify_views(data_dir: &Path, catalog: &Catalog) -> io::Result<ViewCheck> {
    let registry = ViewRegistry::open(data_dir)?;
    registry.validate_backing_tables(catalog)?;
    let names = registry.list_views();
    let mut warnings = Vec::new();
    let mut dirty = 0usize;
    for name in names {
        if registry.is_dirty(name) {
            dirty += 1;
            warnings.push(format!(
                "materialized view '{name}' is marked dirty and needs refresh; this is not storage corruption"
            ));
        }
        let def = registry.get(name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("view registry listed '{name}' but could not resolve it"),
            )
        })?;
        for dep in &def.depends_on {
            if catalog.schema(dep).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("view '{}' depends on missing table '{}'", def.name, dep),
                ));
            }
        }
    }
    Ok(ViewCheck {
        total: registry.list_views().len(),
        dirty,
        warnings,
    })
}

fn verify_links(catalog: &Catalog) -> io::Result<usize> {
    let mut count = 0usize;
    for link in catalog.links() {
        count += 1;
        let owner = catalog.schema(&link.owner_type).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "link {}.{} owner table is missing",
                    link.owner_type, link.name
                ),
            )
        })?;
        if owner.column_index(&link.local_key).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "link {}.{} local key '{}' is missing",
                    link.owner_type, link.name, link.local_key
                ),
            ));
        }
        let target = catalog.schema(&link.target_type).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "link {}.{} target table is missing",
                    link.owner_type, link.name
                ),
            )
        })?;
        if target.column_index(&link.target_key).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "link {}.{} target key '{}' is missing",
                    link.owner_type, link.name, link.target_key
                ),
            ));
        }
        if catalog.link_kind(&link.owner_type, &link.name).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "link {}.{} cannot derive cardinality",
                    link.owner_type, link.name
                ),
            ));
        }
    }
    Ok(count)
}

fn strict_table_rows(
    table_name: &str,
    table: &powdb_storage::table::Table,
) -> io::Result<Vec<(RowId, Row)>> {
    let mut rows = Vec::new();
    for item in table.heap.scan() {
        let (rid, raw) = item?;
        validate_row_format(&raw)?;
        let row = table.get(rid)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "table '{table_name}' heap scan yielded RowId {}:{} but strict get returned None",
                    rid.page_id, rid.slot_index
                ),
            )
        })?;
        rows.push((rid, row));
    }
    rows.sort_by_key(|(rid, _)| (rid.page_id, rid.slot_index));
    Ok(rows)
}

fn verify_table_indexes(
    catalog: &Catalog,
    table_name: &str,
    rows: &[(RowId, Row)],
) -> io::Result<usize> {
    let mut count = 0usize;
    let Some(indexes) = catalog.index_metadata(table_name) else {
        return Ok(0);
    };
    for index in indexes {
        match index.source {
            IndexKeySource::Column { column } => {
                count += 1;
                verify_column_index(catalog, table_name, &column, index.unique, rows)?;
            }
            IndexKeySource::Expression {
                index_id,
                json_path,
                canonical_text,
                ..
            } => {
                count += 1;
                verify_expression_index(
                    catalog,
                    table_name,
                    index_id,
                    index.unique,
                    &json_path,
                    &canonical_text,
                    rows,
                )?;
            }
        }
    }
    Ok(count)
}

fn verify_column_index(
    catalog: &Catalog,
    table_name: &str,
    column: &str,
    unique: bool,
    rows: &[(RowId, Row)],
) -> io::Result<()> {
    let table = catalog
        .get_table(table_name)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "table missing"))?;
    let schema = table.schema();
    let col_idx = schema.column_index(column).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("index references missing column '{column}'"),
        )
    })?;
    let mut expected: HashMap<Value, Vec<RowId>> = HashMap::new();
    for (rid, row) in rows {
        let key = row[col_idx].clone();
        if key.is_empty() {
            continue;
        }
        let bucket = expected.entry(key).or_default();
        bucket.push(*rid);
    }
    for rids in expected.values_mut() {
        sort_rids(rids);
        if unique && rids.len() > 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unique index {table_name}.{column} has duplicate logical keys"),
            ));
        }
    }
    let actual_total = expected
        .iter()
        .map(|(key, want)| {
            let mut got = table.index_lookup_all(column, key);
            sort_rids(&mut got);
            if &got != want {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "index {table_name}.{column} key {} maps to {:?}, expected {:?}",
                        key.to_wire_string(),
                        rid_debug(&got),
                        rid_debug(want)
                    ),
                ))
            } else {
                Ok(want.len())
            }
        })
        .try_fold(0usize, |acc, item| item.map(|n| acc + n))?;
    let stats = catalog.index_stats(table_name, column).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("index metadata exists but stats are unavailable for {table_name}.{column}"),
        )
    })?;
    if stats.total_entries != actual_total as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "index {table_name}.{column} stores {} entries, expected {actual_total}",
                stats.total_entries
            ),
        ));
    }
    Ok(())
}

fn verify_expression_index(
    catalog: &Catalog,
    table_name: &str,
    index_id: u64,
    unique: bool,
    path: &powdb_storage::stored_json_path::StoredJsonPathV1,
    canonical_text: &str,
    rows: &[(RowId, Row)],
) -> io::Result<()> {
    let table = catalog
        .get_table(table_name)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "table missing"))?;
    let col_idx = table.schema().column_index(&path.column).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expression index root column '{}' is missing", path.column),
        )
    })?;
    let mut expected = Vec::new();
    let mut unique_seen: HashMap<Value, RowId> = HashMap::new();
    for (rid, row) in rows {
        let key = expression_key_from_path(path, &row[col_idx])?;
        if unique && !key.is_empty() {
            if let Some(existing) = unique_seen.insert(key.clone(), *rid) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "unique expression index {table_name}.{canonical_text} has duplicate key at {}:{} and {}:{}",
                        existing.page_id, existing.slot_index, rid.page_id, rid.slot_index
                    ),
                ));
            }
        }
        expected.push((key, *rid));
    }
    sort_pairs(&mut expected);

    let index_path = catalog
        .data_dir()
        .join(expression_index_file_name(table_name, index_id));
    let actual_tree = BTree::load(&index_path)?;
    let mut actual = actual_tree.ordered_pairs_nulls_last();
    sort_pairs(&mut actual);
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "expression index {table_name}.{canonical_text} differs from reconstructed rows"
            ),
        ));
    }
    Ok(())
}

fn expression_key_from_path(
    meta: &powdb_storage::stored_json_path::StoredJsonPathV1,
    root: &Value,
) -> io::Result<Value> {
    let Value::Json(document) = root else {
        if root.is_empty() {
            return Ok(Value::Empty);
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expression index root value is not JSON",
        ));
    };
    let mut node = document.as_ref();
    for segment in &meta.segments {
        let seg = match segment {
            StoredJsonPathSegmentV1::Key(key) => PathSeg::Key(key),
            StoredJsonPathSegmentV1::Index(index) => PathSeg::Index(*index),
        };
        let Some(next) = pj1_get(node, &seg) else {
            return Ok(Value::Empty);
        };
        node = next;
    }
    match pj1_scalar(node).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid PJ1 while extracting expression index key: {error}"),
        )
    })? {
        Pj1Scalar::Null => Ok(Value::Empty),
        Pj1Scalar::Bool(value) => Ok(Value::Bool(value)),
        Pj1Scalar::Int(value) => Ok(Value::Int(value)),
        Pj1Scalar::Float(value) => Ok(Value::Float(value)),
        Pj1Scalar::Str(value) => Ok(Value::Str(value.to_owned())),
        Pj1Scalar::NonScalar => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "expression index key must be scalar: {}",
                meta.canonical_text()
            ),
        )),
    }
}

/// Validate a full backup manifest and every referenced file without writing
/// to a destination.
pub fn verify_backup(backup_dir: &Path, options: &VerifyOptions) -> VerifyReport {
    let mut report = VerifyReport::new(format!("backup:{}", backup_dir.display()));
    let manifest = match verify_backup_manifest_and_files(backup_dir, &mut report) {
        Ok(manifest) => manifest,
        Err(()) => return report,
    };
    report.check_ok(
        "manifest",
        format!(
            "{} file(s), source_lsn {}, catalog format {}",
            manifest.files.len(),
            manifest.source_lsn,
            manifest.catalog_version
        ),
    );

    if verify_backup_manifest_completeness(backup_dir, &manifest, &mut report).is_err() {
        return report;
    }
    let content = verify_database_inner(backup_dir, true);
    merge_child_report(&mut report, "backup_database", content);

    if let Some(dest) = &options.restore_drill_dir {
        let drill = verify_restore_drill(backup_dir, dest, options.compare_source_dir.as_deref());
        merge_child_report(&mut report, "restore_drill", drill);
    }
    report
}

fn verify_backup_manifest_and_files(
    backup_dir: &Path,
    report: &mut VerifyReport,
) -> Result<BackupManifest, ()> {
    match fs::symlink_metadata(backup_dir.join(BackupManifest::FILE_NAME)) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_file() => {
            report.check_failed("manifest_read", "manifest.json is not a regular file");
            report.error(
                "manifest_file_type_invalid",
                "backup manifest is not a regular file",
                "reject this backup; manifest symlinks and non-file entries are not safe restore inputs",
            );
            return Err(());
        }
        Ok(_) => {}
        Err(error) => {
            report.check_failed("manifest_read", error.to_string());
            report.error(
                "manifest_invalid",
                format!("failed to stat backup manifest: {error}"),
                "use a backup directory with an intact manifest.json generated by PowDB",
            );
            return Err(());
        }
    }
    let manifest = match BackupManifest::read(backup_dir) {
        Ok(manifest) => manifest,
        Err(error) => {
            report.check_failed("manifest_read", error.to_string());
            report.error(
                "manifest_invalid",
                format!("failed to read backup manifest: {error}"),
                "use a backup directory with an intact manifest.json generated by PowDB",
            );
            return Err(());
        }
    };

    let mut seen = BTreeSet::new();
    let mut has_catalog = false;
    for entry in &manifest.files {
        if !seen.insert(entry.name.clone()) {
            report.check_failed("manifest_names", format!("duplicate file {}", entry.name));
            report.error(
                "manifest_duplicate",
                format!("backup manifest lists file '{}' more than once", entry.name),
                "discard this backup or rebuild it from a trusted source; duplicate manifest entries are ambiguous",
            );
            return Err(());
        }
        if entry.name == CATALOG_FILE {
            has_catalog = true;
        }
        if let Err(error) = validate_backup_file_name(&entry.name) {
            report.check_failed("manifest_names", error.to_string());
            report.error(
                "manifest_name_invalid",
                format!("invalid backup file name '{}': {error}", entry.name),
                "discard this backup; PowDB backup manifests may reference only root durable files",
            );
            return Err(());
        }
        if let Err(error) =
            validate_backup_file_entry(backup_dir, &entry.name, entry.len, &entry.blake3_hex)
        {
            report.check_failed("backup_file_validate", format!("{}: {error}", entry.name));
            let message = error.to_string();
            let code = if message.contains("No such file") || message.contains("not found") {
                "backup_file_missing"
            } else if message.contains("not a regular file") {
                "backup_file_type_invalid"
            } else if message.contains("length") {
                "backup_file_length_mismatch"
            } else if message.contains("blake3 mismatch") {
                "backup_file_hash_mismatch"
            } else {
                "backup_file_unreadable"
            };
            report.error(
                code,
                format!("backup entry '{}' failed validation: {error}", entry.name),
                "discard this backup or copy it again from its trusted source",
            );
            return Err(());
        }
    }
    if !has_catalog {
        report.check_failed(
            "manifest_required_files",
            "catalog.bin absent from manifest",
        );
        report.error(
            "manifest_missing_catalog",
            "backup manifest does not include catalog.bin",
            "discard this backup; a full backup without catalog.bin cannot be restored safely",
        );
        return Err(());
    }
    report.check_ok(
        "backup_files",
        format!(
            "{} regular file(s) matched length and blake3",
            manifest.files.len()
        ),
    );
    Ok(manifest)
}

fn verify_backup_manifest_completeness(
    backup_dir: &Path,
    manifest: &BackupManifest,
    report: &mut VerifyReport,
) -> Result<(), ()> {
    let catalog = match Catalog::open_read_only(backup_dir) {
        Ok(catalog) => catalog,
        Err(error) => {
            report.check_failed("manifest_required_files", error.to_string());
            report.error(
                "backup_catalog_open_failed",
                format!("backup catalog cannot be opened read-only: {error}"),
                "discard this backup; a full backup must contain every catalog-referenced durable file",
            );
            return Err(());
        }
    };
    let listed: BTreeSet<&str> = manifest
        .files
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    let mut missing = Vec::new();
    for expected in active_durable_file_names(&catalog) {
        if durable_file_is_optional(&expected) && !backup_dir.join(&expected).exists() {
            continue;
        }
        if !listed.contains(expected.as_str()) {
            missing.push(expected);
        }
    }
    if !missing.is_empty() {
        report.check_failed(
            "manifest_required_files",
            format!(
                "manifest omits required durable file(s): {}",
                missing.join(", ")
            ),
        );
        report.error(
            "manifest_missing_required_file",
            format!(
                "backup manifest omits catalog-referenced durable file(s): {}",
                missing.join(", ")
            ),
            "discard this backup; restore requires all heap/index/catalog-referenced files to be present and hashed",
        );
        return Err(());
    }
    report.check_ok(
        "manifest_required_files",
        "manifest includes every catalog-referenced durable file",
    );
    Ok(())
}

/// Restore a full backup into a fresh directory, verify the restored copy, and
/// optionally compare it to a source data directory.
pub fn verify_restore_drill(
    backup_dir: &Path,
    dest: &Path,
    compare_source: Option<&Path>,
) -> VerifyReport {
    let mut report = VerifyReport::new(format!("restore-drill:{}", dest.display()));
    match fs::symlink_metadata(dest) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() => {
            report.check_failed("restore_destination", "destination is not a real directory");
            report.error(
                "restore_destination_invalid",
                format!(
                    "restore drill destination {} is not a real directory",
                    dest.display()
                ),
                "choose a writable fresh real directory; verifier never follows destination symlinks",
            );
            return report;
        }
        Ok(_)
            if dest
                .read_dir()
                .map(|mut it| it.next().is_some())
                .unwrap_or(true) =>
        {
            report.check_failed("restore_destination", "destination is not empty");
            report.error(
                "restore_destination_not_empty",
                format!("restore drill destination {} is not empty", dest.display()),
                "choose a fresh empty directory; verifier never overwrites existing data",
            );
            return report;
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            report.check_failed("restore_destination", error.to_string());
            report.error(
                "restore_destination_invalid",
                format!("restore drill destination could not be inspected: {error}"),
                "choose a writable fresh directory",
            );
            return report;
        }
    }
    if let Err(error) = ensure_empty_dir(dest) {
        report.check_failed("restore_destination", error.to_string());
        report.error(
            "restore_destination_invalid",
            format!("restore drill destination could not be prepared: {error}"),
            "choose a writable fresh directory",
        );
        return report;
    }
    match restore_with_sync_mode(backup_dir, dest, RestoreSyncMode::StripSyncIdentity) {
        Ok(()) => report.check_ok("restore", "backup restored into fresh drill directory"),
        Err(error) => {
            report.check_failed("restore", error.to_string());
            report.error(
                "restore_failed",
                format!("restore drill failed: {error}"),
                "use another backup or inspect the manifest/file errors above",
            );
            return report;
        }
    }
    let restored = verify_database(dest);
    merge_child_report(&mut report, "restored_database", restored);
    if let Some(source) = compare_source {
        let compare = compare_database_dirs(source, dest);
        merge_child_report(&mut report, "source_compare", compare);
    }
    report
}

/// Compare logical rows and index metadata between two verified offline data
/// directories.
pub fn compare_database_dirs(source: &Path, other: &Path) -> VerifyReport {
    let mut report = VerifyReport::new(format!("compare:{}:{}", source.display(), other.display()));
    let left = match open_verified_catalog(source, &mut report, "source") {
        Some(open) => open,
        None => return report,
    };
    let right = match open_verified_catalog(other, &mut report, "other") {
        Some(open) => open,
        None => return report,
    };
    let left_tables: BTreeSet<String> = left
        .catalog
        .list_tables()
        .into_iter()
        .map(str::to_string)
        .collect();
    let right_tables: BTreeSet<String> = right
        .catalog
        .list_tables()
        .into_iter()
        .map(str::to_string)
        .collect();
    if left_tables != right_tables {
        report.check_failed("compare_tables", "table sets differ");
        report.error(
            "compare_table_mismatch",
            format!("source tables {:?} differ from restored tables {:?}", left_tables, right_tables),
            "restore from the matching backup or verify that the source directory matches the backup point",
        );
        return report;
    }
    for table_name in left_tables {
        let left_rows = logical_table_digest(left.catalog.get_table(&table_name).unwrap());
        let right_rows = logical_table_digest(right.catalog.get_table(&table_name).unwrap());
        match (left_rows, right_rows) {
            (Ok(left_rows), Ok(right_rows)) if left_rows == right_rows => {
                report.check_ok(format!("compare:{table_name}:rows"), "logical rows match");
            }
            (Ok(_), Ok(_)) => {
                report.check_failed(format!("compare:{table_name}:rows"), "logical rows differ");
                report.error(
                    "compare_rows_mismatch",
                    format!("logical rows differ for table '{table_name}'"),
                    "ensure the source directory was quiescent at the backup LSN before trusting this drill",
                );
            }
            (Err(error), _) | (_, Err(error)) => {
                report.check_failed(format!("compare:{table_name}:rows"), error.to_string());
                report.error(
                    "compare_rows_failed",
                    format!("could not compare table '{table_name}': {error}"),
                    "run database verification on both directories and repair by restore, not in place",
                );
            }
        }
        if left.catalog.index_metadata(&table_name) != right.catalog.index_metadata(&table_name) {
            report.check_failed(
                format!("compare:{table_name}:indexes"),
                "index metadata differs",
            );
            report.error(
                "compare_index_metadata_mismatch",
                format!("index metadata differs for table '{table_name}'"),
                "verify the backup was taken from the intended source and restore chain",
            );
        } else {
            report.check_ok(
                format!("compare:{table_name}:indexes"),
                "index metadata matches",
            );
        }
    }
    report
}

struct OpenVerified {
    catalog: Catalog,
    #[allow(dead_code)]
    reader: DirLock,
}

fn open_verified_catalog(
    path: &Path,
    report: &mut VerifyReport,
    label: &str,
) -> Option<OpenVerified> {
    let reader = match DirLock::acquire_reader(path) {
        Ok(reader) => reader,
        Err(error) => {
            report.check_failed(format!("{label}:reader_lock"), error.to_string());
            report.error(
                "compare_open_failed",
                format!("could not take reader lock for {}: {error}", path.display()),
                "stop live writers before comparing directories",
            );
            return None;
        }
    };
    let catalog = match Catalog::open_read_only(path) {
        Ok(catalog) => catalog,
        Err(error) => {
            report.check_failed(format!("{label}:open"), error.to_string());
            report.error(
                "compare_open_failed",
                format!("could not open {} read-only: {error}", path.display()),
                "run verification on this directory and recover pending WAL if necessary",
            );
            return None;
        }
    };
    Some(OpenVerified { catalog, reader })
}

fn logical_table_digest(table: &powdb_storage::table::Table) -> io::Result<Vec<String>> {
    let mut rows = Vec::new();
    for (_, row) in strict_table_rows(table.schema().table_name.as_str(), table)? {
        rows.push(logical_row_hash(&row));
    }
    rows.sort();
    Ok(rows)
}

fn logical_row_hash(row: &Row) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(row.len() as u64).to_le_bytes());
    for value in row {
        hash_value(&mut hasher, value);
    }
    hasher.finalize().to_hex().to_string()
}

fn hash_value(hasher: &mut blake3::Hasher, value: &Value) {
    hasher.update(&[value.type_id() as u8]);
    match value {
        Value::Int(value) | Value::DateTime(value) => {
            hasher.update(&value.to_le_bytes());
        }
        Value::Float(value) => {
            hasher.update(&value.to_bits().to_le_bytes());
        }
        Value::Bool(value) => {
            hasher.update(&[*value as u8]);
        }
        Value::Str(value) => hash_len_bytes(hasher, value.as_bytes()),
        Value::Uuid(value) => {
            hasher.update(value);
        }
        Value::Bytes(value) => hash_len_bytes(hasher, value),
        Value::Json(value) => hash_len_bytes(hasher, value),
        Value::Empty => {}
    };
}

fn hash_len_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn merge_child_report(parent: &mut VerifyReport, prefix: &str, child: VerifyReport) {
    for mut check in child.checks {
        check.name = format!("{prefix}:{}", check.name);
        if check.status == VerifyStatus::Failed {
            parent.ok = false;
        }
        parent.checks.push(check);
    }
    if !child.ok {
        parent.ok = false;
    }
    parent.errors.extend(child.errors);
    parent.warnings.extend(child.warnings);
}

fn sort_rids(rids: &mut [RowId]) {
    rids.sort_by_key(|rid| (rid.page_id, rid.slot_index));
}

fn sort_pairs(pairs: &mut [(Value, RowId)]) {
    pairs.sort_by(|(left_key, left_rid), (right_key, right_rid)| {
        left_key.cmp(right_key).then_with(|| {
            (left_rid.page_id, left_rid.slot_index).cmp(&(right_rid.page_id, right_rid.slot_index))
        })
    });
}

fn rid_debug(rids: &[RowId]) -> Vec<String> {
    rids.iter()
        .map(|rid| format!("{}:{}", rid.page_id, rid.slot_index))
        .collect()
}
