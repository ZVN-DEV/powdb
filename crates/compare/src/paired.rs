//! Paired PowDB-vs-SQLite value benchmark driver.
//!
//! This binary is intentionally separate from `compare-engines`: the legacy
//! runner remains a human-readable diagnostic, while this one emits one JSON
//! document per process for pairing baseline/candidate binaries.

use powdb_query::ast::Literal;
use powdb_query::executor::{Engine, PreparedQuery};
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use powdb_storage::wal::WalSyncMode;
use rusqlite::{params, Connection};
use std::collections::{BTreeMap, BTreeSet};
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tempfile::TempDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EngineKind {
    Powdb,
    Sqlite,
}

impl EngineKind {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "powdb" => Ok(Self::Powdb),
            "sqlite" => Ok(Self::Sqlite),
            other => Err(format!("unknown engine '{other}', expected powdb|sqlite")),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Powdb => "powdb",
            Self::Sqlite => "sqlite",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Full,
    Off,
}

impl Mode {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "full" => Ok(Self::Full),
            "off" => Ok(Self::Off),
            other => Err(format!("unknown mode '{other}', expected full|off")),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Off => "off",
        }
    }

    fn publishable(self) -> bool {
        matches!(self, Self::Full)
    }
}

#[derive(Debug, Clone)]
struct Config {
    engine: EngineKind,
    mode: Mode,
    profile: String,
    engine_ref: String,
    engine_hash: String,
    round: usize,
    order_index: usize,
    fixture_rows: usize,
    point_read_ops: usize,
    write_ops: usize,
    scan_ops: usize,
    batch_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            engine: EngineKind::Powdb,
            mode: Mode::Full,
            profile: "value-v1".to_string(),
            engine_ref: "unknown".to_string(),
            engine_hash: "unknown".to_string(),
            round: 0,
            order_index: 0,
            fixture_rows: 20_000,
            point_read_ops: 5_000,
            write_ops: 1_000,
            scan_ops: 200,
            batch_size: 100,
        }
    }
}

#[derive(Debug, Clone)]
struct WorkloadResult {
    name: &'static str,
    operations: usize,
    mean_ns_per_op: f64,
    protected: bool,
    timed: bool,
}

#[derive(Debug, Clone)]
struct CheckResult {
    name: &'static str,
    passed: bool,
    detail: String,
}

#[derive(Debug)]
struct RunReport {
    config: Config,
    sqlite_storage: &'static str,
    publishable: bool,
    workloads: Vec<WorkloadResult>,
    checks: Vec<CheckResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UserRow {
    name: String,
    age: i64,
}

trait ValueEngine {
    fn storage_label(&self) -> &'static str;
    fn setup(&mut self, rows: usize) -> Result<(), String>;
    fn point_read(&mut self, id: i64) -> Result<UserRow, String>;
    fn update_age(&mut self, id: i64, age: i64) -> Result<u64, String>;
    fn insert_one(&mut self, id: i64, age: i64) -> Result<(), String>;
    fn insert_batch(&mut self, start_id: i64, rows: usize) -> Result<(), String>;
    fn scan_filter_count(&mut self) -> Result<usize, String>;
    fn aggregate_sum(&mut self) -> Result<i64, String>;
    fn row_count(&mut self) -> Result<usize, String>;
    fn reopen_check(&mut self, expected_count: usize, expected_sum: i64) -> Result<String, String>;
}

struct PowdbValueEngine {
    dir: TempDir,
    engine: Engine,
    mode: Mode,
    point_read: Option<PreparedQuery>,
    update_age: Option<PreparedQuery>,
    insert_one: Option<PreparedQuery>,
}

impl PowdbValueEngine {
    fn new(mode: Mode) -> Result<Self, String> {
        let dir = TempDir::new().map_err(|e| e.to_string())?;
        let mut engine = Engine::new(dir.path()).map_err(|e| e.to_string())?;
        engine.catalog_mut().set_wal_sync_mode(match mode {
            Mode::Full => WalSyncMode::Full,
            Mode::Off => WalSyncMode::Off,
        });
        Ok(Self {
            dir,
            engine,
            mode,
            point_read: None,
            update_age: None,
            insert_one: None,
        })
    }

