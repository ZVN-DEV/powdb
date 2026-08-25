//! Offline administration subcommands: backup, restore, sync, users, sweep.

use super::*;

// ─── Backup / restore ───────────────────────────────────────────────────────

pub(crate) fn run_backup(data_dir: &str, dest: &str, base: Option<&str>) -> i32 {
    // Backup opens the raw catalog, checkpoints it, and truncates the shared
    // wal.log. Done to a directory a live powdb-server owns, that destroys
    // every write the server acknowledged since its last checkpoint (they
    // exist only in the WAL the checkpoint truncates). Take the same writer
    // lock the engine takes so a live owner is refused cleanly; a stale lock
    // from a crashed process is taken over exactly as Engine::open would.
    let _dir_lock = match powdb_storage::dir_lock::DirLock::acquire(Path::new(data_dir)) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("Error: refusing to back up {data_dir}: {e}");
            return 1;
        }
    };
    let mut catalog = match powdb_sync::open_preserving_retained_segments(Path::new(data_dir)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: failed to open data dir {data_dir}: {e}");
            return 1;
        }
    };

    match base {
        // ── Incremental (differential) backup against a full base ──────────
        Some(full_dir) => {
            let base_manifest = match powdb_backup::BackupManifest::read(Path::new(full_dir)) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("Error: failed to read base backup {full_dir}: {e}");
                    return 1;
                }
            };
            let base_lsn = base_manifest.source_lsn;
            match powdb_backup::incremental_backup(&mut catalog, &base_manifest, Path::new(dest)) {
                Ok(m) => {
                    use powdb_backup::ChangedFile;
                    let mut delta_pages: usize = 0;
                    let mut whole_files: usize = 0;
                    for cf in &m.changed {
                        match cf {
                            ChangedFile::Pages { page_indices, .. } => {
                                delta_pages += page_indices.len();
                            }
                            ChangedFile::Whole { .. } => {
                                whole_files += 1;
                            }
                        }
                    }
                    println!(
                        "incremental backup: {} changed files ({whole_files} whole, {} paged), \
                         {delta_pages} delta pages, base lsn {base_lsn} -> lsn {} -> {dest}",
                        m.changed.len(),
                        m.changed.len() - whole_files,
                        m.source_lsn
                    );
                    0
                }
                Err(e) => {
                    eprintln!("Error: incremental backup failed: {e}");
                    1
                }
            }
        }
        // ── Full backup ────────────────────────────────────────────────────
        None => match powdb_backup::full_backup(&mut catalog, Path::new(dest)) {
            Ok(m) => {
                let total_bytes: u64 = m.files.iter().map(|f| f.len).sum();
                println!(
                    "backed up {} files ({total_bytes} bytes) at lsn {} -> {dest}",
                    m.files.len(),
                    m.source_lsn
                );
                0
            }
            Err(e) => {
                eprintln!("Error: backup failed: {e}");
                1
            }
        },
    }
}

pub(crate) fn run_restore(
    backup_dir: &str,
    dest: &str,
    apply: &[String],
    sync_mode: powdb_backup::RestoreSyncMode,
) -> i32 {
    if apply.is_empty() {
        // Full restore.
        return match powdb_backup::restore_with_sync_mode(
            Path::new(backup_dir),
            Path::new(dest),
            sync_mode,
        ) {
            Ok(()) => {
                println!("restored backup {backup_dir} -> {dest}");
                0
            }
            Err(e) => {
                eprintln!("Error: restore failed: {e}");
                1
            }
        };
    }

    // Chain restore: full base + ordered increments.
    let increments: Vec<&Path> = apply.iter().map(|s| Path::new(s.as_str())).collect();
    match powdb_backup::restore_chain_with_sync_mode(
        Path::new(backup_dir),
        &increments,
        Path::new(dest),
        sync_mode,
    ) {
        Ok(()) => {
            println!(
                "restored backup {backup_dir} + {} increment(s) -> {dest}",
                apply.len()
            );
            for inc in apply {
                println!("  applied {inc}");
            }
            0
        }
        Err(e) => {
            eprintln!("Error: chain restore failed: {e}");
            1
        }
    }
}

