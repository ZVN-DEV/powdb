#![no_main]
use libfuzzer_sys::fuzz_target;
use powdb_storage::catalog::Catalog;
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

// WAL replay is the crash-recovery path: whatever bytes are on disk in
// `wal.log` after a crash (torn writes, corruption, a hostile disk) are fed
// through `Catalog::open` on the next start. Three past WAL/LSN P0 releases
// make this the highest-value recovery surface. Invariant: open either
// succeeds or returns a clean `Err`; it must never panic, abort, or allocate
// unboundedly from attacker-controlled length fields.
//
// A purely random WAL dies at the CRC check immediately, so the checked-in
// corpus seeds valid WAL files (real records produced by the engine); the
// fuzzer then mutates from valid structure into the interesting near-valid
// space (bad lengths, bit flips inside records, truncated tails).

/// Template data dir with a real table so replayed records have a live
/// schema/heap to land in. Built once; the WAL is left in place by skipping
/// Drop (`mem::forget`), exactly like the storage crate's crash tests, but
/// each fuzz case gets a fresh copy with `wal.log` replaced by the input.
static TEMPLATE: OnceLock<PathBuf> = OnceLock::new();
static CASE_ID: AtomicU64 = AtomicU64::new(0);

fn template_dir() -> &'static Path {
    TEMPLATE.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("powdb_fuzz_wal_template_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create template dir");
        let mut cat = Catalog::create(&dir).expect("create template catalog");
        cat.create_table(Schema {
            table_name: "users".into(),
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
            ],
        })
        .expect("create template table");
        for i in 0..3i64 {
            cat.insert("users", &vec![Value::Int(i), Value::Str(format!("u{i}"))])
                .expect("insert template row");
        }
        cat.sync_wal().expect("sync template WAL");
        // Skip Drop: no checkpoint, no WAL truncation. The dir now looks like
        // a crashed database whose recovery depends entirely on wal.log.
        std::mem::forget(cat);
        dir
    })
}

/// Copy the template into a fresh case dir, minus the WAL (replaced by the
/// fuzz input) and the advisory-lock artifacts.
fn stage_case_dir(input: &[u8]) -> PathBuf {
    let case = std::env::temp_dir().join(format!(
        "powdb_fuzz_wal_case_{}_{}",
        std::process::id(),
        CASE_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&case);
    copy_dir(template_dir(), &case);
    std::fs::write(case.join("wal.log"), input).expect("write fuzz WAL");
    case
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create case dir");
    for entry in std::fs::read_dir(src).expect("read template dir").flatten() {
        let name = entry.file_name();
        if name == "wal.log" || name == "LOCK" || name == "readers" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if from.is_dir() {
            copy_dir(&from, &to);
        } else {
            std::fs::copy(&from, &to).expect("copy template file");
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let case = stage_case_dir(data);
    // Open runs WAL replay over the fuzz input. Ok and Err are both fine;
    // a panic/abort is the only failure. On Ok, Drop runs the checkpoint
    // path over whatever state replay produced, which must be equally total.
    let _ = Catalog::open(&case);
    let _ = std::fs::remove_dir_all(&case);
});
