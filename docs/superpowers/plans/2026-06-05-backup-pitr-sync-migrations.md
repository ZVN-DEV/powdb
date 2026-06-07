# Backup / PITR / Cloud-Sync / Migrations Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give server/cloud-hosted PowDB durable data-protection — full + incremental backup, restore, point-in-time restore (PITR), cloud disaster-recovery sync, and a schema-migration framework — without regressing the hot read/write path.

**Architecture:** Three substrates, built in dependency order. **S1 = snapshot + page-LSN diff** (reuses the page LSNs PowDB already stamps) powers full backup, incremental backup, cloud DR, and coarse PITR. **S2 = retained WAL archive** (the only genuinely new substrate) adds fine-grained PITR. **S3 = migration manager** is orthogonal, built on existing `alter`/`type` DDL. Restore always finishes through `Catalog::open`, which already sets `next_lsn = max_page_lsn + 1` — so every restore path inherits the v0.4.3 LSN-reset P0 fix for free.

**Tech Stack:** Rust (workspace crates), `powdb-storage` (`Catalog`, `HeapFile`, page LSNs, `checkpoint()`), `blake3` (file integrity), `serde`/`serde_json` (manifests), criterion regression gate (`powdb-bench`).

**Design source:** `docs/design/2026-06-05-backup-pitr-sync-migrations-plan.md` (verified against v0.4.5 source).

---

## CONTINUOUS-VERIFICATION PROTOCOL (read first — applies to EVERY task)

This is the standing guard that the work does not slow or break anything. It is **not optional** and **not a one-time check**.

**Baseline capture (run ONCE, before Task 1):**

- [ ] Confirm the suite is green and record the count:
  - Run: `cargo test --workspace 2>&1 | grep "test result:"`
  - Expected: all `ok`, **0 failed** (current baseline: 633 passed).