pub(crate) fn run_sync_enable(data_dir: &str) -> i32 {
    let mut catalog = match powdb_sync::open_preserving_retained_segments(Path::new(data_dir)) {
        Ok(catalog) => catalog,
        Err(e) => {
            eprintln!("Error: failed to open data dir {data_dir}: {e}");
            return 1;
        }
    };
    if let Err(e) = powdb_sync::checkpoint_with_retained_segments(&mut catalog) {
        eprintln!("Error: sync enable failed: {e}");
        return 1;
    }
    match powdb_sync::read_identity_snapshot(Path::new(data_dir)) {
        Ok(Some(snapshot)) => {
            println!(
                "sync enabled: database {} generation {} at lsn {}",
                snapshot.database_id,
                snapshot.primary_generation,
                catalog.max_lsn()
            );
            0
        }
        Ok(None) => {
            eprintln!("Error: sync enable did not create identity metadata");
            1
        }
        Err(e) => {
            eprintln!("Error: failed to read sync identity: {e}");
            1
        }
    }
}

pub(crate) fn run_sync_bootstrap(
    primary_dir: &str,
    backup_dir: &str,
    replica_dir: &str,
    replica_id: &str,
) -> i32 {
    let mut primary = match powdb_sync::open_preserving_retained_segments(Path::new(primary_dir)) {
        Ok(catalog) => catalog,
        Err(e) => {
            eprintln!("Error: failed to open primary data dir {primary_dir}: {e}");
            return 1;
        }
    };
    match powdb_backup::bootstrap_replica_from_full_backup(
        &mut primary,
        Path::new(backup_dir),
        Path::new(replica_dir),
        replica_id,
    ) {
        Ok(summary) => {
            println!(
                "sync replica bootstrapped: {} snapshot lsn {} remote lsn {} retained units {} -> {}",
                summary.replica_id,
                summary.snapshot_lsn,
                summary.remote_lsn,
                summary.retained_units_available,
                replica_dir
            );
            0
        }
        Err(e) => {
            eprintln!("Error: sync bootstrap failed: {e}");
            1
        }
    }
}

pub(crate) fn format_optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "null".to_string(), |n| n.to_string())
}

pub(crate) fn format_sync_repair_action(action: powdb_sync::SyncRepairAction) -> &'static str {
    match action {
        powdb_sync::SyncRepairAction::None => "none",
        powdb_sync::SyncRepairAction::Pull => "pull",
        powdb_sync::SyncRepairAction::AwaitArchive => "awaitArchive",
        powdb_sync::SyncRepairAction::Rebootstrap => "rebootstrap",
    }
}

pub(crate) fn print_replica_sync_status(status: &powdb_sync::ReplicaSyncStatus) {
    println!("replica {}", status.replica_id);
    println!("  active: {}", status.active);
    println!(
        "  lastAppliedLsn: {}",
        format_optional_u64(status.last_applied_lsn)
    );
    println!("  remoteLsn: {}", status.remote_lsn);
    println!(
        "  servableLsn: {}",
        format_optional_u64(status.servable_lsn)
    );
    println!(
        "  unarchivedLsn: {}",
        format_optional_u64(status.unarchived_lsn)
    );
    println!("  lagLsn: {}", format_optional_u64(status.lag_lsn));
    println!("  lagBytes: {}", format_optional_u64(status.lag_bytes));
    println!("  lagMs: {}", format_optional_u64(status.lag_ms));
    println!("  stale: {}", status.stale);
    println!(
        "  repairAction: {}",
        format_sync_repair_action(status.repair_action)
    );
    if let Some(err) = &status.last_sync_error {
        println!("  lastSyncError: {err}");
    }
}

