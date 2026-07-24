//! TASK-03: a backup is a byte-for-byte copy of every row plus the catalog, so
//! it must be created with the same owner-only posture the live data directory
//! gets from `powdb_storage::create_data_dir_secure` (0700 dirs, 0600 files).
//! Under a typical umask, plain `create_dir_all` / `fs::write` would leave
//! `0755` directories and `0644` files, exposing the whole database to every
//! local user on a shared host.

use powdb_storage::catalog::Catalog;
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};

fn tmp(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let uniq = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "powdb_bkperm_{tag}_{}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        uniq
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn schema_t() -> Schema {
    Schema {
        table_name: "T".into(),
        columns: vec![ColumnDef {
            name: "id".into(),
            type_id: TypeId::Int,
            required: true,
            position: 0,
        }],
    }
}

fn seeded_catalog(dir: &std::path::Path) -> Catalog {
    let mut cat = Catalog::create(dir).unwrap();
    cat.create_table(schema_t()).unwrap();
    cat.insert("T", &vec![Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();
    cat
}

#[cfg(unix)]
fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// `dir` itself must be owner-only, and so must every regular file this crate
/// wrote into it.
///
/// Files the storage engine creates for itself on a later `Catalog::open`
/// (notably `wal.log`) are excluded: they are created by the same code paths
/// that run on the live data directory, whose posture is the `0700` directory.
/// Restore is responsible for the bytes it lays down, not for changing the
/// engine's own file-creation mode.
#[cfg(unix)]
fn assert_owner_only_tree(dir: &std::path::Path) {
    const ENGINE_CREATED: [&str; 1] = ["wal.log"];
    assert_eq!(
        mode_of(dir),
        0o700,
        "{} must be owner-only (0700)",
        dir.display()
    );
    let mut checked = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !path.is_file() || ENGINE_CREATED.contains(&name.as_str()) {
            continue;
        }
        assert_eq!(
            mode_of(&path),
            0o600,
            "{} must be owner-only (0600)",
            path.display()
        );
        checked += 1;
    }
    assert!(checked > 0, "no files were checked in {}", dir.display());
}

#[cfg(unix)]
#[test]
fn full_backup_dir_and_files_are_owner_only() {
    let src = tmp("full_src");
    let mut cat = seeded_catalog(&src);
    let dest = tmp("full_dest");
    powdb_backup::full_backup(&mut cat, &dest).unwrap();
    assert_owner_only_tree(&dest);
}

#[cfg(unix)]
#[test]
fn incremental_backup_dir_and_files_are_owner_only() {
    let src = tmp("inc_src");
    let mut cat = seeded_catalog(&src);
    let full_dir = tmp("inc_full");
    let base = powdb_backup::full_backup(&mut cat, &full_dir).unwrap();

    cat.insert("T", &vec![Value::Int(2)]).unwrap();
    cat.sync_wal().unwrap();

    let inc_dir = tmp("inc_delta");
    let inc = powdb_backup::incremental_backup(&mut cat, &base, &inc_dir).unwrap();
    assert!(
        !inc.changed.is_empty(),
        "increment must contain at least one changed file"
    );
    assert_owner_only_tree(&inc_dir);
}

#[cfg(unix)]
#[test]
fn restored_data_dir_and_files_are_owner_only() {
    let src = tmp("res_src");
    let mut cat = seeded_catalog(&src);
    let backup = tmp("res_backup");
    powdb_backup::full_backup(&mut cat, &backup).unwrap();
    drop(cat);

    let restored = tmp("res_restored");
    powdb_backup::restore(&backup, &restored).unwrap();
    assert_owner_only_tree(&restored);
}

#[cfg(unix)]
#[test]
fn restored_chain_data_dir_and_files_are_owner_only() {
    let src = tmp("chain_src");
    let mut cat = seeded_catalog(&src);
    let full_dir = tmp("chain_full");
    let base = powdb_backup::full_backup(&mut cat, &full_dir).unwrap();
    cat.insert("T", &vec![Value::Int(2)]).unwrap();
    cat.sync_wal().unwrap();
    let inc_dir = tmp("chain_inc");
    powdb_backup::incremental_backup(&mut cat, &base, &inc_dir).unwrap();
    drop(cat);

    let restored = tmp("chain_restored");
    powdb_backup::restore_chain(&full_dir, &[inc_dir.as_path()], &restored).unwrap();
    assert_owner_only_tree(&restored);
}