    fn ensure_point_read(&mut self) -> Result<(), String> {
        if self.point_read.is_none() {
            self.point_read = Some(
                self.engine
                    .prepare("User filter .id = 0 limit 1 { .name, .age }")
                    .map_err(|e| e.to_string())?,
            );
        }
        Ok(())
    }

    fn ensure_update_age(&mut self) -> Result<(), String> {
        if self.update_age.is_none() {
            self.update_age = Some(
                self.engine
                    .prepare("User filter .id = 0 update { age := 0 }")
                    .map_err(|e| e.to_string())?,
            );
        }
        Ok(())
    }

    fn ensure_insert_one(&mut self) -> Result<(), String> {
        if self.insert_one.is_none() {
            self.insert_one = Some(
                self.engine
                    .prepare(
                        r#"insert User { id := 0, name := "", age := 0, status := "", email := "", created_at := 0 }"#,
                    )
                    .map_err(|e| e.to_string())?,
            );
        }
        Ok(())
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn execute_insert(
        engine: &mut Engine,
        prep: &PreparedQuery,
        id: i64,
        age: i64,
    ) -> Result<(), String> {
        let result = engine
            .execute_prepared(
                prep,
                &[
                    Literal::Int(id),
                    Literal::String(format!("user_{id}")),
                    Literal::Int(age),
                    Literal::String(status_for(id).to_string()),
                    Literal::String(format!("user_{id}@example.com")),
                    Literal::Int(1_700_000_000 + id),
                ],
            )
            .map_err(|e| e.to_string())?;
        expect_modified(result, 1).map(|_| ())
    }
}

impl ValueEngine for PowdbValueEngine {
    fn storage_label(&self) -> &'static str {
        match self.mode {
            Mode::Full => "file-backed-wal-full",
            Mode::Off => "file-backed-wal-off-diagnostic",
        }
    }

    fn setup(&mut self, rows: usize) -> Result<(), String> {
        self.engine
            .execute_powql(
                "type User { required unique id: int, required name: str, required age: int, required status: str, required email: str, required created_at: int }",
            )
            .map_err(|e| e.to_string())?;
        let insert = self
            .engine
            .prepare(
                r#"insert User { id := 0, name := "", age := 0, status := "", email := "", created_at := 0 }"#,
            )
            .map_err(|e| e.to_string())?;
        for i in 0..rows {
            let id = i as i64;
            Self::execute_insert(&mut self.engine, &insert, id, fixture_age(id))?;
        }
        self.point_read = None;
        self.update_age = None;
        self.insert_one = None;
        Ok(())
    }

    fn point_read(&mut self, id: i64) -> Result<UserRow, String> {
        self.ensure_point_read()?;
        let prep = self.point_read.as_ref().expect("point_read set");
        let result = self
            .engine
            .execute_prepared(prep, &[Literal::Int(id), Literal::Int(1)])
            .map_err(|e| e.to_string())?;
        match result {
            QueryResult::Rows { rows, .. } if rows.len() == 1 => row_from_values(&rows[0]),
            QueryResult::Rows { rows, .. } => {
                Err(format!("expected one row for id {id}, got {}", rows.len()))
            }
            other => Err(format!("expected rows for point_read, got {other:?}")),
        }
    }

    fn update_age(&mut self, id: i64, age: i64) -> Result<u64, String> {
        self.ensure_update_age()?;
        let prep = self.update_age.as_ref().expect("update_age set");
        let result = self
            .engine
            .execute_prepared(prep, &[Literal::Int(id), Literal::Int(age)])
            .map_err(|e| e.to_string())?;
        expect_modified(result, 1)
    }

    fn insert_one(&mut self, id: i64, age: i64) -> Result<(), String> {
        self.ensure_insert_one()?;
        let prep = self.insert_one.as_ref().expect("insert_one set");
        Self::execute_insert(&mut self.engine, prep, id, age)
    }