pub(crate) fn run_sync_status(data_dir: &str, replica_id: Option<&str>) -> i32 {
    let catalog = match powdb_sync::open_preserving_retained_segments(Path::new(data_dir)) {
        Ok(catalog) => catalog,
        Err(e) => {
            eprintln!("Error: failed to open data dir {data_dir}: {e}");
            return 1;
        }
    };
    let remote_lsn = catalog.max_lsn();

    let identity = match powdb_sync::read_identity_snapshot_if_exists(Path::new(data_dir)) {
        Ok(Some(identity)) => identity,
        Ok(None) => {
            eprintln!("Error: sync-status requires a sync-enabled data dir; run sync-enable first");
            return 1;
        }
        Err(e) => {
            eprintln!("Error: failed to read sync identity: {e}");
            return 1;
        }
    };

    println!(
        "sync status: database {} generation {} remoteLsn {}",
        identity.database_id, identity.primary_generation, remote_lsn
    );

    if let Some(replica_id) = replica_id {
        match powdb_sync::replica_sync_status(Path::new(data_dir), replica_id, remote_lsn) {
            Ok(status) => {
                print_replica_sync_status(&status);
                0
            }
            Err(e) => {
                eprintln!("Error: failed to read sync status for {replica_id}: {e}");
                1
            }
        }
    } else {
        let mut cursors = match powdb_sync::read_replica_cursors(Path::new(data_dir)) {
            Ok(cursors) => cursors,
            Err(e) => {
                eprintln!("Error: failed to read replica cursors: {e}");
                return 1;
            }
        };
        cursors.sort_by(|a, b| a.replica_id.cmp(&b.replica_id));
        println!("replicas: {}", cursors.len());
        for cursor in cursors {
            match powdb_sync::replica_sync_status(
                Path::new(data_dir),
                &cursor.replica_id,
                remote_lsn,
            ) {
                Ok(status) => print_replica_sync_status(&status),
                Err(e) => {
                    eprintln!(
                        "Error: failed to read sync status for {}: {e}",
                        cursor.replica_id
                    );
                    return 1;
                }
            }
        }
        0
    }
}

// ─── User administration (offline / embedded) ───────────────────────────────

/// Resolve a new-user/new-password value from `--password` or, failing that,
/// the `POWDB_NEW_PASSWORD` env var. Returns `None` when neither is set.
pub(crate) fn resolve_new_password(flag: Option<&str>) -> Option<String> {
    flag.map(|s| s.to_string()).or_else(|| {
        std::env::var("POWDB_NEW_PASSWORD")
            .ok()
            .filter(|s| !s.is_empty())
    })
}

/// Persist the user store to `data_dir`, creating the directory first when it
/// does not exist yet.
///
/// Every other CLI path creates its data directory on the way in (both
/// `Catalog::open` and `Engine::new` call `create_data_dir_secure`), so the
/// user-admin commands used to be the lone exception: `useradd` against a fresh
/// install failed with a bare "failed to save user store: No such file or
/// directory". That is the ordering the docs bless — user admin works before
/// the first server start — and it is the first security step an operator
/// takes. Creating the directory here, immediately before the write, uses the
/// same 0700 mode the engine does, so `auth.json` (0600) never lands in a
/// world-readable directory.
pub(crate) fn save_user_store(store: &powdb_auth::UserStore, data_dir: &str) -> Result<(), i32> {
    let dir = Path::new(data_dir);
    if let Err(e) = powdb_storage::create_data_dir_secure(dir) {
        eprintln!("Error: failed to create data directory {data_dir}: {e}");
        return Err(1);
    }
    if let Err(e) = store.save(dir) {
        eprintln!("Error: failed to save user store to {data_dir}: {e}");
        return Err(1);
    }
    Ok(())
}

pub(crate) fn run_useradd(
    data_dir: &str,
    name: &str,
    role: Option<&str>,
    password: Option<&str>,
) -> i32 {
    let role = role.unwrap_or("readwrite");
    let Some(pw) = resolve_new_password(password) else {
        eprintln!(
            "Error: a password is required \u{2014} pass --password <PW> or set POWDB_NEW_PASSWORD"
        );
        return 2;
    };
    let dir = Path::new(data_dir);
    let mut store = match powdb_auth::UserStore::load(dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: failed to load user store from {data_dir}: {e}");
            return 1;
        }
    };
    if let Err(e) = store.create_user(name, &pw, role) {
        eprintln!("Error: {e}");
        return 1;
    }
    if let Err(code) = save_user_store(&store, data_dir) {
        return code;
    }
    println!("user '{name}' created (role {role})");
    0
}

