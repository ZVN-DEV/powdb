//! PowDB [`BenchEngine`] adapter for the Mission A wide bench.
//!
//! Wraps `powdb_query::executor::Engine` behind the uniform comparison
//! interface so all 15 Mission A workloads can run head-to-head against
//! SQLite / Postgres / MySQL.
//!
//! ## Design notes
//!
//! - The inner [`Engine`] lives inside a [`RefCell`] because `execute_powql`
//!   requires `&mut self` while the read methods on [`BenchEngine`] take
//!   `&self`. This is safe for single-threaded bench harnesses.
//! - **Every read goes through `execute_powql()`**, so every published number
//!   is reachable by a user typing PowQL. This is deliberate: two workload
//!   families used to bypass the front end (`point_lookup_indexed` walked the
//!   B-tree directly, and the four non-count aggregates hand-built a
//!   `PlanNode`), which inflated their ratios by the whole parse + plan cost
//!   that SQLite's comparator still paid. The parser gained field-bearing
//!   aggregates (`sum(User { .age })`, see `parser.rs`
//!   `test_parse_sum_with_field_projection`), so the fallback is gone.
//!   PowDB's plan cache with literal substitution is the fair counterpart to
//!   SQLite's `prepare_cached`, and both engines now pay their real front-end
//!   cost.
//! - Writes go through `execute_powql()` with one query string per call,
//!   with batched writes wrapped in a tight loop (PowQL has no batch-insert
//!   syntax today).

use std::cell::RefCell;

use powdb_query::ast::Literal;
use powdb_query::executor::{Engine, PreparedQuery};
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use powdb_storage::wal::WalSyncMode;
use tempfile::TempDir;

use super::{gen_row, BenchEngine};

/// Comparison-bench wrapper around the PowDB query engine.
pub struct PowdbEngine {
    engine: RefCell<Engine>,
    /// Mission C Phase 5: prepared insert statement reused across every
    /// `insert_batch` / `insert_single` row. SQLite's comparator uses
    /// `prepare_cached`; this is the fair equivalent on the PowDB side.
    /// Built lazily on first use because `setup()` may recreate the
    /// underlying engine and invalidate the template.
    insert_prep: RefCell<Option<PreparedQuery>>,
    /// Prepared update-by-pk. Two params: the pk literal (filter) and the
    /// new age (assignment).
    update_pk_prep: RefCell<Option<PreparedQuery>>,
    /// Keeps the temp directory alive for the lifetime of the engine.
    _tmp: TempDir,
}

impl PowdbEngine {
    pub fn new() -> Self {
        let tmp = TempDir::new().expect("create tempdir");
        let mut engine = Engine::new(tmp.path()).expect("engine init");
        // Mission B: SQLite uses `:memory:` (zero fsync); match by
        // disabling WAL fsync in the wide-bench harness so we measure
        // execute_powql throughput, not durability cost. See bench/
        // benches/powql.rs for the same toggle on the criterion side.
        engine.catalog_mut().set_wal_sync_mode(WalSyncMode::Off);
        PowdbEngine {
            engine: RefCell::new(engine),
            insert_prep: RefCell::new(None),
            update_pk_prep: RefCell::new(None),
            _tmp: tmp,
        }
    }