    fn insert_batch(&mut self, start_id: i64, rows: usize) -> Result<(), String> {
        self.ensure_insert_one()?;
        let prep = self.insert_one.as_ref().expect("insert_one set");
        self.engine
            .execute_powql("begin")
            .map_err(|e| e.to_string())?;
        for offset in 0..rows {
            let id = start_id + offset as i64;
            Self::execute_insert(&mut self.engine, prep, id, fixture_age(id))?;
        }
        self.engine
            .execute_powql("commit")
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn scan_filter_count(&mut self) -> Result<usize, String> {
        let result = self
            .engine
            .execute_powql("count(User filter .age > 30)")
            .map_err(|e| e.to_string())?;
        match result {
            QueryResult::Scalar(Value::Int(n)) => Ok(n as usize),
            other => Err(format!("expected integer scalar count, got {other:?}")),
        }
    }

    fn aggregate_sum(&mut self) -> Result<i64, String> {
        let result = self
            .engine
            .execute_powql("sum(User { .age })")
            .map_err(|e| e.to_string())?;
        match result {
            QueryResult::Scalar(Value::Int(n)) => Ok(n),
            other => Err(format!("expected integer scalar sum, got {other:?}")),
        }
    }

    fn row_count(&mut self) -> Result<usize, String> {
        let result = self
            .engine
            .execute_powql("count(User)")
            .map_err(|e| e.to_string())?;
        match result {
            QueryResult::Scalar(Value::Int(n)) => Ok(n as usize),
            other => Err(format!("expected integer scalar row count, got {other:?}")),
        }
    }

    fn reopen_check(&mut self, expected_count: usize, expected_sum: i64) -> Result<String, String> {
        let path = self.path().to_path_buf();
        drop(std::mem::replace(
            &mut self.engine,
            Engine::new(&path).map_err(|e| e.to_string())?,
        ));
        self.engine
            .catalog_mut()
            .set_wal_sync_mode(match self.mode {
                Mode::Full => WalSyncMode::Full,
                Mode::Off => WalSyncMode::Off,
            });
        self.point_read = None;
        self.update_age = None;
        self.insert_one = None;
        let count = self.row_count()?;
        let sum = self.aggregate_sum()?;
        if count == expected_count && sum == expected_sum {
            Ok(format!("count={count},sum={sum}"))
        } else {
            Err(format!(
                "after reopen expected count={expected_count},sum={expected_sum}; got count={count},sum={sum}"
            ))
        }
    }
}

struct SqliteValueEngine {
    mode: Mode,
    _dir: Option<TempDir>,
    path: Option<PathBuf>,
    conn: Connection,
}

impl SqliteValueEngine {
    fn new(mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Full => {
                let dir = TempDir::new().map_err(|e| e.to_string())?;
                let path = dir.path().join("value.sqlite3");
                let conn = Connection::open(&path).map_err(|e| e.to_string())?;
                configure_sqlite_full(&conn)?;
                Ok(Self {
                    mode,
                    _dir: Some(dir),
                    path: Some(path),
                    conn,
                })
            }
            Mode::Off => {
                let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
                Ok(Self {
                    mode,
                    _dir: None,
                    path: None,
                    conn,
                })
            }
        }
    }
}

impl ValueEngine for SqliteValueEngine {
    fn storage_label(&self) -> &'static str {
        match self.mode {
            Mode::Full => "file-backed-wal-full",
            Mode::Off => "memory-diagnostic",
        }
    }