pub(crate) fn run_userdel(data_dir: &str, name: &str) -> i32 {
    let dir = Path::new(data_dir);
    let mut store = match powdb_auth::UserStore::load(dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: failed to load user store from {data_dir}: {e}");
            return 1;
        }
    };
    if let Err(e) = store.delete_user(name) {
        eprintln!("Error: {e}");
        return 1;
    }
    if let Err(code) = save_user_store(&store, data_dir) {
        return code;
    }
    println!("user '{name}' deleted");
    0
}

pub(crate) fn run_passwd(data_dir: &str, name: &str, password: Option<&str>) -> i32 {
    let Some(pw) = resolve_new_password(password) else {
        eprintln!(
            "Error: a password is required \u{2014} pass --password <PW> or set POWDB_NEW_PASSWORD"
        );
        return 2;
    };
    let dir = Path::new(data_dir);
    let mut store = match powdb_auth::UserStore::load(dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: failed to load user store from {data_dir}: {e}");
            return 1;
        }
    };
    if let Err(e) = store.set_password(name, &pw) {
        eprintln!("Error: {e}");
        return 1;
    }
    if let Err(code) = save_user_store(&store, data_dir) {
        return code;
    }
    println!("password updated for user '{name}'");
    0
}

pub(crate) fn run_users(data_dir: &str) -> i32 {
    let dir = Path::new(data_dir);
    let store = match powdb_auth::UserStore::load(dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: failed to load user store from {data_dir}: {e}");
            return 1;
        }
    };
    let users = store.list_users();
    if users.is_empty() {
        println!("(no users \u{2014} shared-password mode)");
        return 0;
    }
    // Simple table: name + role. Never print password hashes.
    let name_w = users.iter().map(|(n, _)| n.len()).max().unwrap_or(4).max(4);
    let role_w = users.iter().map(|(_, r)| r.len()).max().unwrap_or(4).max(4);
    println!(" {:<name_w$} | {:<role_w$} ", "Name", "Role");
    println!("-{}-+-{}-", "-".repeat(name_w), "-".repeat(role_w));
    for (n, r) in &users {
        println!(" {n:<name_w$} | {r:<role_w$} ");
    }
    println!(
        "({} user{})",
        users.len(),
        if users.len() == 1 { "" } else { "s" }
    );
    0
}

/// Offline overflow reclamation. Opens the catalog at `data_dir` directly
/// (like the other offline admin commands) and mark-and-sweeps orphaned
/// overflow-chain pages for one table, or all tables when `table == "all"`.
pub(crate) fn run_sweep(data_dir: &str, table: &str) -> i32 {
    let dir = Path::new(data_dir);
    // Sweep rewrites heap pages in place; refuse a directory a live process
    // (e.g. a running powdb-server) owns, same as `run_backup`.
    let _dir_lock = match powdb_storage::dir_lock::DirLock::acquire(dir) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("Error: refusing to sweep {data_dir}: {e}");
            return 1;
        }
    };
    let mut cat = match powdb_storage::catalog::Catalog::open(dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: failed to open catalog at {data_dir}: {e}");
            return 1;
        }
    };
    let result = if table == "all" {
        cat.sweep_all()
    } else {
        cat.sweep(table)
    };
    match result {
        Ok(reclaimed) => {
            let scope = if table == "all" {
                "all tables".to_string()
            } else {
                format!("table '{table}'")
            };
            println!("sweep {scope}: reclaimed {reclaimed} overflow page(s)");
            // Checkpoint so the reclamation (and its OverflowFree record) is
            // durable and the WAL is truncated cleanly.
            if let Err(e) = cat.checkpoint() {
                eprintln!("Warning: checkpoint after sweep failed: {e}");
            }
            0
        }
        Err(e) => {
            eprintln!("Error: sweep failed: {e}");
            1
        }
    }
}