    /// Get (or lazily build) the prepared INSERT statement.
    ///
    /// The template values don't matter — they'll be overwritten on every
    /// `execute_prepared` call. The *shape* is what we're caching: six
    /// assignments in the order `(id, name, age, status, email, created_at)`.
    fn insert_prepared(&self) -> std::cell::RefMut<'_, PreparedQuery> {
        {
            let borrow = self.insert_prep.borrow();
            if borrow.is_some() {
                drop(borrow);
                return std::cell::RefMut::map(self.insert_prep.borrow_mut(), |o| {
                    o.as_mut().unwrap()
                });
            }
        }
        let prep = self.engine
            .borrow_mut()
            .prepare(
                r#"insert User { id := 0, name := "", age := 0, status := "", email := "", created_at := 0 }"#
            )
            .expect("prepare insert template");
        *self.insert_prep.borrow_mut() = Some(prep);
        std::cell::RefMut::map(self.insert_prep.borrow_mut(), |o| o.as_mut().unwrap())
    }

    /// Get (or lazily build) the prepared UPDATE-by-pk statement.
    fn update_pk_prepared(&self) -> std::cell::RefMut<'_, PreparedQuery> {
        {
            let borrow = self.update_pk_prep.borrow();
            if borrow.is_some() {
                drop(borrow);
                return std::cell::RefMut::map(self.update_pk_prep.borrow_mut(), |o| {
                    o.as_mut().unwrap()
                });
            }
        }
        let prep = self
            .engine
            .borrow_mut()
            .prepare("User filter .id = 0 update { age := 0 }")
            .expect("prepare update-by-pk template");
        *self.update_pk_prep.borrow_mut() = Some(prep);
        std::cell::RefMut::map(self.update_pk_prep.borrow_mut(), |o| o.as_mut().unwrap())
    }

    /// Run a PowQL read query and return the first row's first column as an
    /// `Option<String>`. Used by the two point-lookup workloads.
    fn powql_first_string(&self, query: &str) -> Option<String> {
        let result = self
            .engine
            .borrow_mut()
            .execute_powql(query)
            .expect("powql read failed");
        match result {
            QueryResult::Rows { mut rows, .. } => {
                if rows.is_empty() {
                    None
                } else {
                    match rows.remove(0).into_iter().next() {
                        Some(Value::Str(s)) => Some(s),
                        _ => None,
                    }
                }
            }
            _ => None,
        }
    }

    /// Run a PowQL query through the full lex → parse → plan → execute
    /// pipeline and return the raw result.
    ///
    /// Used by the aggregate workloads. These used to hand-build a
    /// `PlanNode::Aggregate` and call `execute_plan` directly, skipping the
    /// parser and planner that SQLite's comparator never got to skip. The
    /// parser now lifts a trailing single-field projection into the aggregate
    /// argument, so the honest query string works and is what we measure.
    fn powql_scalar(&self, query: &str) -> QueryResult {
        self.engine
            .borrow_mut()
            .execute_powql(query)
            .expect("aggregate query failed")
    }

    fn scalar_int(r: QueryResult) -> i64 {
        match r {
            QueryResult::Scalar(Value::Int(n)) => n,
            other => panic!("expected scalar int, got {other:?}"),
        }
    }

    fn scalar_float(r: QueryResult) -> f64 {
        match r {
            QueryResult::Scalar(Value::Float(f)) => f,
            QueryResult::Scalar(Value::Int(n)) => n as f64,
            other => panic!("expected scalar number, got {other:?}"),
        }
    }
}

impl Default for PowdbEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl BenchEngine for PowdbEngine {
    fn name(&self) -> &str {
        "powdb"
    }

    fn setup(&mut self, n_rows: usize) {
        // Idempotent: destructive workloads (`delete_by_filter`) call
        // `setup` repeatedly to rebuild the fixture. Because PowDB's heap
        // holds a write-through mmap that is frozen at `enable_mmap()`
        // time, the cleanest way to reset the fixture is to wipe the
        // entire engine + tempdir and start over. That also guarantees
        // we don't accidentally inherit stale indexes or half-deleted
        // rows from a prior iteration.
        if self.engine.get_mut().catalog().get_table("User").is_some() {
            let fresh_tmp = TempDir::new().expect("create tempdir");
            let mut fresh_engine = Engine::new(fresh_tmp.path()).expect("engine reset");
            // Re-apply the bench-only sync-off mode (see `new()`).
            fresh_engine
                .catalog_mut()
                .set_wal_sync_mode(WalSyncMode::Off);
            self.engine = RefCell::new(fresh_engine);
            // Mission C Phase 5: a fresh engine means a fresh catalog,
            // fresh schema, fresh plan cache. The cached prepared-plan
            // templates reference the *old* engine's parsed plan trees —
            // they're still structurally valid but logically stale, so
            // wipe them and let them be rebuilt lazily on the next call.
            *self.insert_prep.get_mut() = None;
            *self.update_pk_prep.get_mut() = None;
            self._tmp = fresh_tmp;
        }

        let engine = self.engine.get_mut();
        engine
            .execute_powql(
                "type User { \
                    required id: int, \
                    required name: str, \
                    required age: int, \
                    required status: str, \
                    required email: str, \
                    required created_at: int \
                }",
            )
            .expect("create type");

        let data_dir = self._tmp.path().to_path_buf();
        {
            let table = engine
                .catalog_mut()
                .get_table_mut("User")
                .expect("get User table");
            for i in 0..n_rows {
                let (id, name, age, status, email, created_at) = gen_row(i);
                let row = vec![
                    Value::Int(id),
                    Value::Str(name),
                    Value::Int(age),
                    Value::Str(status),
                    Value::Str(email),
                    Value::Int(created_at),
                ];
                table.insert(&row).expect("insert row");
            }

            // Preserve index on `id` — this is the only index for the
            // 15-workload run. `point_lookup_indexed` depends on it being
            // present; losing it silently turns the Project(IndexScan) fast
            // path into a linear scan and tanks the headline ratio.
            // `create_index` is safe to call again; the table rebuilds the
            // index from current row contents.
            table
                .create_index_with_unique("id", &data_dir, true)
                .expect("build id index");

            // Activate mmap for zero-syscall reads.
            table.heap.enable_mmap();
        }
    }

