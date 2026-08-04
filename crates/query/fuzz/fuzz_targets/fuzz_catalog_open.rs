#![no_main]
use libfuzzer_sys::fuzz_target;
use powdb_storage::catalog::Catalog;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

// `catalog.bin` is the first file the engine reads on every start, and it is
// the file that decides which on-disk features the rest of the open path will
// trust: table schemas, column types, index definitions, entity links, and the
// active catalog version. Everything downstream indexes into structures this
// file describes.
//
// `fuzz_wal_replay` covers the recovery log; nothing covered the catalog
// itself. A truncated write, a half-flushed version bump, or a hostile file in
// a mounted volume all arrive here first. Invariant: `Catalog::open` returns
// `Ok` or a clean `Err`; it must never panic, index out of bounds, or
// pre-allocate from an attacker-controlled length field.
//
// Random bytes die at the CRC/magic check instantly, so the checked-in seeds
// are REAL catalog files at each shipped format version (v5 legacy, v6
// expression-index, v7 entity-link). The fuzzer mutates outward from valid
// structure into the near-valid space, which is where the interesting failures
// are.

static TEMPLATE: OnceLock<PathBuf> = OnceLock::new();
static CASE_ID: AtomicU64 = AtomicU64::new(0);

/// A data dir with a real heap and index alongside the catalog, so a mutated
/// catalog is validated against files that actually exist rather than failing
/// early on a missing heap.
fn template_dir() -> &'static Path {
    TEMPLATE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "powdb_fuzz_catalog_template_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create template dir");
        {
            use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};
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
                        required: false,
                        position: 1,
                    },
                ],
            })
            .expect("create template table");
            cat.create_index_unique("users", "id", true)
                .expect("create template index");
            for i in 0..3i64 {
                cat.insert("users", &vec![Value::Int(i), Value::Str(format!("u{i}"))])
                    .expect("insert template row");
            }
        }
        dir
    })
}

fn stage_case_dir(input: &[u8]) -> PathBuf {
    let case = std::env::temp_dir().join(format!(
        "powdb_fuzz_catalog_case_{}_{}",
        std::process::id(),
        CASE_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&case);
    copy_dir(template_dir(), &case);
    std::fs::write(case.join("catalog.bin"), input).expect("write fuzz catalog");
    case
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create case dir");
    for entry in std::fs::read_dir(src).expect("read template dir").flatten() {
        let name = entry.file_name();
        // The catalog is replaced by the fuzz input; the lock/reader artifacts
        // belong to the template's own process.
        if name == "catalog.bin" || name == "LOCK" || name == "readers" {
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
    // On Ok, dropping the catalog runs the checkpoint/flush path over whatever
    // state the mutated file produced, which must be equally total.
    let _ = Catalog::open(&case);
    let _ = std::fs::remove_dir_all(&case);
});