- [ ] Capture the performance baseline (the regression gate's reference):
  - Run: `./scripts/update-bench-baseline.sh`
  - This records criterion baselines so later runs can detect regressions. Commit the baseline files it writes.

**After EVERY implementation step that changes Rust code** (this is the inner loop, do not skip):

1. The task's own test passes (TDD — see each task).
2. Run: `cargo test --workspace 2>&1 | grep -E "test result:|FAILED"` → **0 failed**, count never drops below baseline.
3. Run: `cargo clippy --workspace --all-targets -- -D warnings` → clean.
4. Run: `cargo fmt --check` → clean (run `cargo fmt` if not).
5. Commit (small, frequent).

**At every PHASE boundary (the anti-slowdown gate):**

- [ ] Run the regression gate: `cargo run --release -p powdb-bench --bin compare`
  - Expected: **PASS** — no workload regressed beyond its threshold.
  - Phases 1–4 add only admin/backup-time code paths (NOT the hot read/write path), so the gate should pass trivially — but running it *proves* we didn't accidentally touch a hot path.
  - **Phase 5 is the exception**: WAL archiving runs on the commit path. Its gate is a hard requirement — if `insert`/`update` throughput regresses, the phase is not done. See Phase 5.
- [ ] Run the durability suite explicitly: `cargo test -p powdb-query --test durability` → all green. Backup/restore must never reintroduce a data-loss path.

**If any gate fails:** stop, use superpowers:systematic-debugging, fix before proceeding. Never advance a phase on a red gate.

---

## SCOPE DECOMPOSITION (5 phases, each independently shippable)

| Phase | Feature | Substrate | Hot-path risk | Size | Ships |
|---|---|---|---|---|---|
| **P1** | Full snapshot backup **+ restore** | S1 | None | M | A working backup/restore CLI |
| **P2** | Incremental backup (page-LSN diff) + coarse PITR | S1 | None | M | Cheap incrementals + restore-to-snapshot |
| **P3** | Cloud DR sync (push base+increments to object store) | S1 | None | M | Off-site disaster recovery |
| **P4** | Schema migration framework | S3 | None | M | `migrate` / `rollback` / `status` |
| **P5** | Fine-grained PITR (retained WAL archive) | S2 | **Yes (commit path)** | L | Restore to an exact LSN/time |

**This document fully details Phase 1** (the foundation everything else builds on). Phases 2–5 are specified at task level with files, key tests, and exit criteria; each is expanded to bite-sized TDD steps at execution time (its concrete interfaces depend on P1's outcome — pre-writing every micro-step would be speculative). Per the writing-plans scope check, **treat each phase as its own plan**: do not start P2 until P1 ships green.

---

## File Structure (Phase 1)

- `crates/backup/` — NEW crate `powdb-backup`. Owns all backup/restore logic (grows in P2/P3). Depends on `powdb-storage`.
  - `crates/backup/Cargo.toml`
  - `crates/backup/src/lib.rs` — public API (`full_backup`, `restore`), re-exports.
  - `crates/backup/src/manifest.rs` — `BackupManifest`, `FileEntry`, integrity.
  - `crates/backup/tests/backup_roundtrip.rs` — integration tests.
- `crates/storage/src/catalog.rs` — MODIFY: add two small accessors (`data_dir()`, `max_lsn()`).
- `crates/cli/src/main.rs` — MODIFY: add `backup` / `restore` subcommands.
- `Cargo.toml` (workspace root) — MODIFY: add `crates/backup` to members.
- `docs/backup-and-restore.md` — NEW: operator docs.

---

## Phase 1: Full Snapshot Backup + Restore

### Task 1: Catalog accessors (`data_dir`, `max_lsn`)

**Files:**
- Modify: `crates/storage/src/catalog.rs`
- Test: `crates/storage/src/catalog.rs` (inline `#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

In the existing `#[cfg(test)] mod tests` in `catalog.rs`:

```rust
#[test]
fn data_dir_and_max_lsn_accessors() {
    let dir = std::env::temp_dir().join(format!("powdb_acc_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cat = Catalog::create(&dir).unwrap();
    assert_eq!(cat.data_dir(), dir.as_path());
    assert_eq!(cat.max_lsn(), 0, "fresh catalog has no page LSNs");

    cat.create_table(Schema::new(
        "T",
        vec![Column::new("id", ColumnType::Int, true)],
    ))
    .unwrap();
    cat.insert("T", &[Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();
    assert!(cat.max_lsn() > 0, "an inserted row must stamp a page LSN");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p powdb-storage data_dir_and_max_lsn_accessors`
Expected: FAIL — `no method named data_dir`/`max_lsn`.

- [ ] **Step 3: Write minimal implementation**

Add to `impl Catalog` in `catalog.rs`:

```rust
/// The directory this catalog's files live in.
pub fn data_dir(&self) -> &Path {
    &self.data_dir
}

/// Highest page LSN across all tables (0 if nothing has been written).
/// This is the durability high-water mark — the LSN a backup taken now
/// corresponds to, and the value `Catalog::open` uses to restore
/// `next_lsn` after a reopen/restore.
pub fn max_lsn(&self) -> u64 {
    self.tables
        .iter()
        .map(|t| t.heap.max_page_lsn())
        .max()
        .unwrap_or(0)
}
```

(Adjust `create_table`/`insert` call shapes in the test to the real `Catalog` API if they differ — grep `fn create_table`, `fn insert` in `catalog.rs` first.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p powdb-storage data_dir_and_max_lsn_accessors`
Expected: PASS.

- [ ] **Step 5: Verification protocol + commit**

Run the inner-loop checks (full suite / clippy / fmt). Then:

```bash
git add crates/storage/src/catalog.rs
git commit -m "feat(storage): add Catalog::data_dir() and max_lsn() accessors"
```

---

### Task 2: `powdb-backup` crate + `BackupManifest`

**Files:**
- Create: `crates/backup/Cargo.toml`
- Create: `crates/backup/src/lib.rs`
- Create: `crates/backup/src/manifest.rs`
- Modify: `Cargo.toml` (workspace members)

- [ ] **Step 1: Create the crate skeleton**

`crates/backup/Cargo.toml`:

```toml
[package]
name = "powdb-backup"
version = "0.4.5"
edition = "2021"
rust-version = "1.93"
license = "MIT"
description = "Backup, restore, and point-in-time recovery for PowDB."

[dependencies]
powdb-storage = { version = "0.4.5", path = "../storage" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
blake3 = "1"
```

Add `"crates/backup"` to `members` in the workspace root `Cargo.toml`.

`crates/backup/src/lib.rs`:

```rust
//! Backup / restore / PITR for PowDB. See
//! docs/design/2026-06-05-backup-pitr-sync-migrations-plan.md.
pub mod manifest;
pub use manifest::{BackupManifest, FileEntry};
```

- [ ] **Step 2: Write the failing manifest test**

`crates/backup/src/manifest.rs` (test first, at bottom):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest_round_trips_and_rejects_bad_version() {
        let m = BackupManifest {
            format_version: BackupManifest::FORMAT_VERSION,
            created_unix_secs: 1_700_000_000,
            source_lsn: 42,
            files: vec![FileEntry { name: "catalog.bin".into(), len: 10, blake3_hex: "ab".into() }],
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: BackupManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source_lsn, 42);
        assert_eq!(back.files.len(), 1);

        let mut bad = m.clone();
        bad.format_version = 999;
        assert!(bad.validate_version().is_err(), "unknown format must be rejected");
    }
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p powdb-backup manifest_round_trips`
Expected: FAIL — types not defined.

- [ ] **Step 4: Implement the manifest**

At the top of `crates/backup/src/manifest.rs`:

```rust
use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub len: u64,
    pub blake3_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub format_version: u32,
    pub created_unix_secs: u64,
    /// The page-LSN high-water mark this backup is consistent at.
    pub source_lsn: u64,
    pub files: Vec<FileEntry>,
}

impl BackupManifest {
    pub const FORMAT_VERSION: u32 = 1;
    pub const FILE_NAME: &'static str = "manifest.json";

    pub fn validate_version(&self) -> io::Result<()> {
        if self.format_version != Self::FORMAT_VERSION {
            return Err(io::Error::other(format!(
                "unsupported backup format {} (this build understands {})",
                self.format_version,
                Self::FORMAT_VERSION
            )));
        }
        Ok(())
    }

    pub fn write(&self, dir: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        std::fs::write(dir.join(Self::FILE_NAME), json)
    }

    pub fn read(dir: &Path) -> io::Result<Self> {
        let bytes = std::fs::read(dir.join(Self::FILE_NAME))?;
        let m: BackupManifest = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        m.validate_version()?;
        Ok(m)
    }
}
```

- [ ] **Step 5: Run to verify it passes, then verification + commit**

Run: `cargo test -p powdb-backup`
Expected: PASS. Then inner-loop checks, then:

```bash
git add crates/backup Cargo.toml
git commit -m "feat(backup): add powdb-backup crate + BackupManifest"
```

---

### Task 3: `full_backup(catalog, dest)`

**Files:**
- Create: `crates/backup/src/full.rs`
- Modify: `crates/backup/src/lib.rs` (add `mod full; pub use full::full_backup;`)
- Test: `crates/backup/tests/backup_roundtrip.rs`

- [ ] **Step 1: Write the failing test**

`crates/backup/tests/backup_roundtrip.rs`:

```rust
use powdb_storage::catalog::Catalog;
use powdb_storage::types::{Column, ColumnType, Schema, Value};

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "powdb_bk_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

#[test]
fn full_backup_copies_files_and_records_lsn() {
    let src = tmp("src");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(Schema::new("T", vec![Column::new("id", ColumnType::Int, true)])).unwrap();
    cat.insert("T", &[Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();

    let dest = tmp("dest");
    let manifest = powdb_backup::full_backup(&mut cat, &dest).unwrap();

    assert!(manifest.source_lsn > 0);
    assert!(manifest.files.iter().any(|f| f.name == "catalog.bin"));
    assert!(manifest.files.iter().any(|f| f.name == "T.heap"));
    assert!(!manifest.files.iter().any(|f| f.name == "wal.log"), "WAL is truncated by checkpoint; not in a snapshot");
    assert!(dest.join("T.heap").exists());
    assert!(dest.join(powdb_backup::BackupManifest::FILE_NAME).exists());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p powdb-backup --test backup_roundtrip full_backup_copies_files`
Expected: FAIL — `full_backup` not found.

- [ ] **Step 3: Implement `full_backup`**

`crates/backup/src/full.rs`:

```rust
use crate::manifest::{BackupManifest, FileEntry};
use powdb_storage::catalog::Catalog;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Take a consistent full snapshot of `catalog`'s data dir into `dest`.
///
/// Consistency model: `checkpoint()` flushes every dirty heap page + index
/// and truncates the WAL, producing a clean-shutdown image. We then copy the
/// durable files. The brief write-quiesce is the duration of the checkpoint
/// (ms), held by the caller's `&mut` borrow — no stop-the-world for the copy.
pub fn full_backup(catalog: &mut Catalog, dest: &Path) -> io::Result<BackupManifest> {
    catalog.checkpoint()?;
    let source_lsn = catalog.max_lsn();
    let src = catalog.data_dir().to_path_buf();
    std::fs::create_dir_all(dest)?;

    let mut files = Vec::new();
    for entry in std::fs::read_dir(&src)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        // Snapshot the durable state only: catalog + heaps + indexes.
        // wal.log was just truncated; manifest.json is ours.
        let is_durable = name == "catalog.bin"
            || name.ends_with(".heap")
            || name.ends_with(".idx");
        if !is_durable {
            continue;
        }
        let bytes = std::fs::read(entry.path())?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        std::fs::write(dest.join(&name), &bytes)?;
        files.push(FileEntry { name, len: bytes.len() as u64, blake3_hex: hash });
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));

    let manifest = BackupManifest {
        format_version: BackupManifest::FORMAT_VERSION,
        created_unix_secs: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
        source_lsn,
        files,
    };
    manifest.write(dest)?;
    Ok(manifest)
}
```

Wire it in `lib.rs`: `mod full; pub use full::full_backup;`

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p powdb-backup --test backup_roundtrip full_backup_copies_files`
Expected: PASS.

- [ ] **Step 5: Verification + commit**

Inner-loop checks, then:

```bash
git add crates/backup
git commit -m "feat(backup): full_backup — checkpoint-then-copy snapshot with integrity manifest"
```

---

### Task 4: `restore(backup_dir, dest_data_dir)` with integrity check

**Files:**
- Create: `crates/backup/src/restore.rs`
- Modify: `crates/backup/src/lib.rs` (`mod restore; pub use restore::restore;`)
- Test: `crates/backup/tests/backup_roundtrip.rs`

- [ ] **Step 1: Write the failing test**

Append to `backup_roundtrip.rs`:

```rust
#[test]
fn restore_rebuilds_a_usable_database() {
    let src = tmp("rsrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(Schema::new("T", vec![Column::new("id", ColumnType::Int, true)])).unwrap();
    for i in 0..50 { cat.insert("T", &[Value::Int(i)]).unwrap(); }
    cat.sync_wal().unwrap();
    let backup = tmp("bkp");
    powdb_backup::full_backup(&mut cat, &backup).unwrap();
    drop(cat);

    let restored = tmp("restored");
    powdb_backup::restore(&backup, &restored).unwrap();

    // Reopen the restored dir and confirm the data is all there.
    let cat2 = Catalog::open(&restored).unwrap();
    assert_eq!(cat2.table_row_count("T"), 50);
}

#[test]
fn restore_rejects_a_tampered_backup() {
    let src = tmp("tsrc");
    let mut cat = Catalog::create(&src).unwrap();
    cat.create_table(Schema::new("T", vec![Column::new("id", ColumnType::Int, true)])).unwrap();
    cat.insert("T", &[Value::Int(1)]).unwrap();
    cat.sync_wal().unwrap();
    let backup = tmp("tbkp");
    powdb_backup::full_backup(&mut cat, &backup).unwrap();

    // Corrupt a backed-up file.
    std::fs::write(backup.join("T.heap"), b"corrupted").unwrap();
    let restored = tmp("trestored");
    let err = powdb_backup::restore(&backup, &restored).unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("integrity") || format!("{err}").to_lowercase().contains("hash"));
}
```

(If `Catalog` has no `table_row_count`, add a tiny accessor in Task 1's spirit, or assert via a scan helper — grep `fn table_row_count`/`fn scan` first.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p powdb-backup --test backup_roundtrip restore_`
Expected: FAIL — `restore` not found.

- [ ] **Step 3: Implement `restore`**

`crates/backup/src/restore.rs`:

```rust
use crate::manifest::BackupManifest;
use powdb_storage::catalog::Catalog;
use std::io;
use std::path::Path;

/// Rebuild a data dir from a full backup. Verifies every file's blake3 against
/// the manifest before writing, then opens the result through `Catalog::open`
/// (which sets `next_lsn = max_page_lsn + 1` — the v0.4.3 LSN-reset fix) to
/// validate the restored database actually opens.
pub fn restore(backup_dir: &Path, dest_data_dir: &Path) -> io::Result<()> {
    let manifest = BackupManifest::read(backup_dir)?;
    std::fs::create_dir_all(dest_data_dir)?;
    for f in &manifest.files {
        let bytes = std::fs::read(backup_dir.join(&f.name))?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        if hash != f.blake3_hex {
            return Err(io::Error::other(format!(
                "integrity check failed for {}: hash mismatch (backup is corrupt)",
                f.name
            )));
        }
        std::fs::write(dest_data_dir.join(&f.name), &bytes)?;
    }
    // Validate: opening must succeed and (critically) reset next_lsn correctly.
    let cat = Catalog::open(dest_data_dir)?;
    drop(cat);
    Ok(())
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p powdb-backup --test backup_roundtrip restore_`
Expected: PASS (both restore tests).

- [ ] **Step 5: Verification + commit**

Inner-loop checks, then commit `feat(backup): restore with blake3 integrity verification`.

---

### Task 5: Restore LSN-invariant durability test (the v0.4.3 P0 guard)

**Files:**
- Test: `crates/backup/tests/restore_durability.rs`

This task is **test-only** — it proves the most important property: a restored DB accepts new writes that then survive a crash (i.e. `next_lsn` was set correctly post-restore, no LSN reset). This is the exact bug class that yanked v0.4.1–v0.4.3.

- [ ] **Step 1: Write the test**

`crates/backup/tests/restore_durability.rs`:

```rust
use powdb_query::executor::Engine;
use powdb_storage::types::Value;

fn tmp(t: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("powdb_rd_{t}_{}_{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    let _ = std::fs::remove_dir_all(&p); p
}
fn count(e: &mut Engine) -> i64 {
    match e.execute_powql("count(T)").unwrap() {
        powdb_query::result::QueryResult::Scalar(Value::Int(n)) => n,
        other => panic!("{other:?}"),
    }
}

#[test]
fn writes_after_restore_survive_a_crash() {
    // 1. Build + back up.
    let src = tmp("src");
    {
        let mut e = Engine::new(&src).unwrap();
        e.execute_powql("type T { required id: int }").unwrap();
        for i in 0..100 { e.execute_powql(&format!("insert T {{ id := {i} }}")).unwrap(); }
        // backup needs &mut Catalog; expose via Engine::backup (Task 6) or open a Catalog here.
        powdb_backup::full_backup(e.catalog_mut(), &tmp("ignored")).unwrap_or_default();
    }
    // (Adjust to whatever backup entry point Task 6 exposes on Engine.)

    let backup = tmp("bkp");
    {
        let mut cat = powdb_storage::catalog::Catalog::open(&src).unwrap();
        powdb_backup::full_backup(&mut cat, &backup).unwrap();
    }

    // 2. Restore to a fresh dir.
    let restored = tmp("restored");
    powdb_backup::restore(&backup, &restored).unwrap();

    // 3. Open restored, write MORE, then HARD crash (no checkpoint).
    {
        let mut e = Engine::new(&restored).unwrap();
        assert_eq!(count(&mut e), 100, "restored rows present");
        for i in 100..150 { e.execute_powql(&format!("insert T {{ id := {i} }}")).unwrap(); }
        std::mem::forget(e); // crash: WAL holds the 50 new rows, no checkpoint
    }
    // 4. Reopen — the post-restore writes MUST replay (next_lsn was correct).
    let mut e = Engine::new(&restored).unwrap();
    assert_eq!(count(&mut e), 150, "post-restore writes must survive a crash — no LSN reset");
}
```

> Note: this test references `powdb_query` + `powdb_backup` together. Put it in `crates/backup/tests/` and add `powdb-query` as a `[dev-dependencies]` of `powdb-backup`. Trim the first block's placeholder `catalog_mut()` call — the canonical path is opening a `Catalog` directly as the second block does.

- [ ] **Step 2: Run, verify pass, commit**

Run: `cargo test -p powdb-backup --test restore_durability`
Expected: PASS. Inner-loop checks, then commit `test(backup): restored DB survives write-then-crash (LSN invariant)`.

---

### Task 6: CLI `backup` / `restore` subcommands

**Files:**
- Modify: `crates/cli/src/main.rs`
- Modify: `crates/cli/Cargo.toml` (add `powdb-backup` dep)
- Test: `crates/cli/tests/backup_cli.rs`

- [ ] **Step 1: Write the failing CLI integration test**

`crates/cli/tests/backup_cli.rs` drives the built binary via `assert_cmd` or `std::process::Command`:

```rust
use std::process::Command;
fn bin() -> &'static str { env!("CARGO_BIN_EXE_powdb-cli") }
fn tmp(t: &str) -> std::path::PathBuf { /* same helper as above */ }

#[test]
fn cli_backup_then_restore_roundtrip() {
    let data = tmp("data");
    // seed via one-shot exec
    let seed = Command::new(bin())
        .args(["--data-dir", data.to_str().unwrap(), "-c", "type T { required id: int }"])
        .status().unwrap();
    assert!(seed.success());
    for i in 0..10 {
        Command::new(bin())
            .args(["--data-dir", data.to_str().unwrap(), "-c", &format!("insert T {{ id := {i} }}")])
            .status().unwrap();
    }
    let backup = tmp("bkp");
    let b = Command::new(bin())
        .args(["--data-dir", data.to_str().unwrap(), "backup", backup.to_str().unwrap()])
        .status().unwrap();
    assert!(b.success(), "backup subcommand must succeed");

    let restored = tmp("restored");
    let r = Command::new(bin())
        .args(["restore", backup.to_str().unwrap(), restored.to_str().unwrap()])
        .status().unwrap();
    assert!(r.success(), "restore subcommand must succeed");

    // verify restored data
    let out = Command::new(bin())
        .args(["--data-dir", restored.to_str().unwrap(), "-c", "count(T)"])
        .output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("10"));
}
```

- [ ] **Step 2: Run, verify it fails** (`backup`/`restore` are unknown args). 
- [ ] **Step 3: Implement the subcommands** in `main.rs`'s arg loop: a positional `backup <dest>` opens an `Engine`/`Catalog` on `--data-dir` and calls `powdb_backup::full_backup`; a positional `restore <backup_dir> <dest>` calls `powdb_backup::restore`. Print a one-line summary (files, bytes, source_lsn). Add `powdb-backup = { version = "0.4.5", path = "../backup" }` to `crates/cli/Cargo.toml`.
- [ ] **Step 4: Run, verify pass.**
- [ ] **Step 5: Verification + commit** `feat(cli): backup and restore subcommands`.

---

### Task 7: Operator docs + Phase-1 exit gate

**Files:**
- Create: `docs/backup-and-restore.md`
- Modify: `README.md` (link it), `CHANGELOG.md`

- [ ] **Step 1:** Write `docs/backup-and-restore.md`: what a backup is (consistent snapshot at an LSN), the commands, the integrity guarantee, the **honest limits** (same-engine-version only; a backup is full-DB; restore is offline to a fresh dir; coarse PITR only until Phase 5). Link from README.
- [ ] **Step 2:** Add a CHANGELOG entry under a new `## [Unreleased]` / next version: "Full backup + restore (`powdb-cli backup`/`restore`), blake3-verified, crash-consistent via checkpoint."
- [ ] **Step 3: PHASE-1 EXIT GATE (the anti-slowdown proof):**
  - Run: `cargo test --workspace` → 0 failed, count ≥ baseline + new backup tests.
  - Run: `cargo clippy --workspace --all-targets -- -D warnings` → clean.
  - Run: `cargo run --release -p powdb-bench --bin compare` → **PASS** (no workload regressed — expected, P1 added no hot-path code).
  - Run: `cargo test -p powdb-query --test durability` → green.
- [ ] **Step 4: Commit** `docs(backup): operator guide + phase-1 changelog`.

**Phase 1 is done when:** `powdb-cli backup`/`restore` round-trips a real DB, the restored DB survives write-then-crash, integrity is enforced, and all four exit-gate checks pass.

---

## Phase 2: Incremental Backup + Coarse PITR (task-level)

**Substrate:** S1 (page-LSN diff). **Hot-path risk:** none (diff runs at backup time). **Expand to TDD steps at execution.**

- **Task 2.1** — `HeapFile::pages_changed_since(lsn) -> impl Iterator<Item=(page_id, &[u8])>`: enumerate pages whose `page_lsn > lsn`. Test: write N rows, snapshot LSN, mutate M pages, assert exactly the changed pages are returned. (Reuses `page_lsn`, `num_pages`.)
- **Task 2.2** — `incremental_backup(catalog, base_manifest, dest)`: checkpoint, then for each heap write only changed pages into a sparse `*.heap.delta` keyed by page_id; manifest records `base_lsn` (chain pointer) + `source_lsn`. Test: change a small subset, assert delta size ≪ full, manifest chains to base.
- **Task 2.3** — `restore` accepts a base + ordered increments: apply base, then overlay each delta's pages by page_id. Test: full == (base ⊕ increments) byte-for-byte; restored DB row-count correct.
- **Task 2.4** — Coarse PITR surface: `restore --as-of <backup-id|timestamp>` picks the newest backup ≤ target and restores it. Test: three timestamped backups, restore "as of" the middle, assert middle state.
- **Task 2.5** — CLI `backup --incremental --base <dir>`, docs, **exit gate** (full suite + bench `compare` + durability suite).

**Exit:** incremental backup smaller than full, base+increment restore == full restore, coarse PITR selects the right snapshot, bench gate green.

---

## Phase 3: Cloud DR Sync (task-level)

**Substrate:** S1 (scheduled incremental → object store). **Hot-path risk:** none. **Expand to TDD steps at execution.**

- **Task 3.1** — `ObjectStore` trait (`put`, `get`, `list`, `delete`) with a `FsStore` impl (a local dir = the test/dev target) so everything is testable without a real S3. Test: round-trip put/get/list.
- **Task 3.2** — `push(store, data_dir, &mut catalog)`: full backup on first run, then incrementals; upload base + deltas + a remote `chain.json` index. Test (against `FsStore`): two pushes, second uploads only a delta; chain index lists both.
- **Task 3.3** — `restore_from_cloud(store, dest)`: download chain + apply base+increments via Phase-2 restore. Test: push from DB A, restore into DB B, assert identical.
- **Task 3.4** — Scheduler hook (server side): a `--backup-interval`/`POWDB_BACKUP_*` config that calls `push` on a timer; off by default. Test: interval fires → store gains an increment. **Crucially: the timer must take the same checkpoint-quiesce path and not block reads.** Add an S3-compatible impl behind a feature flag (real creds out of scope for tests).
- **Task 3.5** — Docs (DR runbook), **exit gate** (suite + bench + durability).

**Exit:** scheduled push to an object store, restore-from-cloud reproduces the DB, the timer doesn't regress the hot path (bench gate).

---

## Phase 4: Schema Migration Framework (task-level)

**Substrate:** S3 (orthogonal). **Hot-path risk:** none. New crate `crates/migrate` (`powdb-migrate`). **Expand to TDD steps at execution.**

- **Task 4.1** — Applied-version store: a reserved `_migrations` table (or catalog record) `{ version: int, name: str, applied_unix: int, checksum: str }`. Test: fresh DB has version 0; recording advances it.
- **Task 4.2** — Migration file format + loader: ordered files `migrations/NNNN_name.powql` with `-- up` / `-- down` sections (PowQL DDL). Test: load + parse a dir, reject gaps/dupes in version numbers, checksum each.
- **Task 4.3** — `migrate(engine, dir)`: apply pending in order; record each in `_migrations`; **idempotent** (re-run = no-op). Test: apply twice → version stable, schema correct.
- **Task 4.4** — Safety: because PowDB has **no multi-statement DDL atomicity** (`rollback` rewinds data, not committed DDL — verified in design doc), a migration with >1 DDL step **takes a pre-migration backup** (Phase 1) and, on failure, instructs/automates restore. Test: a migration whose 2nd step errors → DB is restored to the pre-migration snapshot; `_migrations` not advanced.
- **Task 4.5** — `rollback(engine, dir)`: run the `-- down` of the latest applied migration; decrement version. Test: up then down returns to prior schema.
- **Task 4.6** — CLI `migrate` / `migrate status` / `rollback`, docs, **exit gate** (suite + bench + durability).

**Exit:** ordered idempotent apply, status reporting, rollback, and failure-safety via pre-migration snapshot; bench gate green.

---

## Phase 5: Fine-Grained PITR via Retained WAL Archive (task-level) — HOT-PATH SENSITIVE

**Substrate:** S2 (the only new substrate). **Hot-path risk: YES — archiving touches the commit path.** Build last. **Expand to TDD steps at execution.**

- **Task 5.1** — WAL segment rotation: instead of one growing `wal.log` truncated at checkpoint, roll sealed segments `wal-<startLSN>.seg`. Test: segments seal at checkpoint with correct LSN ranges; recovery still replays across segments. **Run the full durability suite — this changes the WAL lifecycle.**
- **Task 5.2** — Archive-before-truncate hook: on checkpoint, copy the sealed segment to an archive store (local dir / `ObjectStore` from P3) before truncation; key by LSN range. Test: archive accumulates every segment; gap detection errors loudly.
- **Task 5.3** — `restore --to-lsn <N>` / `--to-time <ts>`: restore the newest base ≤ target, then replay archived WAL segments up to the target LSN (stop mid-segment at the right record). Test: insert at t1, t2, t3; restore to t2; assert exactly t1+t2 state. **This reuses `replay_wal` + the per-page idempotent redo already in `catalog.rs`.**
- **Task 5.4** — Retention/GC: prune archived segments older than the oldest base backup; never delete a segment a retained base still needs. Test: GC keeps the minimal closure.
- **Task 5.5 — THE GATE THAT MATTERS:** `cargo run --release -p powdb-bench --bin compare` must show **no regression on `insert`/`update`/`delete` workloads**. Archiving must be off the synchronous commit latency path (e.g. copy the *already-sealed* segment, never the live append). If write throughput regresses, the design is wrong — do not ship. Also re-run the full durability suite.

**Exit:** restore to an exact LSN/timestamp works, retention is safe, and **write performance is unchanged** (hard bench gate). Until this gate is green, fine PITR is not done.

---

## Self-Review (against the design doc)

**Spec coverage:** incremental backup (P2), PITR coarse (P2) + fine (P5), cloud sync DR (P3), migrations (P4) — all four requested features map to phases; full backup/restore (P1) is the prerequisite the design doc identified. ✓ Local-first/PowSync explicitly excluded per scope. ✓

**Consistent-snapshot design:** checkpoint-then-copy (Task 3) matches the design doc's recommended Option A; the brief quiesce = checkpoint duration. ✓

**Restore LSN invariant:** every restore ends in `Catalog::open` (Tasks 4, 5) which sets `next_lsn = max_page_lsn+1`; Task 5 is a dedicated crash-after-restore test for exactly the v0.4.3 P0 class. ✓

**Anti-slowdown requirement (the user's explicit ask):** baked into the Continuous-Verification Protocol (per-step full-suite + clippy; per-phase `bench compare` gate) and made a hard, named exit gate in every phase — with Phase 5 flagged as the one hot-path-sensitive phase and gated hardest. ✓

**Type consistency:** `BackupManifest`/`FileEntry` defined in Task 2 are used unchanged in Tasks 3–6; `full_backup`/`restore` signatures are stable across CLI + tests; `source_lsn`/`max_lsn()` naming consistent. ✓

**Placeholder scan:** Phase 1 steps contain real code + real commands. Phases 2–5 are deliberately task-level (not micro-step) and explicitly marked "expand at execution" — this is the scope-check decomposition, not a placeholder; their interfaces depend on P1's emergent API and pre-writing them would be speculative/wrong.