    // ── Reads ─────────────────────────────────────────────────────────

    fn point_lookup_indexed(&self, id: i64) -> Option<String> {
        // Goes through PowQL, like every other read here. This used to be a
        // raw `tbl.index("id").lookup_int(id)` B-tree probe, which skipped
        // the parse + plan work SQLite's `prepare_cached` comparator still
        // performed. The `id` index is still used: the planner emits an
        // IndexScan for an equality on an indexed column.
        let query = format!("User filter .id = {id} limit 1 {{ .name }}");
        self.powql_first_string(&query)
    }

    fn point_lookup_nonindexed(&self, created_at: i64) -> Option<String> {
        // Fair comparison with SQLite's `LIMIT 1` — stop scanning as soon
        // as the first (and usually only) match is found.
        let query = format!("User filter .created_at = {created_at} limit 1 {{ .name }}");
        self.powql_first_string(&query)
    }

    fn scan_filter_count(&self, age_threshold: i64) -> usize {
        let query = format!("count(User filter .age > {age_threshold})");
        let result = self
            .engine
            .borrow_mut()
            .execute_powql(&query)
            .expect("scan_filter_count query failed");
        match result {
            QueryResult::Scalar(Value::Int(n)) => n as usize,
            _ => 0,
        }
    }

    fn scan_filter_project_top100(&self, age_threshold: i64) -> Vec<(String, String)> {
        let query = format!("User filter .age > {age_threshold} limit 100 {{ .name, .email }}");
        let result = self
            .engine
            .borrow_mut()
            .execute_powql(&query)
            .expect("scan_filter_project_top100 query failed");
        match result {
            QueryResult::Rows { rows, .. } => rows
                .into_iter()
                .map(|mut r| {
                    let email = match r.pop() {
                        Some(Value::Str(s)) => s,
                        _ => String::new(),
                    };
                    let name = match r.pop() {
                        Some(Value::Str(s)) => s,
                        _ => String::new(),
                    };
                    (name, email)
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn scan_filter_sort_limit10(&self, age_threshold: i64) -> Vec<(String, i64)> {
        let query = format!(
            "User filter .age > {age_threshold} order .created_at desc limit 10 {{ .name, .created_at }}"
        );
        let result = self
            .engine
            .borrow_mut()
            .execute_powql(&query)
            .expect("scan_filter_sort_limit10 query failed");
        match result {
            QueryResult::Rows { rows, .. } => rows
                .into_iter()
                .map(|mut r| {
                    let created_at = match r.pop() {
                        Some(Value::Int(n)) => n,
                        _ => 0,
                    };
                    let name = match r.pop() {
                        Some(Value::Str(s)) => s,
                        _ => String::new(),
                    };
                    (name, created_at)
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn agg_sum(&self) -> i64 {
        Self::scalar_int(self.powql_scalar("sum(User { .age })"))
    }

    fn agg_avg(&self, age_threshold: i64) -> f64 {
        let query = format!("avg(User filter .age > {age_threshold} {{ .age }})");
        Self::scalar_float(self.powql_scalar(&query))
    }

    fn agg_min(&self) -> i64 {
        Self::scalar_int(self.powql_scalar("min(User { .created_at })"))
    }

    fn agg_max(&self) -> i64 {
        Self::scalar_int(self.powql_scalar("max(User { .age })"))
    }

    fn multi_col_and_filter(&self, age_threshold: i64, status: &str) -> Vec<(String, i64)> {
        // Note: PowQL string literals use double quotes. We rely on the
        // caller passing a status with no embedded quotes (always one of
        // the canonical `STATUSES`); no escaping is needed.
        let query = format!(
            "User filter .age > {age_threshold} and .status = \"{status}\" {{ .name, .age }}"
        );
        let result = self
            .engine
            .borrow_mut()
            .execute_powql(&query)
            .expect("multi_col_and_filter query failed");
        match result {
            QueryResult::Rows { rows, .. } => rows
                .into_iter()
                .map(|mut r| {
                    let age = match r.pop() {
                        Some(Value::Int(n)) => n,
                        _ => 0,
                    };
                    let name = match r.pop() {
                        Some(Value::Str(s)) => s,
                        _ => String::new(),
                    };
                    (name, age)
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    // ── Writes ────────────────────────────────────────────────────────

    fn insert_single(
        &mut self,
        id: i64,
        name: &str,
        age: i64,
        status: &str,
        email: &str,
        created_at: i64,
    ) {
        // Mission C Phase 5: prepared statement instead of `format!()` +
        // `execute_powql()`. Saves canonicalise + parse + plan cache
        // lookup on every call. SQLite's adapter uses `prepare_cached` —
        // this is the fair equivalent.
        let prep = self.insert_prepared();
        self.engine
            .borrow_mut()
            .execute_prepared(
                &prep,
                &[
                    Literal::Int(id),
                    Literal::String(name.to_string()),
                    Literal::Int(age),
                    Literal::String(status.to_string()),
                    Literal::String(email.to_string()),
                    Literal::Int(created_at),
                ],
            )
            .expect("insert_single failed");
    }

    fn insert_batch(&mut self, rows: &[(i64, String, i64, String, String, i64)]) {
        // Mission C Phase 5: batch over a single prepared statement. Every
        // row reuses the same template — zero lexing, zero parsing, zero
        // planning per row. Each call is still independent (PowDB writes
        // are already ACID per-call), which matches SQLite's `prepare_cached`
        // + `execute` pattern in the comparator.
        //
        // Mission C Phase 13: use the moving `execute_prepared_take`
        // variant so the three `String::clone()` calls inside PowDB's
        // `Literal → Value` conversion collapse into `mem::take`. The
        // `params` vec is reused across rows — only the string slots
        // get overwritten each iteration, and those strings are moved
        // into the row encoder on the next call.
        let prep = self.insert_prepared();
        let mut engine = self.engine.borrow_mut();
        let mut params: Vec<Literal> = vec![
            Literal::Int(0),
            Literal::String(String::new()),
            Literal::Int(0),
            Literal::String(String::new()),
            Literal::String(String::new()),
            Literal::Int(0),
        ];
        for (id, name, age, status, email, created_at) in rows {
            params[0] = Literal::Int(*id);
            params[1] = Literal::String(name.clone());
            params[2] = Literal::Int(*age);
            params[3] = Literal::String(status.clone());
            params[4] = Literal::String(email.clone());
            params[5] = Literal::Int(*created_at);
            engine
                .execute_prepared_take(&prep, &mut params)
                .expect("insert_batch row failed");
        }
    }

    fn update_by_pk(&mut self, id: i64, new_age: i64) -> u64 {
        // Mission C Phase 5: prepared statement. Combined with the Phase 4
        // in-place byte-patch fast path this makes update_by_pk nearly a
        // pure write — no parse, no plan, no row decode/encode.
        let prep = self.update_pk_prepared();
        let result = self
            .engine
            .borrow_mut()
            .execute_prepared(&prep, &[Literal::Int(id), Literal::Int(new_age)])
            .expect("update_by_pk failed");
        match result {
            QueryResult::Modified(n) => n,
            _ => 0,
        }
    }

    fn update_by_filter(&mut self, age_threshold: i64, new_status: &str) -> u64 {
        let query =
            format!("User filter .age > {age_threshold} update {{ status := \"{new_status}\" }}");
        let result = self
            .engine
            .get_mut()
            .execute_powql(&query)
            .expect("update_by_filter failed");
        match result {
            QueryResult::Modified(n) => n,
            _ => 0,
        }
    }

    fn delete_by_filter(&mut self, age_threshold: i64) -> u64 {
        let query = format!("User filter .age < {age_threshold} delete");
        let result = self
            .engine
            .get_mut()
            .execute_powql(&query)
            .expect("delete_by_filter failed");
        match result {
            QueryResult::Modified(n) => n,
            _ => 0,
        }
    }
}
