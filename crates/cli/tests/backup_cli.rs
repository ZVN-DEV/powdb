use std::process::Command;

use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_powdb-cli")
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "powdb_clibk_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("failed to run powdb-cli")
}

fn assert_cli_count(data_dir: &std::path::Path, table: &str, expected: usize) {
    let query = format!("count({table})");
    let out = run(&["--data-dir", data_dir.to_str().unwrap(), "-c", &query]);
    assert!(
        out.status.success(),
        "count query failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim(),
        expected.to_string(),
        "expected count {expected} in restored DB, got stdout: {stdout:?}"
    );
}

#[test]
fn cli_backup_then_restore_roundtrip() {
    let data = tmp("data");
    let data_s = data.to_str().unwrap();

    // seed
    assert!(
        run(&["--data-dir", data_s, "-c", "type T { required id: int }"])
            .status
            .success()
    );
    for i in 0..10 {
        let q = format!("insert T {{ id := {i} }}");
        assert!(run(&["--data-dir", data_s, "-c", &q]).status.success());
    }

    // backup
    let backup = tmp("bkp");
    let b = run(&["--data-dir", data_s, "backup", backup.to_str().unwrap()]);
    assert!(
        b.status.success(),
        "backup failed: {}",
        String::from_utf8_lossy(&b.stderr)
    );

    // restore
    let restored = tmp("restored");
    let r = run(&[
        "restore",
        backup.to_str().unwrap(),
        restored.to_str().unwrap(),
    ]);
    assert!(
        r.status.success(),
        "restore failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );

    assert_cli_count(&restored, "T", 10);
}

#[test]
fn cli_restore_sync_identity_modes_default_preserve_and_fork() {
    let data = tmp("syncmodesdata");
    let data_s = data.to_str().unwrap();

    assert!(
        run(&["--data-dir", data_s, "-c", "type T { required id: int }"])
            .status
            .success()
    );
    for i in 0..4 {
        let q = format!("insert T {{ id := {i} }}");
        assert!(run(&["--data-dir", data_s, "-c", &q]).status.success());
    }
    let enabled = run(&["--data-dir", data_s, "sync-enable"]);
    assert!(
        enabled.status.success(),
        "sync-enable failed: {}",
        String::from_utf8_lossy(&enabled.stderr)
    );

    let backup = tmp("syncmodesbackup");
    let b = run(&["--data-dir", data_s, "backup", backup.to_str().unwrap()]);
    assert!(
        b.status.success(),
        "sync backup failed: {}",
        String::from_utf8_lossy(&b.stderr)
    );
    let manifest = powdb_backup::BackupManifest::read(&backup).unwrap();
    let source_snapshot = manifest
        .sync
        .as_ref()
        .expect("sync backup should carry identity")
        .identity
        .clone();
    let source_identity = source_snapshot.identity().unwrap();

    let plain = tmp("syncmodesplain");
    let r = run(&["restore", backup.to_str().unwrap(), plain.to_str().unwrap()]);
    assert!(
        r.status.success(),
        "default restore failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    let err = powdb_sync::read_identity(&plain).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert_cli_count(&plain, "T", 4);

    let explicit_plain = tmp("syncmodesexplicitplain");
    let r = run(&[
        "restore",
        "--sync-strip",
        backup.to_str().unwrap(),
        explicit_plain.to_str().unwrap(),
    ]);
    assert!(
        r.status.success(),
        "explicit strip restore failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    let err = powdb_sync::read_identity(&explicit_plain).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert_cli_count(&explicit_plain, "T", 4);

    let preserved = tmp("syncmodespreserve");
    let r = run(&[
        "restore",
        "--sync-preserve",
        backup.to_str().unwrap(),
        preserved.to_str().unwrap(),
    ]);
    assert!(
        r.status.success(),
        "preserve restore failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(
        powdb_sync::read_identity_snapshot(&preserved)
            .unwrap()
            .unwrap(),
        source_snapshot
    );
    assert_eq!(
        powdb_sync::read_identity(&preserved).unwrap(),
        source_identity
    );
    assert_cli_count(&preserved, "T", 4);

    let forked = tmp("syncmodesfork");
    let r = run(&[
        "restore",
        backup.to_str().unwrap(),
        forked.to_str().unwrap(),
        "--sync-fork",
    ]);
    assert!(
        r.status.success(),
        "fork restore failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    let forked_identity = powdb_sync::read_identity(&forked).unwrap();
    assert_ne!(
        forked_identity, source_identity,
        "fork restore must not reuse the source sync identity"
    );
    assert_eq!(forked_identity.primary_generation, 1);
    assert_cli_count(&forked, "T", 4);

    let conflict_dest = tmp("syncmodesconflict");
    let conflict = run(&[
        "restore",
        "--sync-strip",
        "--sync-preserve",
        backup.to_str().unwrap(),
        conflict_dest.to_str().unwrap(),
    ]);
    assert_eq!(
        conflict.status.code(),
        Some(2),
        "conflicting sync mode flags should fail with usage error; stderr: {}",
        String::from_utf8_lossy(&conflict.stderr)
    );

    let wrong_scope_backup = tmp("wrongscopebackup");
    let wrong_scope = run(&[
        "--data-dir",
        data_s,
        "backup",
        wrong_scope_backup.to_str().unwrap(),
        "--sync-fork",
    ]);
    assert_eq!(
        wrong_scope.status.code(),
        Some(2),
        "restore sync flags outside restore should fail with usage error; stderr: {}",
        String::from_utf8_lossy(&wrong_scope.stderr)
    );

    for i in 4..6 {
        let q = format!("insert T {{ id := {i} }}");
        assert!(run(&["--data-dir", data_s, "-c", &q]).status.success());
    }
    let inc = tmp("syncmodesinc");
    let ib = run(&[
        "--data-dir",
        data_s,
        "backup",
        inc.to_str().unwrap(),
        "--base",
        backup.to_str().unwrap(),
    ]);
    assert!(
        ib.status.success(),
        "incremental sync backup failed: {}",
        String::from_utf8_lossy(&ib.stderr)
    );

    let chain_preserved = tmp("syncmodeschainpreserve");
    let r = run(&[
        "restore",
        "--sync-preserve",
        backup.to_str().unwrap(),
        chain_preserved.to_str().unwrap(),
        "--apply",
        inc.to_str().unwrap(),
    ]);
    assert!(
        r.status.success(),
        "chain preserve restore failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(
        powdb_sync::read_identity(&chain_preserved).unwrap(),
        source_identity
    );
    assert_cli_count(&chain_preserved, "T", 6);

    let chain_forked = tmp("syncmodeschainfork");
    let r = run(&[
        "restore",
        backup.to_str().unwrap(),
        chain_forked.to_str().unwrap(),
        "--apply",
        inc.to_str().unwrap(),
        "--sync-fork",
    ]);
    assert!(
        r.status.success(),
        "chain fork restore failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_ne!(
        powdb_sync::read_identity(&chain_forked).unwrap(),
        source_identity
    );
    assert_cli_count(&chain_forked, "T", 6);
}

#[test]
fn cli_incremental_backup_and_chain_restore() {
    let data = tmp("incdata");
    let data_s = data.to_str().unwrap();

    // seed schema + 20 rows
    assert!(
        run(&["--data-dir", data_s, "-c", "type T { required id: int }"])
            .status
            .success()
    );
    for i in 0..20 {
        let q = format!("insert T {{ id := {i} }}");
        assert!(run(&["--data-dir", data_s, "-c", &q]).status.success());
    }

    // full backup
    let full = tmp("incfull");
    let b = run(&["--data-dir", data_s, "backup", full.to_str().unwrap()]);
    assert!(
        b.status.success(),
        "full backup failed: {}",
        String::from_utf8_lossy(&b.stderr)
    );

    // insert 10 more rows after the full base
    for i in 20..30 {
        let q = format!("insert T {{ id := {i} }}");
        assert!(run(&["--data-dir", data_s, "-c", &q]).status.success());
    }

    // incremental backup against the full base
    let inc = tmp("incinc");
    let ib = run(&[
        "--data-dir",
        data_s,
        "backup",
        inc.to_str().unwrap(),
        "--base",
        full.to_str().unwrap(),
    ]);
    assert!(
        ib.status.success(),
        "incremental backup failed: {}",
        String::from_utf8_lossy(&ib.stderr)
    );

    // chain restore: full base + increment
    let restored = tmp("increstored");
    let r = run(&[
        "restore",
        full.to_str().unwrap(),
        restored.to_str().unwrap(),
        "--apply",
        inc.to_str().unwrap(),
    ]);
    assert!(
        r.status.success(),
        "chain restore failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );

    assert_cli_count(&restored, "T", 30);
}

#[test]
fn cli_sync_bootstrap_restores_snapshot_and_pins_cursor() {
    let data = tmp("syncprimary");
    let data_s = data.to_str().unwrap();

    assert!(
        run(&["--data-dir", data_s, "-c", "type T { required id: int }"])
            .status
            .success()
    );
    for i in 0..3 {
        let q = format!("insert T {{ id := {i} }}");
        assert!(run(&["--data-dir", data_s, "-c", &q]).status.success());
    }

    let enabled = run(&["--data-dir", data_s, "sync-enable"]);
    assert!(
        enabled.status.success(),
        "sync-enable failed: {}",
        String::from_utf8_lossy(&enabled.stderr)
    );
    assert!(
        String::from_utf8_lossy(&enabled.stdout).contains("sync enabled"),
        "unexpected sync-enable output: {}",
        String::from_utf8_lossy(&enabled.stdout)
    );

    let backup = tmp("syncbackup");
    let b = run(&["--data-dir", data_s, "backup", backup.to_str().unwrap()]);
    assert!(
        b.status.success(),
        "sync backup failed: {}",
        String::from_utf8_lossy(&b.stderr)
    );
    let manifest = powdb_backup::BackupManifest::read(&backup).unwrap();
    assert!(manifest.sync.is_some(), "backup should carry sync metadata");

    for i in 3..5 {
        let q = format!("insert T {{ id := {i} }}");
        assert!(run(&["--data-dir", data_s, "-c", &q]).status.success());
    }

    let replica = tmp("syncreplica");
    let boot = run(&[
        "--data-dir",
        data_s,
        "sync-bootstrap",
        backup.to_str().unwrap(),
        replica.to_str().unwrap(),
        "cli-replica",
    ]);
    assert!(
        boot.status.success(),
        "sync-bootstrap failed: {}",
        String::from_utf8_lossy(&boot.stderr)
    );
    assert!(
        String::from_utf8_lossy(&boot.stdout).contains("sync replica bootstrapped"),
        "unexpected sync-bootstrap output: {}",
        String::from_utf8_lossy(&boot.stdout)
    );

    assert_cli_count(&replica, "T", 3);

    let cursors = powdb_sync::read_replica_cursors(&data).unwrap();
    let cursor = cursors
        .iter()
        .find(|cursor| cursor.replica_id == "cli-replica")
        .expect("sync-bootstrap should publish a primary-side cursor");
    assert!(cursor.active);
    assert_eq!(cursor.applied_lsn, manifest.source_lsn);

    let status = run(&["--data-dir", data_s, "sync-status", "cli-replica"]);
    assert!(
        status.status.success(),
        "sync-status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("replica cli-replica"),
        "sync-status should name the replica, got: {stdout:?}"
    );
    assert!(
        stdout.contains("stale: true"),
        "sync-status should report the post-backup primary writes as stale, got: {stdout:?}"
    );
    assert!(
        stdout.contains("repairAction: pull"),
        "sync-status should recommend pull when retained history is available, got: {stdout:?}"
    );
    assert!(
        stdout.contains(&format!("lastAppliedLsn: {}", manifest.source_lsn)),
        "sync-status should report the bootstrap cursor, got: {stdout:?}"
    );

    let all_status = run(&["--data-dir", data_s, "sync-status"]);
    assert!(
        all_status.status.success(),
        "sync-status all failed: {}",
        String::from_utf8_lossy(&all_status.stderr)
    );
    let stdout = String::from_utf8_lossy(&all_status.stdout);
    assert!(
        stdout.contains("replicas: 1") && stdout.contains("replica cli-replica"),
        "sync-status without replica id should list registered cursors, got: {stdout:?}"
    );

    let missing_status = run(&["--data-dir", data_s, "sync-status", "missing-replica"]);
    assert!(
        missing_status.status.success(),
        "sync-status missing cursor should return a rebootstrap status: {}",
        String::from_utf8_lossy(&missing_status.stderr)
    );
    let stdout = String::from_utf8_lossy(&missing_status.stdout);
    assert!(
        stdout.contains("replica missing-replica")
            && stdout.contains("repairAction: rebootstrap")
            && stdout.contains("cursor not found"),
        "sync-status missing cursor should recommend rebootstrap, got: {stdout:?}"
    );
}

#[test]
fn cli_backup_replays_and_archives_pending_sync_wal() {
    let data = tmp("syncdirtyprimary");
    let data_s = data.to_str().unwrap();

    let identity = {
        let mut catalog = powdb_storage::catalog::Catalog::create(&data).unwrap();
        catalog
            .create_table(Schema {
                table_name: "T".into(),
                columns: vec![ColumnDef {
                    name: "id".into(),
                    type_id: TypeId::Int,
                    required: true,
                    position: 0,
                }],
            })
            .unwrap();
        catalog.insert("T", &vec![Value::Int(1)]).unwrap();
        catalog.sync_wal().unwrap();
        powdb_sync::open_or_create_identity(&data).unwrap()
    };

    let backup = tmp("syncdirtybackup");
    let b = run(&["--data-dir", data_s, "backup", backup.to_str().unwrap()]);
    assert!(
        b.status.success(),
        "sync backup with pending WAL failed: {}",
        String::from_utf8_lossy(&b.stderr)
    );

    let manifest = powdb_backup::BackupManifest::read(&backup).unwrap();
    assert!(
        manifest.sync.is_some(),
        "backup should retain sync snapshot metadata"
    );

    let units = powdb_sync::read_units_since(
        &powdb_sync::retained_segments_dir(&data),
        identity.segment_identity(),
        0,
        100,
    )
    .unwrap();
    assert!(
        !units.is_empty(),
        "backup open should archive replayed WAL before truncation"
    );
}

#[test]
fn cli_sync_status_requires_sync_enabled_data_dir() {
    let data = tmp("syncstatusplain");
    let data_s = data.to_str().unwrap();

    assert!(
        run(&["--data-dir", data_s, "-c", "type T { required id: int }"])
            .status
            .success()
    );

    let status = run(&["--data-dir", data_s, "sync-status"]);
    assert_eq!(
        status.status.code(),
        Some(1),
        "sync-status on a plain data dir should fail; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        String::from_utf8_lossy(&status.stderr).contains("run sync-enable first"),
        "sync-status should explain how to enable sync, got: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}