    fn setup(&mut self, rows: usize) -> Result<(), String> {
        self.conn
            .execute_batch(
                "CREATE TABLE user_table (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    age INTEGER NOT NULL,
                    status TEXT NOT NULL,
                    email TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );",
            )
            .map_err(|e| e.to_string())?;
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO user_table (id, name, age, status, email, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .map_err(|e| e.to_string())?;
            for i in 0..rows {
                let id = i as i64;
                stmt.execute(params![
                    id,
                    format!("user_{id}"),
                    fixture_age(id),
                    status_for(id),
                    format!("user_{id}@example.com"),
                    1_700_000_000 + id
                ])
                .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    fn point_read(&mut self, id: i64) -> Result<UserRow, String> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT name, age FROM user_table WHERE id = ?1")
            .map_err(|e| e.to_string())?;
        stmt.query_row(params![id], |row| {
            Ok(UserRow {
                name: row.get(0)?,
                age: row.get(1)?,
            })
        })
        .map_err(|e| e.to_string())
    }

    fn update_age(&mut self, id: i64, age: i64) -> Result<u64, String> {
        let changed = self
            .conn
            .prepare_cached("UPDATE user_table SET age = ?1 WHERE id = ?2")
            .map_err(|e| e.to_string())?
            .execute(params![age, id])
            .map_err(|e| e.to_string())?;
        if changed == 1 {
            Ok(1)
        } else {
            Err(format!(
                "expected one updated row for id {id}, got {changed}"
            ))
        }
    }

    fn insert_one(&mut self, id: i64, age: i64) -> Result<(), String> {
        self.conn
            .prepare_cached(
                "INSERT INTO user_table (id, name, age, status, email, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(|e| e.to_string())?
            .execute(params![
                id,
                format!("user_{id}"),
                age,
                status_for(id),
                format!("user_{id}@example.com"),
                1_700_000_000 + id
            ])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn insert_batch(&mut self, start_id: i64, rows: usize) -> Result<(), String> {
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO user_table (id, name, age, status, email, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .map_err(|e| e.to_string())?;
            for offset in 0..rows {
                let id = start_id + offset as i64;
                stmt.execute(params![
                    id,
                    format!("user_{id}"),
                    fixture_age(id),
                    status_for(id),
                    format!("user_{id}@example.com"),
                    1_700_000_000 + id
                ])
                .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    fn scan_filter_count(&mut self) -> Result<usize, String> {
        self.conn
            .prepare_cached("SELECT COUNT(*) FROM user_table WHERE age > ?1")
            .map_err(|e| e.to_string())?
            .query_row(params![30], |row| row.get::<_, i64>(0))
            .map(|n| n as usize)
            .map_err(|e| e.to_string())
    }

    fn aggregate_sum(&mut self) -> Result<i64, String> {
        self.conn
            .prepare_cached("SELECT SUM(age) FROM user_table")
            .map_err(|e| e.to_string())?
            .query_row([], |row| row.get::<_, i64>(0))
            .map_err(|e| e.to_string())
    }

    fn row_count(&mut self) -> Result<usize, String> {
        self.conn
            .prepare_cached("SELECT COUNT(*) FROM user_table")
            .map_err(|e| e.to_string())?
            .query_row([], |row| row.get::<_, i64>(0))
            .map(|n| n as usize)
            .map_err(|e| e.to_string())
    }

    fn reopen_check(&mut self, expected_count: usize, expected_sum: i64) -> Result<String, String> {
        let Some(path) = self.path.clone() else {
            return Ok("skipped:sqlite-memory-diagnostic".to_string());
        };
        let replacement = Connection::open(&path).map_err(|e| e.to_string())?;
        configure_sqlite_full(&replacement)?;
        drop(std::mem::replace(&mut self.conn, replacement));
        let count = self.row_count()?;
        let sum = self.aggregate_sum()?;
        if count == expected_count && sum == expected_sum {
            Ok(format!("count={count},sum={sum}"))
        } else {
            Err(format!(
                "after reopen expected count={expected_count},sum={expected_sum}; got count={count},sum={sum}"
            ))
        }
    }
}

fn configure_sqlite_full(conn: &Connection) -> Result<(), String> {
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn run(config: Config) -> Result<RunReport, String> {
    if config.profile != "value-v1" {
        return Err(format!(
            "unsupported profile '{}'; expected value-v1",
            config.profile
        ));
    }
    if config.fixture_rows == 0 {
        return Err("--fixture-rows must be positive".to_string());
    }
    if config.batch_size == 0 {
        return Err("--batch-size must be positive".to_string());
    }

    let mut engine: Box<dyn ValueEngine> = match config.engine {
        EngineKind::Powdb => Box::new(PowdbValueEngine::new(config.mode)?),
        EngineKind::Sqlite => Box::new(SqliteValueEngine::new(config.mode)?),
    };
    engine.setup(config.fixture_rows)?;
    let sqlite_storage = engine.storage_label();
    let mut expected_ages: BTreeMap<i64, i64> = (0..config.fixture_rows)
        .map(|i| (i as i64, fixture_age(i as i64)))
        .collect();
    let mut checks = Vec::new();
    check_fixture(
        &mut *engine,
        config.fixture_rows,
        &expected_ages,
        &mut checks,
    );

    warmup(&mut *engine, config.fixture_rows)?;

    let mut workloads = Vec::new();
    let read_keys = varied_keys(config.point_read_ops, config.fixture_rows, 0x51);
    let point_mean = time_ops(read_keys.len(), |i| {
        let row = engine.point_read(read_keys[i]).expect("timed point read");
        black_box(row);
    });
    workloads.push(WorkloadResult {
        name: "point_read_indexed",
        operations: read_keys.len(),
        mean_ns_per_op: point_mean,
        protected: false,
        timed: true,
    });

    let update_keys = varied_keys(config.write_ops, config.fixture_rows, 0xA7);
    let update_mean = time_ops(update_keys.len(), |i| {
        let id = update_keys[i];
        let age = changed_age(i);
        engine.update_age(id, age).expect("timed update");
        black_box(age);
    });
    for (i, id) in update_keys.iter().copied().enumerate() {
        expected_ages.insert(id, changed_age(i));
    }
    workloads.push(WorkloadResult {
        name: "point_update_changed_value",
        operations: update_keys.len(),
        mean_ns_per_op: update_mean,
        protected: false,
        timed: true,
    });

    let insert_start = config.fixture_rows as i64;
    let insert_mean = time_ops(config.write_ops, |i| {
        let id = insert_start + i as i64;
        let age = fixture_age(id);
        engine.insert_one(id, age).expect("timed insert");
        black_box(id);
    });
    for i in 0..config.write_ops {
        let id = insert_start + i as i64;
        expected_ages.insert(id, fixture_age(id));
    }
    workloads.push(WorkloadResult {
        name: "prepared_insert_single",
        operations: config.write_ops,
        mean_ns_per_op: insert_mean,
        protected: false,
        timed: true,
    });

    let batch_ops = config.write_ops.div_ceil(config.batch_size);
    let batch_start = insert_start + config.write_ops as i64;
    let batch_mean = time_ops(batch_ops, |batch| {
        let start = batch_start + (batch * config.batch_size) as i64;
        engine
            .insert_batch(start, config.batch_size)
            .expect("timed batch insert");
        black_box(start);
    });
    for i in 0..(batch_ops * config.batch_size) {
        let id = batch_start + i as i64;
        expected_ages.insert(id, fixture_age(id));
    }
    workloads.push(WorkloadResult {
        name: "prepared_insert_bounded_batch",
        operations: batch_ops * config.batch_size,
        mean_ns_per_op: batch_mean / config.batch_size as f64,
        protected: false,
        timed: true,
    });

    let scan_mean = time_ops(config.scan_ops, |_| {
        let n = engine.scan_filter_count().expect("timed scan count");
        black_box(n);
    });
    workloads.push(WorkloadResult {
        name: "protected_scan_filter_count",
        operations: config.scan_ops,
        mean_ns_per_op: scan_mean,
        protected: true,
        timed: true,
    });

    let agg_mean = time_ops(config.scan_ops, |_| {
        let n = engine.aggregate_sum().expect("timed aggregate sum");
        black_box(n);
    });
    workloads.push(WorkloadResult {
        name: "protected_aggregate_sum",
        operations: config.scan_ops,
        mean_ns_per_op: agg_mean,
        protected: true,
        timed: true,
    });

    check_mutations(&mut *engine, &expected_ages, &update_keys, &mut checks);
    let expected_count = expected_ages.len();
    let expected_sum = expected_ages.values().sum::<i64>();
    checks.push(match engine.reopen_check(expected_count, expected_sum) {
        Ok(detail) => CheckResult {
            name: "reopen_parity",
            passed: true,
            detail,
        },
        Err(detail) => CheckResult {
            name: "reopen_parity",
            passed: false,
            detail,
        },
    });

    let all_checks_passed = checks.iter().all(|c| c.passed);
    Ok(RunReport {
        config,
        sqlite_storage,
        publishable: all_checks_passed,
        workloads,
        checks,
    })
}

fn warmup(engine: &mut dyn ValueEngine, rows: usize) -> Result<(), String> {
    for id in [0, (rows / 2) as i64, rows.saturating_sub(1) as i64] {
        black_box(engine.point_read(id)?);
    }
    black_box(engine.scan_filter_count()?);
    black_box(engine.aggregate_sum()?);
    Ok(())
}

fn check_fixture(
    engine: &mut dyn ValueEngine,
    rows: usize,
    expected_ages: &BTreeMap<i64, i64>,
    checks: &mut Vec<CheckResult>,
) {
    let mut failures = Vec::new();
    for id in [0, (rows / 2) as i64, rows.saturating_sub(1) as i64] {
        match engine.point_read(id) {
            Ok(row) if row.name == format!("user_{id}") && row.age == expected_ages[&id] => {}
            Ok(row) => failures.push(format!("id {id} got {row:?}")),
            Err(e) => failures.push(format!("id {id} error {e}")),
        }
    }
    checks.push(CheckResult {
        name: "fixture_point_parity",
        passed: failures.is_empty(),
        detail: if failures.is_empty() {
            "sample point reads matched fixture".to_string()
        } else {
            failures.join("; ")
        },
    });

    let expected_count = expected_ages.values().filter(|age| **age > 30).count();
    let expected_sum = expected_ages.values().sum::<i64>();
    let scan_ok = engine.scan_filter_count().ok() == Some(expected_count);
    checks.push(CheckResult {
        name: "fixture_scan_count",
        passed: scan_ok,
        detail: format!("expected {expected_count}"),
    });
    let sum_ok = engine.aggregate_sum().ok() == Some(expected_sum);
    checks.push(CheckResult {
        name: "fixture_aggregate_sum",
        passed: sum_ok,
        detail: format!("expected {expected_sum}"),
    });
}

fn check_mutations(
    engine: &mut dyn ValueEngine,
    expected_ages: &BTreeMap<i64, i64>,
    update_keys: &[i64],
    checks: &mut Vec<CheckResult>,
) {
    let mut sample_ids: BTreeSet<i64> = update_keys.iter().copied().take(8).collect();
    sample_ids.insert(0);
    sample_ids.insert(expected_ages.len().saturating_sub(1) as i64);
    let mut failures = Vec::new();
    for id in sample_ids {
        match (engine.point_read(id), expected_ages.get(&id)) {
            (Ok(row), Some(age)) if row.age == *age => {}
            (Ok(row), Some(age)) => {
                failures.push(format!("id {id} expected age {age}, got {}", row.age))
            }
            (Ok(_), None) => failures.push(format!("id {id} unexpectedly exists")),
            (Err(e), Some(_)) => failures.push(format!("id {id} missing/error {e}")),
            (Err(_), None) => {}
        }
    }
    checks.push(CheckResult {
        name: "mutation_readback",
        passed: failures.is_empty(),
        detail: if failures.is_empty() {
            "updated and inserted sample rows read back".to_string()
        } else {
            failures.join("; ")
        },
    });

    let expected_count = expected_ages.len();
    let expected_sum = expected_ages.values().sum::<i64>();
    let count_ok = engine.row_count().ok() == Some(expected_count);
    checks.push(CheckResult {
        name: "post_mutation_row_count",
        passed: count_ok,
        detail: format!("expected {expected_count}"),
    });
    let sum_ok = engine.aggregate_sum().ok() == Some(expected_sum);
    checks.push(CheckResult {
        name: "post_mutation_aggregate_sum",
        passed: sum_ok,
        detail: format!("expected {expected_sum}"),
    });
}

fn time_ops<F: FnMut(usize)>(ops: usize, mut f: F) -> f64 {
    assert!(ops > 0, "ops must be positive");
    let start = Instant::now();
    for i in 0..ops {
        f(i);
    }
    start.elapsed().as_nanos() as f64 / ops as f64
}

fn varied_keys(ops: usize, rows: usize, salt: u64) -> Vec<i64> {
    assert!(rows > 0, "rows must be positive");
    (0..ops)
        .map(|i| (mix64(i as u64 ^ salt) % rows as u64) as i64)
        .collect()
}

fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn fixture_age(id: i64) -> i64 {
    18 + id.rem_euclid(60)
}

fn changed_age(iteration: usize) -> i64 {
    10_000 + iteration as i64
}

fn status_for(id: i64) -> &'static str {
    match id.rem_euclid(3) {
        0 => "active",
        1 => "inactive",
        _ => "pending",
    }
}

fn row_from_values(values: &[Value]) -> Result<UserRow, String> {
    match values {
        [Value::Str(name), Value::Int(age)] => Ok(UserRow {
            name: name.clone(),
            age: *age,
        }),
        other => Err(format!("expected [str,int] row, got {other:?}")),
    }
}

fn expect_modified(result: QueryResult, expected: u64) -> Result<u64, String> {
    match result {
        QueryResult::Modified(n) if n == expected => Ok(n),
        QueryResult::Modified(n) => Err(format!("expected {expected} modified rows, got {n}")),
        other => Err(format!("expected modified result, got {other:?}")),
    }
}

fn parse_args<I>(args: I) -> Result<Config, String>
where
    I: IntoIterator<Item = String>,
{
    let mut config = Config::default();
    let mut args = args.into_iter();
    let _program = args.next();
    while let Some(arg) = args.next() {
        let (flag, value) = if let Some((flag, value)) = arg.split_once('=') {
            (flag.to_string(), value.to_string())
        } else if arg == "--help" || arg == "-h" {
            return Err(usage());
        } else {
            let value = args
                .next()
                .ok_or_else(|| format!("missing value for {arg}\n{}", usage()))?;
            (arg, value)
        };
        match flag.as_str() {
            "--engine" => config.engine = EngineKind::parse(&value)?,
            "--mode" => config.mode = Mode::parse(&value)?,
            "--profile" => config.profile = value,
            "--engine-ref" => config.engine_ref = value,
            "--engine-hash" => config.engine_hash = value,
            "--round" => config.round = parse_usize(&flag, &value)?,
            "--order-index" => config.order_index = parse_usize(&flag, &value)?,
            "--fixture-rows" => config.fixture_rows = parse_usize(&flag, &value)?,
            "--point-read-ops" => config.point_read_ops = parse_usize(&flag, &value)?,
            "--write-ops" => config.write_ops = parse_usize(&flag, &value)?,
            "--scan-ops" => config.scan_ops = parse_usize(&flag, &value)?,
            "--batch-size" => config.batch_size = parse_usize(&flag, &value)?,
            other => return Err(format!("unknown flag {other}\n{}", usage())),
        }
    }
    Ok(config)
}

fn parse_usize(flag: &str, value: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|e| format!("invalid value for {flag}: {value}: {e}"))
}

fn usage() -> String {
    "usage: powdb-compare-paired --engine powdb|sqlite --mode full|off [--profile value-v1] [--engine-ref LABEL] [--engine-hash HASH] [--round N] [--order-index N] [--fixture-rows N] [--point-read-ops N] [--write-ops N] [--scan-ops N] [--batch-size N]".to_string()
}

fn write_json(report: &RunReport) {
    let cfg = &report.config;
    print!("{{");
    print_json_field("schema", "powdb.compare.paired.v1", false);
    print_json_field("profile", &cfg.profile, true);
    print_json_field("engine", cfg.engine.as_str(), true);
    print_json_field("engine_ref", &cfg.engine_ref, true);
    print_json_field("engine_hash", &cfg.engine_hash, true);
    print_json_field("mode", cfg.mode.as_str(), true);
    print_json_field("storage", report.sqlite_storage, true);
    print!(
        ",\"publishable\":{}",
        report.publishable && cfg.mode.publishable()
    );
    print!(",\"round\":{}", cfg.round);
    print!(",\"order_index\":{}", cfg.order_index);
    print!(
        ",\"fixture\":{{\"rows\":{},\"key_strategy\":\"deterministic-varied-mix64\",\"correctness_outside_timing\":true}}",
        cfg.fixture_rows
    );
    print!(
        ",\"settings\":{{\"point_read_ops\":{},\"write_ops\":{},\"scan_ops\":{},\"batch_size\":{},\"protected_regression_limit\":0.10,\"candidate_write_improvement_required\":0.25,\"candidate_point_read_improvement_required\":0.30}}",
        cfg.point_read_ops, cfg.write_ops, cfg.scan_ops, cfg.batch_size
    );
    print!(",\"workloads\":[");
    for (i, w) in report.workloads.iter().enumerate() {
        if i > 0 {
            print!(",");
        }
        print!(
            "{{\"name\":\"{}\",\"operations\":{},\"mean_ns_per_op\":{:.3},\"protected\":{},\"timed\":{}}}",
            json_escape(w.name),
            w.operations,
            w.mean_ns_per_op,
            w.protected,
            w.timed
        );
    }
    print!("],\"checks\":[");
    for (i, c) in report.checks.iter().enumerate() {
        if i > 0 {
            print!(",");
        }
        print!(
            "{{\"name\":\"{}\",\"passed\":{},\"detail\":\"{}\"}}",
            json_escape(c.name),
            c.passed,
            json_escape(&c.detail)
        );
    }
    println!("]}}");
}

fn print_json_field(name: &str, value: &str, comma: bool) {
    if comma {
        print!(",");
    }
    print!("\"{}\":\"{}\"", json_escape(name), json_escape(value));
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn main() {
    let config = match parse_args(std::env::args()) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    match run(config) {
        Ok(report) => {
            let ok = report.checks.iter().all(|c| c.passed);
            write_json(&report);
            if !ok {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varied_keys_are_bounded_deterministic_and_diverse() {
        let keys = varied_keys(1_000, 10_000, 0x51);
        assert_eq!(keys, varied_keys(1_000, 10_000, 0x51));
        assert!(keys.iter().all(|key| (0..10_000).contains(key)));
        let distinct = keys.iter().collect::<BTreeSet<_>>().len();
        assert!(distinct > 900, "distinct={distinct}");
    }

    #[test]
    fn changed_age_makes_repeated_key_updates_real_mutations() {
        let keys = varied_keys(200, 7, 0xA7);
        let mut final_ages = BTreeMap::new();
        for (i, id) in keys.iter().copied().enumerate() {
            final_ages.insert(id, changed_age(i));
        }
        for age in final_ages.values() {
            assert!(*age >= 10_000);
        }
        assert!(final_ages.len() <= 7);
        assert!(final_ages.values().collect::<BTreeSet<_>>().len() > 1);
    }

    #[test]
    fn report_settings_expose_evaluator_contract() {
        let config = Config {
            engine: EngineKind::Sqlite,
            mode: Mode::Off,
            fixture_rows: 20,
            point_read_ops: 5,
            write_ops: 5,
            scan_ops: 2,
            batch_size: 2,
            ..Config::default()
        };
        let report = run(config).expect("small sqlite diagnostic run");
        assert_eq!(report.config.profile, "value-v1");
        assert!(report
            .workloads
            .iter()
            .any(|w| w.name == "protected_scan_filter_count" && w.protected));
        assert!(
            report.checks.iter().all(|c| c.passed),
            "{:?}",
            report.checks
        );
        assert_eq!(report.sqlite_storage, "memory-diagnostic");
    }

    #[test]
    fn small_powdb_full_run_verifies_mutations_and_reopen() {
        let config = Config {
            engine: EngineKind::Powdb,
            mode: Mode::Full,
            fixture_rows: 12,
            point_read_ops: 4,
            write_ops: 4,
            scan_ops: 1,
            batch_size: 2,
            ..Config::default()
        };
        let report = run(config).expect("small powdb full run");
        assert!(
            report.checks.iter().all(|c| c.passed),
            "{:?}",
            report.checks
        );
        assert!(report.checks.iter().any(|c| c.name == "reopen_parity"));
        assert!(report.publishable);
    }

    #[test]
    fn cli_contract_parses_metadata_and_knobs() {
        let config = parse_args(
            [
                "powdb-compare-paired",
                "--engine",
                "powdb",
                "--mode=full",
                "--engine-ref",
                "candidate",
                "--engine-hash",
                "abc123",
                "--round",
                "4",
                "--order-index",
                "1",
                "--fixture-rows",
                "99",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("parse args");
        assert_eq!(config.engine, EngineKind::Powdb);
        assert_eq!(config.mode, Mode::Full);
        assert_eq!(config.engine_ref, "candidate");
        assert_eq!(config.engine_hash, "abc123");
        assert_eq!(config.round, 4);
        assert_eq!(config.order_index, 1);
        assert_eq!(config.fixture_rows, 99);
    }
}
