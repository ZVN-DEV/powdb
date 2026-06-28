//! PowDB query executor.

// Submodules that don't use macros defined in this file.
mod compiled;
mod eval;
pub mod mem_budget;

use crate::ast::*;
use crate::canonicalize::canonicalize;
use crate::plan::*;
use crate::plan_cache::PlanCache;
use crate::planner;
use crate::result::{QueryError, QueryResult};
use powdb_storage::catalog::Catalog;
use powdb_storage::row::{decode_column, decode_row, RowLayout, ROW_MAGIC, ROW_PREFIX_SIZE};
use powdb_storage::types::*;
use powdb_storage::view::ViewRegistry;
pub use powdb_storage::wal::WalSyncMode;

use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;
use tracing::{error, info, Level};

use self::compiled::*;
use self::eval::*;

/// Legacy sentinel string constant — kept for backward compatibility with
/// any external code matching on the string representation. New code should
/// match on `QueryError::ReadonlyNeedsWrite` directly.
pub const READONLY_NEEDS_WRITE: &str = "__POWDB_READONLY_NEEDS_WRITE__";

/// Return the byte offset where the row body starts.
///
/// v0.5 rows begin with the `PROW` magic/version prefix. Legacy rows start
/// directly with the row body. Raw executor fast paths must add this base
/// before reading body-relative bitmap/data offsets.
#[inline]
pub(crate) fn row_body_base(row: &[u8]) -> usize {
    if row.len() >= ROW_PREFIX_SIZE && &row[0..4] == ROW_MAGIC {
        ROW_PREFIX_SIZE
    } else {
        0
    }
}

/// Query frontend dialect. PowQL remains the default/native dialect; SQL is
/// an explicit frontend that lowers to the same AST before planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryDialect {
    PowQL,
    Sql,
}

/// Plan cache capacity. Bench workloads fill ~15 slots; real apps will sit
/// comfortably in 256. Lookup is O(1), collisions clear the cache (see
/// `plan_cache::PlanCache::insert`).
const PLAN_CACHE_CAPACITY: usize = 256;

/// Maximum number of rows a join may produce before the executor aborts.
/// Prevents Cartesian-product blowups (e.g. `T cross join T` on 10K rows
/// would produce 100M rows in memory without this cap).
pub(super) const MAX_JOIN_ROWS: usize = 1_000_000;

/// Maximum number of rows that may be materialized for sorting.
/// Queries that exceed this should add a LIMIT clause to narrow the input
/// before sorting.
pub(super) const MAX_SORT_ROWS: usize = 10_000_000;

#[inline]
pub(super) fn check_join_limit(row_count: usize) -> Result<(), QueryError> {
    if row_count > MAX_JOIN_ROWS {
        return Err(QueryError::JoinLimitExceeded);
    }
    Ok(())
}

// ─── Mission D11 Phase 1: scalar hot-loop helpers ─────────────────────────
//
// These macros expand into the scan body of `agg_single_col_fast` and sit
// inside the `for_each_row_raw` closure. They exist to:
//
//   1. Split the loop on presence of a predicate *outside* the hot body,
//      so the no-predicate path (agg_sum/agg_min/agg_max bench workloads)
//      never pays the `Option<CompiledPredicate>` branch per row.
//   2. Drop two bounds checks per row by reading the null bitmap byte
//      and the 8-byte value via raw pointer casts.
//
// SAFETY (shared across every call site below):
//
//   - `$bmp_byte` is `col_idx / 8` where `col_idx < n_cols`, and the row body
//     encoding stores `bitmap_size = n_cols.div_ceil(8)` bytes of bitmap
//     starting at body offset 2. So `bmp_off = row_body_base(row) + 2 +
//     $bmp_byte < row_len`, and `get_unchecked(bmp_off)` is inside the
//     row slice.
//   - `$off = 2 + bitmap_size + fixed_offsets[col_idx]` is body-relative for a fixed-size
//     column. Every fixed-size column contributes `fixed_size(type_id)`
//     bytes to the fixed region, so the row always has
//     `[data_off .. data_off + 8]` available for any i64/f64 column, where
//     `data_off = row_body_base(row) + $off` — enforced by the row encoder
//     (`storage/src/row.rs`) and the schema invariant that a row with a
//     given schema has enough body bytes for `2 + bitmap_size + fixed_region_size`.
//   - Both macros are only invoked from `agg_single_col_fast`, which
//     early-returns if the column isn't Int/Float (8-byte fixed) and
//     early-returns if `fast.fixed_offsets[col_idx]` is `None`.
macro_rules! agg_int_loop {
    (
        $self:expr, $table:expr, $pred:expr,
        $bmp_byte:expr, $bmp_bit:expr, $off:expr,
        |$v:ident : i64| $body:block
    ) => {{
        let bmp_byte = $bmp_byte;
        let bmp_bit = $bmp_bit;
        let off = $off;
        if let Some(pred) = &$pred {
            $self
                .catalog
                .for_each_row_raw($table, |_rid, data| {
                    if !pred(data) {
                        return;
                    }
                    let base = row_body_base(data);
                    let bmp_off = base + 2 + bmp_byte;
                    let data_off = base + off;
                    // Bounds guard: skip corrupt/truncated rows that are too
                    // short to contain the bitmap byte or the 8-byte value.
                    if bmp_off >= data.len() || data_off + 8 > data.len() {
                        return;
                    }
                    // SAFETY: `bmp_off < data.len()` is checked above.
                    // The bitmap byte lives at body offset 2..2+bitmap_size in
                    // the row encoding, and bmp_byte = col_idx / 8 < bitmap_size.
                    // Corrupt rows are rejected by the bounds guard.
                    let bmp = unsafe { *data.get_unchecked(bmp_off) };
                    if (bmp >> bmp_bit) & 1 == 1 {
                        return;
                    }
                    // SAFETY: `data_off + 8 <= data.len()` is checked above.
                    // `data_off = base + 2 + bitmap_size + fixed_offsets[col_idx]`
                    // points to an 8-byte i64 in the fixed-size region of the row.
                    // The pointer cast is valid because we read exactly 8
                    // bytes via from_le_bytes. Corrupt rows are rejected by
                    // the bounds guard.
                    let $v: i64 = unsafe {
                        i64::from_le_bytes(*(data.as_ptr().add(data_off) as *const [u8; 8]))
                    };
                    $body
                })
                .map_err(|e| QueryError::StorageError(e.to_string()))?;
        } else {
            $self
                .catalog
                .for_each_row_raw($table, |_rid, data| {
                    let base = row_body_base(data);
                    let bmp_off = base + 2 + bmp_byte;
                    let data_off = base + off;
                    // Bounds guard: skip corrupt/truncated rows.
                    if bmp_off >= data.len() || data_off + 8 > data.len() {
                        return;
                    }
                    // SAFETY: `bmp_off < data.len()` is checked above.
                    // See the predicate branch for the full invariant.
                    let bmp = unsafe { *data.get_unchecked(bmp_off) };
                    if (bmp >> bmp_bit) & 1 == 1 {
                        return;
                    }
                    // SAFETY: `data_off + 8 <= data.len()` is checked above.
                    // See the predicate branch for the full invariant.
                    let $v: i64 = unsafe {
                        i64::from_le_bytes(*(data.as_ptr().add(data_off) as *const [u8; 8]))
                    };
                    $body
                })
                .map_err(|e| QueryError::StorageError(e.to_string()))?;
        }
    }};
}

macro_rules! agg_float_loop {
    (
        $self:expr, $table:expr, $pred:expr,
        $bmp_byte:expr, $bmp_bit:expr, $off:expr,
        |$v:ident : f64| $body:block
    ) => {{
        let bmp_byte = $bmp_byte;
        let bmp_bit = $bmp_bit;
        let off = $off;
        if let Some(pred) = &$pred {
            $self
                .catalog
                .for_each_row_raw($table, |_rid, data| {
                    if !pred(data) {
                        return;
                    }
                    let base = row_body_base(data);
                    let bmp_off = base + 2 + bmp_byte;
                    let data_off = base + off;
                    // Bounds guard: skip corrupt/truncated rows that are too
                    // short to contain the bitmap byte or the 8-byte value.
                    if bmp_off >= data.len() || data_off + 8 > data.len() {
                        return;
                    }
                    // SAFETY: `bmp_off < data.len()` is checked above.
                    // The bitmap byte lives at body offset 2..2+bitmap_size in
                    // the row encoding, and bmp_byte = col_idx / 8 < bitmap_size.
                    // Corrupt rows are rejected by the bounds guard.
                    let bmp = unsafe { *data.get_unchecked(bmp_off) };
                    if (bmp >> bmp_bit) & 1 == 1 {
                        return;
                    }
                    // SAFETY: `data_off + 8 <= data.len()` is checked above.
                    // `data_off = base + 2 + bitmap_size + fixed_offsets[col_idx]`
                    // points to an 8-byte f64 in the fixed-size region of the row.
                    // The pointer cast is valid because we read exactly 8
                    // bytes via from_le_bytes. Corrupt rows are rejected by
                    // the bounds guard.
                    let $v: f64 = unsafe {
                        f64::from_le_bytes(*(data.as_ptr().add(data_off) as *const [u8; 8]))
                    };
                    $body
                })
                .map_err(|e| QueryError::StorageError(e.to_string()))?;
        } else {
            $self
                .catalog
                .for_each_row_raw($table, |_rid, data| {
                    let base = row_body_base(data);
                    let bmp_off = base + 2 + bmp_byte;
                    let data_off = base + off;
                    // Bounds guard: skip corrupt/truncated rows.
                    if bmp_off >= data.len() || data_off + 8 > data.len() {
                        return;
                    }
                    // SAFETY: `bmp_off < data.len()` is checked above.
                    // See the predicate branch for the full invariant.
                    let bmp = unsafe { *data.get_unchecked(bmp_off) };
                    if (bmp >> bmp_bit) & 1 == 1 {
                        return;
                    }
                    // SAFETY: `data_off + 8 <= data.len()` is checked above.
                    // See the predicate branch for the full invariant.
                    let $v: f64 = unsafe {
                        f64::from_le_bytes(*(data.as_ptr().add(data_off) as *const [u8; 8]))
                    };
                    $body
                })
                .map_err(|e| QueryError::StorageError(e.to_string()))?;
        }
    }};
}

// Submodules that use the macros above — must be declared after macro_rules!.
mod plan_exec;
mod prepared;

#[cfg(test)]
mod tests;

// Re-exports for the public API
pub use self::prepared::PreparedQuery;

use self::plan_exec::{
    compute_group_aggregate, execute_window, format_plan_tree, hash_join, lower_unindexed_scans,
    range_matches, synthesize_range_predicate, try_extract_equi_join_keys,
};

/// Mission infra-1: classify a parsed statement as read-only vs. mutating.
/// Used by [`Engine::execute_powql_readonly`] and by the server handler
/// to decide between the RwLock reader and writer sides. `Union` recurses
/// because each side can independently be read/write (though in practice
/// both sides are reads — the parser only builds Union from query shapes).
pub fn is_read_only_statement(stmt: &Statement) -> bool {
    match stmt {
        Statement::Query(_) => true,
        Statement::Union(u) => is_read_only_statement(&u.left) && is_read_only_statement(&u.right),
        Statement::Insert(_)
        | Statement::Upsert(_)
        | Statement::UpdateQuery(_)
        | Statement::DeleteQuery(_)
        | Statement::CreateType(_)
        | Statement::AlterTable(_)
        | Statement::DropTable(_)
        | Statement::CreateView(_)
        | Statement::RefreshView(_)
        | Statement::DropView(_) => false,
        Statement::Begin | Statement::Commit | Statement::Rollback => false,
        Statement::Explain(inner) => is_read_only_statement(inner),
    }
}

pub struct Engine {
    catalog: Catalog,
    /// Exclusive PID-based lock on the data directory, held for the engine's
    /// lifetime so two separate processes can't open the same dir and corrupt
    /// the heap/WAL. Released on clean drop; a `mem::forget` crash leaves a
    /// stale lock the next open takes over. Leading `_`: it does its work
    /// through `Drop`, never read directly.
    _dir_lock: powdb_storage::dir_lock::DirLock,
    /// Mission D9 — cached parsed+planned query trees keyed by canonical
    /// hash. Saves the ~3μs parse+plan cost on repeat queries that differ
    /// only in literal values.
    ///
    /// Mission infra-1: wrapped in `Mutex` so the read path can be driven
    /// by `&self`. The critical section is extremely short — a single
    /// hashmap lookup + plan clone on a hit, or a single insert on a miss.
    /// A full `RwLock` would be over-engineered here; the contention window
    /// is smaller than the read-path scan work it gates.
    plan_cache: Mutex<PlanCache>,
    /// Mission C Phase 13: reusable `Vec<Value>` scratch buffer for the
    /// prepared-insert fast path. `execute_prepared` used to allocate a
    /// fresh `vec![Value::Empty; n_cols]` on every insert; recycling this
    /// buffer shaves one heap alloc per row on `insert_batch_1k`.
    insert_values_scratch: Vec<Value>,
    /// Materialized view registry: tracks view definitions, dependencies,
    /// and dirty state. Views are backed by regular catalog tables; this
    /// registry adds the lifecycle metadata.
    view_registry: ViewRegistry,
    in_transaction: bool,
    /// WS2 — per-query memory budget ceiling (bytes). The running total lives
    /// in a thread-local (see [`mem_budget`]) and is reset at every top-level
    /// query entry, so sort/join/GROUP BY/IN-list materialization can be capped
    /// without OOM-killing the process. This field holds only the *limit* (a
    /// plain `usize`, so `Engine` stays `Sync` for the concurrent read path).
    /// Default [`mem_budget::DEFAULT_QUERY_MEMORY_LIMIT`] (256 MB); overridable
    /// via `Engine::with_memory_limit` (server reads `POWDB_QUERY_MEMORY_LIMIT`).
    query_memory_limit: usize,
}

impl Engine {
    /// Open or create a PowDB engine rooted at `data_dir`.
    ///
    /// If the directory already contains a catalog, it is reopened.
    /// Otherwise a fresh empty database is created.
    ///
    /// # Examples
    ///
    /// ```
    /// use powdb_query::executor::Engine;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let engine = Engine::new(dir.path()).unwrap();
    /// // Engine is ready — the directory now contains a catalog.
    /// ```
    pub fn new(data_dir: &Path) -> io::Result<Self> {
        powdb_storage::create_data_dir_secure(data_dir)?;
        // Refuse to open a directory another live process already holds, before
        // touching any on-disk state (concurrent writers corrupt the heap/WAL).
        let dir_lock = powdb_storage::dir_lock::DirLock::acquire(data_dir)?;
        // Try to reopen an existing database first; only create a fresh
        // catalog when there isn't one already on disk.
        let catalog = match Catalog::open(data_dir) {
            Ok(c) => {
                info!(data_dir = %data_dir.display(), "engine reopened existing database");
                c
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                info!(data_dir = %data_dir.display(), "engine initialized fresh database");
                Catalog::create(data_dir)?
            }
            Err(e) => return Err(e),
        };
        let view_registry =
            ViewRegistry::open(data_dir).unwrap_or_else(|_| ViewRegistry::new(data_dir));
        Ok(Engine {
            catalog,
            _dir_lock: dir_lock,
            plan_cache: Mutex::new(PlanCache::new(PLAN_CACHE_CAPACITY)),
            insert_values_scratch: Vec::new(),
            view_registry,
            in_transaction: false,
            query_memory_limit: mem_budget::DEFAULT_QUERY_MEMORY_LIMIT,
        })
    }

    /// Open or create an engine with an explicit per-query memory limit
    /// (bytes). Used by the server to apply `POWDB_QUERY_MEMORY_LIMIT`, and by
    /// tests that need a tiny limit to exercise the budget guard.
    pub fn with_memory_limit(data_dir: &Path, limit_bytes: usize) -> io::Result<Self> {
        let mut engine = Engine::new(data_dir)?;
        engine.set_query_memory_limit(limit_bytes);
        Ok(engine)
    }

    /// Current per-query memory limit in bytes.
    pub fn query_memory_limit(&self) -> usize {
        self.query_memory_limit
    }

    /// Override the per-query memory limit in bytes (builder-style).
    pub fn set_query_memory_limit(&mut self, limit_bytes: usize) {
        self.query_memory_limit = limit_bytes;
    }

    /// Set the WAL durability mode (see [`WalSyncMode`]). `Full` (the default)
    /// fsyncs every commit; `Normal` moves the fsync to a background flusher
    /// with a bounded crash-loss window; `Off` is bench-only (no durability).
    /// Wired from the server's `POWDB_SYNC_MODE` / `--sync-mode` config.
    pub fn set_wal_sync_mode(&mut self, mode: WalSyncMode) {
        self.catalog.set_wal_sync_mode(mode);
    }

    /// Enter a budgeted-statement frame for the current query. The returned
    /// guard must be held for the duration of the statement; on its drop the
    /// reentrancy depth is decremented. Only the *outermost* statement entry
    /// zeroes this thread's running total, so a nested `execute_powql` (the
    /// source query of a `create_view`/`refresh_view`) does NOT discard the
    /// outer frame's accounting. The accumulator is thread-local, so this never
    /// touches another concurrent query's total.
    #[must_use = "the budget guard must outlive the statement body"]
    pub(super) fn enter_memory_budget(&self) -> mem_budget::EnterGuard {
        mem_budget::enter()
    }

    /// Charge the estimated footprint of a freshly materialized batch of rows
    /// against the current per-query budget. Returns
    /// [`QueryError::MemoryLimitExceeded`] cleanly if the batch would push the
    /// query over its limit. Used at every full-materialization point (sort
    /// buffer, join build side, GROUP BY hash table, IN-list).
    pub(super) fn charge_rows(&self, rows: &[Vec<Value>]) -> Result<(), QueryError> {
        let mut total = 0usize;
        for row in rows {
            total = total.saturating_add(mem_budget::estimate_row_size(row));
        }
        mem_budget::charge(total, self.query_memory_limit)
    }

    /// Charge a materialized IN-list (the literal expressions pulled out of an
    /// uncorrelated `IN (subquery)`) against the current per-query budget.
    /// Each item is conservatively sized at the `Expr` slot plus, for string
    /// literals, the owned heap bytes.
    pub(super) fn charge_in_list(&self, list: &[crate::ast::Expr]) -> Result<(), QueryError> {
        let base = std::mem::size_of::<crate::ast::Expr>();
        let mut total = std::mem::size_of::<Vec<crate::ast::Expr>>();
        for item in list {
            total = total.saturating_add(base);
            if let crate::ast::Expr::Literal(crate::ast::Literal::String(s)) = item {
                total = total.saturating_add(s.capacity());
            }
        }
        mem_budget::charge(total, self.query_memory_limit)
    }

    /// Dispatch to the requested query frontend.
    pub fn execute_with_dialect(
        &mut self,
        dialect: QueryDialect,
        input: &str,
    ) -> Result<QueryResult, QueryError> {
        match dialect {
            QueryDialect::PowQL => self.execute_powql(input),
            QueryDialect::Sql => self.execute_sql(input),
        }
    }

    /// Read-only variant of [`Engine::execute_with_dialect`].
    pub fn execute_readonly_with_dialect(
        &self,
        dialect: QueryDialect,
        input: &str,
    ) -> Result<QueryResult, QueryError> {
        match dialect {
            QueryDialect::PowQL => self.execute_powql_readonly(input),
            QueryDialect::Sql => self.execute_sql_readonly(input),
        }
    }

    /// Parse + plan + execute a PowQL query.
    ///
    /// # Examples
    ///
    /// ```
    /// use powdb_query::executor::Engine;
    /// use powdb_query::result::QueryResult;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let mut engine = Engine::new(dir.path()).unwrap();
    ///
    /// // Create a table and insert a row.
    /// engine.execute_powql("type User { required name: str, age: int }").unwrap();
    /// engine.execute_powql(r#"insert User { name := "Alice", age := 30 }"#).unwrap();
    ///
    /// // Query rows back.
    /// let result = engine.execute_powql("User").unwrap();
    /// assert_eq!(result.row_count(), 1);
    /// ```
    ///
    /// Mission D6 — tracing collapse: the previous implementation ran 4
    /// `Instant::now()` + 3 `elapsed().as_micros()` calls + formatted an
    /// `info!` span on every query, even when tracing was disabled. On a
    /// sub-microsecond `point_lookup_indexed` call that overhead was
    /// 100-200ns — 20%+ of the whole query. We now measure time only when
    /// INFO is actually enabled via `tracing::enabled!`, and we moved the
    /// noisy `debug!(?plan)` line behind the same gate so the Debug
    /// formatter can't run unconditionally either.
    ///
    /// Mission D9 — plan cache: on the hot path we canonicalise the query
    /// text (lex + FNV-1a hash with literal values stripped), check the
    /// cache, and on a hit substitute the new literals into a clone of the
    /// cached plan. This skips re-lexing, re-parsing, and re-planning —
    /// around 3μs per call on bench workloads. On a miss we plan as before
    /// and insert the plan under its canonical hash.
    pub fn execute_powql(&mut self, input: &str) -> Result<QueryResult, QueryError> {
        // WS2: each *outermost* statement starts with the full memory
        // allowance. The guard holds the reentrancy depth so a nested
        // `execute_powql` (e.g. a view's source query) does not reset the
        // outer frame's accounting mid-statement.
        let _budget = self.enter_memory_budget();
        // Hot path: tracing disabled. Zero syscalls, zero formatting.
        if !tracing::enabled!(Level::INFO) {
            // D9: try the plan cache first. Canonicalisation lexes the
            // query once; on a hit we skip the parser and planner entirely.
            if let Ok((hash, literals)) = canonicalize(input) {
                let cached = self
                    .plan_cache
                    .lock()
                    .map_err(|e| QueryError::Execution(format!("plan cache lock poisoned: {e}")))?
                    .get_with_substitution(hash, &literals);
                if let Some(plan) = cached {
                    let plan = lower_unindexed_scans(&self.catalog, &plan);
                    let result = self.execute_plan(&plan);
                    // Mission B (post-review): statement-boundary WAL
                    // group commit. Catalog::wal_log now only appends;
                    // the fsync happens here exactly once per statement.
                    // `sync_wal` is a no-op when nothing was buffered
                    // (pure reads pay zero fsync).
                    if !self.in_transaction {
                        self.catalog
                            .commit_autocommit()
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    }
                    return result;
                }
                // Miss — plan, insert, execute.
                return match planner::plan(input) {
                    Ok(plan) => {
                        self.plan_cache
                            .lock()
                            .map_err(|e| {
                                QueryError::Execution(format!("plan cache lock poisoned: {e}"))
                            })?
                            .insert(hash, plan.clone());
                        let plan = lower_unindexed_scans(&self.catalog, &plan);
                        let result = self.execute_plan(&plan);
                        if !self.in_transaction {
                            self.catalog
                                .commit_autocommit()
                                .map_err(|e| QueryError::StorageError(e.to_string()))?;
                        }
                        result
                    }
                    Err(e) => Err(QueryError::Parse(e.to_string())),
                };
            }
            // Lex error — fall through to the planner so the caller gets a
            // consistent error shape.
            return match planner::plan(input) {
                Ok(plan) => {
                    let plan = lower_unindexed_scans(&self.catalog, &plan);
                    let result = self.execute_plan(&plan);
                    if !self.in_transaction {
                        self.catalog
                            .commit_autocommit()
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    }
                    result
                }
                Err(e) => Err(QueryError::Parse(e.to_string())),
            };
        }

        // Instrumented path — only taken under explicit tracing subscribers.
        let total_start = Instant::now();
        let plan_start = Instant::now();
        let plan = planner::plan(input).map_err(|e| {
            let msg = e.to_string();
            error!(query = %input, error = %msg, "query plan failed");
            QueryError::Parse(msg)
        })?;
        let plan_us = plan_start.elapsed().as_micros();

        let exec_start = Instant::now();
        let plan = lower_unindexed_scans(&self.catalog, &plan);
        let result = self.execute_plan(&plan);
        if !self.in_transaction {
            self.catalog
                .commit_autocommit()
                .map_err(|e| QueryError::StorageError(e.to_string()))?;
        }
        let exec_us = exec_start.elapsed().as_micros();

        let total_us = total_start.elapsed().as_micros();
        match &result {
            Ok(r) => {
                info!(
                    query = %input,
                    plan_us = plan_us,
                    exec_us = exec_us,
                    total_us = total_us,
                    rows = r.row_count(),
                    "query ok"
                );
            }
            Err(e) => {
                error!(
                    query = %input,
                    plan_us = plan_us,
                    exec_us = exec_us,
                    error = %e,
                    "query failed"
                );
            }
        }
        result
    }

    /// Parse + plan + execute a SQL query through the SQL frontend.
    ///
    /// SQL is lowered to the existing PowDB AST and to canonical PowQL text.
    /// The canonical PowQL text is used as the plan-cache key, so equivalent
    /// SQL and PowQL spellings share cached plans.
    pub fn execute_sql(&mut self, input: &str) -> Result<QueryResult, QueryError> {
        let _budget = self.enter_memory_budget();
        let parsed = crate::sql::parse_sql_with_canonical(input)
            .map_err(|e| QueryError::Parse(e.to_string()))?;

        if !tracing::enabled!(Level::INFO) {
            if let Ok((hash, literals)) = canonicalize(&parsed.canonical_powql) {
                let cached = self
                    .plan_cache
                    .lock()
                    .map_err(|e| QueryError::Execution(format!("plan cache lock poisoned: {e}")))?
                    .get_with_substitution(hash, &literals);
                if let Some(plan) = cached {
                    let plan = lower_unindexed_scans(&self.catalog, &plan);
                    let result = self.execute_plan(&plan);
                    if !self.in_transaction {
                        self.catalog
                            .commit_autocommit()
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    }
                    return result;
                }

                let plan = crate::planner::plan_statement(parsed.statement)
                    .map_err(|e| QueryError::Parse(e.to_string()))?;
                self.plan_cache
                    .lock()
                    .map_err(|e| QueryError::Execution(format!("plan cache lock poisoned: {e}")))?
                    .insert(hash, plan.clone());
                let plan = lower_unindexed_scans(&self.catalog, &plan);
                let result = self.execute_plan(&plan);
                if !self.in_transaction {
                    self.catalog
                        .commit_autocommit()
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                }
                return result;
            }
        }

        let plan = crate::planner::plan_statement(parsed.statement)
            .map_err(|e| QueryError::Parse(e.to_string()))?;
        let plan = lower_unindexed_scans(&self.catalog, &plan);
        let result = self.execute_plan(&plan);
        if !self.in_transaction {
            self.catalog
                .commit_autocommit()
                .map_err(|e| QueryError::StorageError(e.to_string()))?;
        }
        result
    }

    /// Read-only variant of [`Engine::execute_sql`].
    pub fn execute_sql_readonly(&self, input: &str) -> Result<QueryResult, QueryError> {
        let _budget = self.enter_memory_budget();
        let parsed = crate::sql::parse_sql_with_canonical(input)
            .map_err(|e| QueryError::Parse(e.to_string()))?;
        if !is_read_only_statement(&parsed.statement) {
            return Err(QueryError::ReadonlyNeedsWrite);
        }

        if let Ok((hash, literals)) = canonicalize(&parsed.canonical_powql) {
            let cached = self
                .plan_cache
                .lock()
                .map_err(|e| QueryError::Execution(format!("plan cache lock poisoned: {e}")))?
                .get_with_substitution(hash, &literals);
            if let Some(plan) = cached {
                let plan = lower_unindexed_scans(&self.catalog, &plan);
                return self.execute_plan_readonly(&plan);
            }
            let plan = crate::planner::plan_statement(parsed.statement)
                .map_err(|e| QueryError::Parse(e.to_string()))?;
            self.plan_cache
                .lock()
                .map_err(|e| QueryError::Execution(format!("plan cache lock poisoned: {e}")))?
                .insert(hash, plan.clone());
            let plan = lower_unindexed_scans(&self.catalog, &plan);
            return self.execute_plan_readonly(&plan);
        }

        let plan = crate::planner::plan_statement(parsed.statement)
            .map_err(|e| QueryError::Parse(e.to_string()))?;
        let plan = lower_unindexed_scans(&self.catalog, &plan);
        self.execute_plan_readonly(&plan)
    }

    /// Execute PowQL with `$N` placeholders bound to positional `params`.
    ///
    /// Task 4: parameters are substituted as literal *tokens* before
    /// parsing (see [`crate::parser::parse_with_params`]), so untrusted
    /// input can never change the query's shape. This path deliberately
    /// **bypasses the plan cache** — template caching is a follow-up — and
    /// otherwise mirrors the non-cached tail of [`Engine::execute_powql`].
    pub fn execute_powql_with_params(
        &mut self,
        input: &str,
        params: &[crate::ast::ParamValue],
    ) -> Result<QueryResult, QueryError> {
        let _budget = self.enter_memory_budget();
        let stmt = crate::parser::parse_with_params(input, params)
            .map_err(|e| QueryError::Parse(e.to_string()))?;
        let plan =
            crate::planner::plan_statement(stmt).map_err(|e| QueryError::Parse(e.to_string()))?;
        let plan = lower_unindexed_scans(&self.catalog, &plan);
        let result = self.execute_plan(&plan);
        if !self.in_transaction {
            self.catalog
                .commit_autocommit()
                .map_err(|e| QueryError::StorageError(e.to_string()))?;
        }
        result
    }

    /// Read-only variant of [`Engine::execute_powql_with_params`].
    ///
    /// Mirrors [`Engine::execute_powql_readonly`]: parses with bound
    /// params, rejects any write statement with
    /// [`QueryError::ReadonlyNeedsWrite`] so the caller can escalate to the
    /// write lock, then executes under a shared borrow. No plan-cache
    /// interaction.
    pub fn execute_powql_readonly_with_params(
        &self,
        input: &str,
        params: &[crate::ast::ParamValue],
    ) -> Result<QueryResult, QueryError> {
        let _budget = self.enter_memory_budget();
        let stmt = crate::parser::parse_with_params(input, params)
            .map_err(|e| QueryError::Parse(e.to_string()))?;
        if !is_read_only_statement(&stmt) {
            return Err(QueryError::ReadonlyNeedsWrite);
        }
        let plan =
            crate::planner::plan_statement(stmt).map_err(|e| QueryError::Parse(e.to_string()))?;
        let plan = lower_unindexed_scans(&self.catalog, &plan);
        self.execute_plan_readonly(&plan)
    }

    /// Plan cache stats — useful for benches and debugging.
    pub fn plan_cache_stats(&self) -> (u64, u64, usize) {
        let cache = self.plan_cache.lock().unwrap_or_else(|e| e.into_inner());
        (cache.hits, cache.misses, cache.len())
    }

    /// Mission infra-1: read-only entry point.
    ///
    /// Parses + plans + executes a PowQL query using only a shared borrow
    /// on the engine. Rejects any statement that would mutate state
    /// (Insert/Update/Delete/CreateTable/AlterTable/DropTable/CreateView/
    /// RefreshView/DropView) by returning [`READONLY_NEEDS_WRITE`] so the
    /// caller can escalate to the write lock.
    ///
    /// Also returns [`READONLY_NEEDS_WRITE`] if a materialized view in the
    /// query is dirty — refreshing one requires `&mut self`, so the caller
    /// must retake the write lock for the first refresh.
    ///
    /// This method is the concurrent-read fast path behind
    /// `Arc<RwLock<Engine>>`: multiple threads can call it simultaneously
    /// under a shared `.read()` lock and each will scan independently.
    pub fn execute_powql_readonly(&self, input: &str) -> Result<QueryResult, QueryError> {
        // WS2: each *outermost* statement starts with the full memory
        // allowance. The guard holds the reentrancy depth so a nested
        // `execute_powql*` does not reset the outer frame's accounting.
        let _budget = self.enter_memory_budget();
        // Parse the statement first so we can classify read vs. write
        // without touching the catalog. This is the same lex+parse cost
        // the hot path would pay anyway.
        let stmt = crate::parser::parse(input).map_err(|e| QueryError::Parse(e.to_string()))?;
        if !is_read_only_statement(&stmt) {
            return Err(QueryError::ReadonlyNeedsWrite);
        }

        // Try the plan cache first — identical hash scheme to
        // `execute_powql` so both paths share cache state. The mutex
        // section is just a hashmap lookup + plan clone.
        if let Ok((hash, literals)) = canonicalize(input) {
            let cached = self
                .plan_cache
                .lock()
                .map_err(|e| QueryError::Execution(format!("plan cache lock poisoned: {e}")))?
                .get_with_substitution(hash, &literals);
            if let Some(plan) = cached {
                let plan = lower_unindexed_scans(&self.catalog, &plan);
                return self.execute_plan_readonly(&plan);
            }
            // Miss: plan + insert + execute. The planner is pure, so this
            // is safe from `&self`.
            let plan = crate::planner::plan_statement(stmt)
                .map_err(|e| QueryError::Parse(e.to_string()))?;
            self.plan_cache
                .lock()
                .map_err(|e| QueryError::Execution(format!("plan cache lock poisoned: {e}")))?
                .insert(hash, plan.clone());
            let plan = lower_unindexed_scans(&self.catalog, &plan);
            return self.execute_plan_readonly(&plan);
        }
        // Lex error — fall through to the planner for a consistent error
        // shape (though `parse` above would usually have caught it).
        let plan =
            crate::planner::plan_statement(stmt).map_err(|e| QueryError::Parse(e.to_string()))?;
        let plan = lower_unindexed_scans(&self.catalog, &plan);
        self.execute_plan_readonly(&plan)
    }

    /// Read-only version of [`Engine::execute_plan`]. Dispatches the
    /// read-path plan variants by calling `&self` helpers and errors with
    /// [`READONLY_NEEDS_WRITE`] on any write variant. This is the
    /// recursion target for composite read plans under the RwLock reader.
    ///
    /// The dispatch mirrors `execute_plan` for the read branches but does
    /// not carry any of the fast-paths that need `&mut self` (e.g. plan-
    /// cache mutation on inner subqueries is handled via the shared mutex
    /// in [`Engine::execute_powql_readonly`]; in-flight subquery
    /// materialisation uses [`Engine::materialize_subqueries_readonly`]).
    fn execute_plan_readonly(&self, plan: &PlanNode) -> Result<QueryResult, QueryError> {
        match plan {
            PlanNode::SeqScan { table } => {
                // Dirty view means we'd need to refresh it — can't do that
                // under `&self`. Escalate to the write path.
                if self.view_registry.is_dirty(table) {
                    return Err(QueryError::ReadonlyNeedsWrite);
                }
                let schema = self
                    .catalog
                    .schema(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.clone()))?
                    .clone();
                let columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
                let rows: Vec<Vec<Value>> = self
                    .catalog
                    .scan(table)
                    .map_err(|e| e.to_string())?
                    .map(|(_, row)| row)
                    .collect();
                Ok(QueryResult::Rows { columns, rows })
            }

            PlanNode::AliasScan { table, alias } => {
                let schema = self
                    .catalog
                    .schema(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.clone()))?
                    .clone();
                let columns: Vec<String> = schema
                    .columns
                    .iter()
                    .map(|c| format!("{alias}.{}", c.name))
                    .collect();
                let rows: Vec<Vec<Value>> = self
                    .catalog
                    .scan(table)
                    .map_err(|e| e.to_string())?
                    .map(|(_, row)| row)
                    .collect();
                Ok(QueryResult::Rows { columns, rows })
            }

            PlanNode::IndexScan { table, column, key } => {
                let schema = self
                    .catalog
                    .schema(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.clone()))?
                    .clone();
                let columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
                let key_value = literal_to_value(key)?;
                let tbl = self
                    .catalog
                    .get_table(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.clone()))?;

                if tbl.has_index(column) {
                    // Use index_lookup_all to handle both unique and
                    // non-unique indexes — returns all matching RowIds.
                    let rids = tbl.index_lookup_all(column, &key_value);
                    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(rids.len());
                    for rid in rids {
                        if let Some(data) = tbl.heap.get(rid) {
                            rows.push(decode_row(&tbl.schema, &data));
                        }
                    }
                    return Ok(QueryResult::Rows { columns, rows });
                }

                // No index: synthetic eq predicate + compiled scan.
                let fast = FastLayout::new(&schema);
                let synth_pred = Expr::BinaryOp(
                    Box::new(Expr::Field(column.clone())),
                    BinOp::Eq,
                    Box::new(key.clone()),
                );
                if let Some(compiled) = compile_predicate(&synth_pred, &columns, &fast, &schema) {
                    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(64);
                    self.catalog
                        .for_each_row_raw(table, |_rid, data| {
                            if compiled(data) {
                                rows.push(decode_row(&schema, data));
                            }
                        })
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    return Ok(QueryResult::Rows { columns, rows });
                }

                // Last resort: slow eq-check.
                let col_idx =
                    schema
                        .column_index(column)
                        .ok_or_else(|| QueryError::ColumnNotFound {
                            table: String::new(),
                            column: column.clone(),
                        })?;
                let rows: Vec<Vec<Value>> = tbl
                    .scan()
                    .filter_map(|(_, row)| {
                        if row[col_idx] == key_value {
                            Some(row)
                        } else {
                            None
                        }
                    })
                    .collect();
                Ok(QueryResult::Rows { columns, rows })
            }

            PlanNode::RangeScan {
                table,
                column,
                start,
                end,
            } => {
                let tbl = self
                    .catalog
                    .get_table(table)
                    .ok_or_else(|| QueryError::TableNotFound(table.clone()))?;
                let columns: Vec<String> =
                    tbl.schema.columns.iter().map(|c| c.name.clone()).collect();
                let schema = tbl.schema.clone();

                let start_val = match start {
                    Some((expr, _)) => Some(literal_to_value(expr)?),
                    None => None,
                };
                let end_val = match end {
                    Some((expr, _)) => Some(literal_to_value(expr)?),
                    None => None,
                };
                let start_inclusive = start.as_ref().map(|(_, inc)| *inc).unwrap_or(true);
                let end_inclusive = end.as_ref().map(|(_, inc)| *inc).unwrap_or(true);

                // Range scans only use the btree fast path for unique indexes.
                // Non-unique indexes store composite keys that don't compare
                // directly against raw column values.
                if tbl.is_index_unique(column) == Some(true) {
                    if let Some(btree) = tbl.index(column) {
                        let hits: Vec<(Value, RowId)> = match (&start_val, &end_val) {
                            (Some(s), Some(e)) => btree.range(s, e).collect(),
                            (Some(s), None) => btree.range_from(s),
                            (None, Some(e)) => btree.range_to(e),
                            (None, None) => {
                                // Unbounded both sides — equivalent to seq scan.
                                let rows: Vec<Vec<Value>> =
                                    tbl.scan().map(|(_, row)| row).collect();
                                return Ok(QueryResult::Rows { columns, rows });
                            }
                        };
                        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(hits.len());
                        for (key, rid) in hits {
                            // Filter for exclusive bounds.
                            if !start_inclusive {
                                if let Some(ref s) = start_val {
                                    if &key == s {
                                        continue;
                                    }
                                }
                            }
                            if !end_inclusive {
                                if let Some(ref e) = end_val {
                                    if &key == e {
                                        continue;
                                    }
                                }
                            }
                            if let Some(data) = tbl.heap.get(rid) {
                                rows.push(decode_row(&schema, &data));
                            }
                        }
                        return Ok(QueryResult::Rows { columns, rows });
                    }
                }

                // Fallback: no index — synthesize the range predicate and scan.
                let fast = FastLayout::new(&schema);
                let synth = synthesize_range_predicate(column, start, end);
                if let Some(compiled) = compile_predicate(&synth, &columns, &fast, &schema) {
                    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(64);
                    self.catalog
                        .for_each_row_raw(table, |_rid, data| {
                            if compiled(data) {
                                rows.push(decode_row(&schema, data));
                            }
                        })
                        .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    return Ok(QueryResult::Rows { columns, rows });
                }

                // Last resort: decoded row eval.
                let col_idx =
                    schema
                        .column_index(column)
                        .ok_or_else(|| QueryError::ColumnNotFound {
                            table: String::new(),
                            column: column.clone(),
                        })?;
                let rows: Vec<Vec<Value>> = tbl
                    .scan()
                    .filter(|(_, row)| {
                        range_matches(
                            &row[col_idx],
                            &start_val,
                            start_inclusive,
                            &end_val,
                            end_inclusive,
                        )
                    })
                    .map(|(_, row)| row)
                    .collect();
                Ok(QueryResult::Rows { columns, rows })
            }

            PlanNode::Filter { input, predicate } => {
                // Materialise subqueries using the `&self` variant.
                // Uncorrelated subqueries are replaced with InList/Bool;
                // correlated ones are left as InSubquery/ExistsSubquery
                // for per-row materialisation below.
                let materialized;
                let predicate = if contains_subquery(predicate) {
                    materialized = self.materialize_subqueries_readonly(predicate)?;
                    &materialized
                } else {
                    predicate
                };

                // Correlated subquery path: per-row materialisation.
                if contains_subquery(predicate) {
                    let result = self.execute_plan_readonly(input)?;
                    return match result {
                        QueryResult::Rows { columns, rows } => {
                            let mut filtered = Vec::new();
                            for row in rows {
                                let row_pred = self.materialize_correlated_for_row_readonly(
                                    predicate, &row, &columns,
                                )?;
                                if eval_predicate(&row_pred, &row, &columns) {
                                    filtered.push(row);
                                }
                            }
                            Ok(QueryResult::Rows {
                                columns,
                                rows: filtered,
                            })
                        }
                        _ => Err("filter requires row input".into()),
                    };
                }

                // Fused Filter+SeqScan fast path.
                if let PlanNode::SeqScan { table } = input.as_ref() {
                    if self.view_registry.is_dirty(table) {
                        return Err(QueryError::ReadonlyNeedsWrite);
                    }
                    let schema = self
                        .catalog
                        .schema(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.clone()))?
                        .clone();
                    let columns: Vec<String> =
                        schema.columns.iter().map(|c| c.name.clone()).collect();
                    let fast = FastLayout::new(&schema);
                    let row_layout = RowLayout::new(&schema);
                    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(64);

                    if let Some(compiled) = compile_predicate(predicate, &columns, &fast, &schema) {
                        self.catalog
                            .for_each_row_raw(table, |_rid, data| {
                                if compiled(data) {
                                    rows.push(decode_row(&schema, data));
                                }
                            })
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    } else {
                        let pred_cols = predicate_column_indices(predicate, &columns);
                        self.catalog
                            .for_each_row_raw(table, |_rid, data| {
                                let pred_row =
                                    decode_selective(&schema, &row_layout, data, &pred_cols);
                                if eval_predicate(predicate, &pred_row, &columns) {
                                    rows.push(decode_row(&schema, data));
                                }
                            })
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                    }

                    return Ok(QueryResult::Rows { columns, rows });
                }

                // General path.
                let result = self.execute_plan_readonly(input)?;
                match result {
                    QueryResult::Rows { columns, rows } => {
                        let filtered: Vec<Vec<Value>> = rows
                            .into_iter()
                            .filter(|row| eval_predicate(predicate, row, &columns))
                            .collect();
                        Ok(QueryResult::Rows {
                            columns,
                            rows: filtered,
                        })
                    }
                    _ => Err("filter requires row input".into()),
                }
            }

            PlanNode::Project { input, fields } => {
                // Fast path: Project over IndexScan. Avoids full-row decode
                // by calling decode_column only for projected fields.
                if let PlanNode::IndexScan { table, column, key } = input.as_ref() {
                    let key_value = literal_to_value(key)?;
                    let tbl = self
                        .catalog
                        .get_table(table)
                        .ok_or_else(|| QueryError::TableNotFound(table.clone()))?;
                    let schema = &tbl.schema;
                    let layout = tbl.row_layout();

                    let proj_columns: Vec<String> = fields
                        .iter()
                        .map(|f| {
                            f.alias.clone().unwrap_or_else(|| match &f.expr {
                                Expr::Field(name) => name.clone(),
                                _ => "?".into(),
                            })
                        })
                        .collect();

                    let proj_indices: Vec<usize> = fields
                        .iter()
                        .filter_map(|f| {
                            if let Expr::Field(name) = &f.expr {
                                schema.column_index(name)
                            } else {
                                None
                            }
                        })
                        .collect();

                    if tbl.has_index(column) {
                        let rids = tbl.index_lookup_all(column, &key_value);
                        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(rids.len());
                        for rid in rids {
                            if let Some(data) = tbl.heap.get(rid) {
                                let row: Vec<Value> = proj_indices
                                    .iter()
                                    .map(|&ci| decode_column(schema, layout, &data, ci))
                                    .collect();
                                rows.push(row);
                            }
                        }
                        return Ok(QueryResult::Rows {
                            columns: proj_columns,
                            rows,
                        });
                    }
                }

                // Fast paths over Limit(Sort(...)) / Limit(Filter(...)) / Limit(SeqScan).
                if let PlanNode::Limit {
                    input: inner,
                    count: limit_expr,
                } = input.as_ref()
                {
                    if let PlanNode::Sort {
                        input: sort_input,
                        keys,
                    } = inner.as_ref()
                    {
                        if keys.len() == 1 {
                            let sort_field = &keys[0].field;
                            let descending = keys[0].descending;
                            let limit = match limit_expr {
                                Expr::Literal(Literal::Int(v)) if *v >= 0 => *v as usize,
                                _ => usize::MAX,
                            };
                            let (table_opt, pred_opt): (Option<&str>, Option<&Expr>) =
                                match sort_input.as_ref() {
                                    PlanNode::SeqScan { table } => (Some(table.as_str()), None),
                                    PlanNode::Filter {
                                        input: fi,
                                        predicate,
                                    } => {
                                        if let PlanNode::SeqScan { table } = fi.as_ref() {
                                            (Some(table.as_str()), Some(predicate))
                                        } else {
                                            (None, None)
                                        }
                                    }
                                    _ => (None, None),
                                };
                            if let Some(table) = table_opt {
                                if let Some(result) = self.project_filter_sort_limit_fast(
                                    table, fields, sort_field, descending, limit, pred_opt,
                                )? {
                                    return Ok(result);
                                }
                            }
                        }
                    }
                    if let PlanNode::Filter {
                        input: fi,
                        predicate,
                    } = inner.as_ref()
                    {
                        if let PlanNode::SeqScan { table } = fi.as_ref() {
                            let limit = match limit_expr {
                                Expr::Literal(Literal::Int(v)) if *v >= 0 => *v as usize,
                                _ => usize::MAX,
                            };
                            if let Some(result) = self.project_filter_limit_fast(
                                table,
                                fields,
                                limit,
                                Some(predicate),
                            )? {
                                return Ok(result);
                            }
                        }
                    }
                    if let PlanNode::SeqScan { table } = inner.as_ref() {
                        let limit = match limit_expr {
                            Expr::Literal(Literal::Int(v)) if *v >= 0 => *v as usize,
                            _ => usize::MAX,
                        };
                        if let Some(result) =
                            self.project_filter_limit_fast(table, fields, limit, None)?
                        {
                            return Ok(result);
                        }
                    }
                }

                // Project(Filter(SeqScan)) without Limit.
                if let PlanNode::Filter {
                    input: fi,
                    predicate,
                } = input.as_ref()
                {
                    if let PlanNode::SeqScan { table } = fi.as_ref() {
                        if let Some(result) = self.project_filter_limit_fast(
                            table,
                            fields,
                            usize::MAX,
                            Some(predicate),
                        )? {
                            return Ok(result);
                        }
                    }
                }

                // Project(SeqScan) without Filter or Limit.
                if let PlanNode::SeqScan { table } = input.as_ref() {
                    if let Some(result) =
                        self.project_filter_limit_fast(table, fields, usize::MAX, None)?
                    {
                        return Ok(result);
                    }
                }

                // Generic path.
                let result = self.execute_plan_readonly(input)?;
                match result {
                    QueryResult::Rows { columns, rows } => {
                        let proj_columns: Vec<String> = fields
                            .iter()
                            .map(|f| {
                                f.alias.clone().unwrap_or_else(|| match &f.expr {
                                    Expr::Field(name) => name.clone(),
                                    Expr::QualifiedField { qualifier, field } => {
                                        format!("{qualifier}.{field}")
                                    }
                                    _ => "?".into(),
                                })
                            })
                            .collect();
                        let proj_rows: Vec<Vec<Value>> = rows
                            .iter()
                            .map(|row| {
                                fields
                                    .iter()
                                    .map(|f| eval_expr(&f.expr, row, &columns))
                                    .collect()
                            })
                            .collect();
                        Ok(QueryResult::Rows {
                            columns: proj_columns,
                            rows: proj_rows,
                        })
                    }
                    _ => Err("project requires row input".into()),
                }
            }

            PlanNode::Sort { input, keys } => {
                let result = self.execute_plan_readonly(input)?;
                match result {
                    QueryResult::Rows { columns, mut rows } => {
                        if rows.len() > MAX_SORT_ROWS {
                            return Err(QueryError::SortLimitExceeded);
                        }
                        // WS2: byte-budget guard on the sort buffer.
                        self.charge_rows(&rows)?;
                        let key_indices: Vec<(usize, bool)> = keys
                            .iter()
                            .map(|k| {
                                columns
                                    .iter()
                                    .position(|c| c == &k.field)
                                    .map(|idx| (idx, k.descending))
                                    .ok_or_else(|| QueryError::ColumnNotFound {
                                        table: String::new(),
                                        column: k.field.clone(),
                                    })
                            })
                            .collect::<Result<_, QueryError>>()?;
                        rows.sort_by(|a, b| {
                            for &(col_idx, descending) in &key_indices {
                                let cmp = a[col_idx].cmp(&b[col_idx]);
                                let cmp = if descending { cmp.reverse() } else { cmp };
                                if cmp != std::cmp::Ordering::Equal {
                                    return cmp;
                                }
                            }
                            std::cmp::Ordering::Equal
                        });
                        Ok(QueryResult::Rows { columns, rows })
                    }
                    _ => Err("sort requires row input".into()),
                }
            }

            PlanNode::Limit { input, count } => {
                let result = self.execute_plan_readonly(input)?;
                let n = match count {
                    Expr::Literal(Literal::Int(v)) => *v as usize,
                    _ => return Err("limit must be integer literal".into()),
                };
                match result {
                    QueryResult::Rows { columns, rows } => Ok(QueryResult::Rows {
                        columns,
                        rows: rows.into_iter().take(n).collect(),
                    }),
                    _ => Err("limit requires row input".into()),
                }
            }

            PlanNode::Offset { input, count } => {
                let result = self.execute_plan_readonly(input)?;
                let n = match count {
                    Expr::Literal(Literal::Int(v)) => *v as usize,
                    _ => return Err("offset must be integer literal".into()),
                };
                match result {
                    QueryResult::Rows { columns, rows } => Ok(QueryResult::Rows {
                        columns,
                        rows: rows.into_iter().skip(n).collect(),
                    }),
                    _ => Err("offset requires row input".into()),
                }
            }

            PlanNode::Aggregate {
                input,
                function,
                field,
            } => {
                // Fast path: count() over SeqScan.
                if *function == AggFunc::Count {
                    if let PlanNode::SeqScan { table } = input.as_ref() {
                        // A dirty materialized view must be refreshed before
                        // it can be counted, which needs `&mut self`. Escalate
                        // to the write path (F3: count(View) returned stale).
                        if self.view_registry.is_dirty(table) {
                            return Err(QueryError::ReadonlyNeedsWrite);
                        }
                        let mut count: i64 = 0;
                        self.catalog
                            .for_each_row_raw(table, |_rid, _data| {
                                count += 1;
                            })
                            .map_err(|e| QueryError::StorageError(e.to_string()))?;
                        return Ok(QueryResult::Scalar(Value::Int(count)));
                    }
                    if let PlanNode::Filter {
                        input: inner,
                        predicate,
                    } = input.as_ref()
                    {
                        // Only take the fast path for a plain Filter(SeqScan)
                        // with no subquery in the predicate. A subquery
                        // predicate (`count(T filter .x in (...))`) must be
                        // resolved first; the fast path evaluates the raw
                        // predicate with no subquery materialisation, which
                        // silently yields 0 (F1). Falling through routes it to
                        // the generic path that runs the subquery correctly.
                        if let PlanNode::SeqScan { table } = inner.as_ref() {
                            if self.view_registry.is_dirty(table) {
                                // F3: count(View filter ...) over a dirty view.
                                return Err(QueryError::ReadonlyNeedsWrite);
                            }
                        }
                        if let (PlanNode::SeqScan { table }, false) =
                            (inner.as_ref(), contains_subquery(predicate))
                        {
                            let schema = self
                                .catalog
                                .schema(table)
                                .ok_or_else(|| QueryError::TableNotFound(table.clone()))?
                                .clone();
                            let columns: Vec<String> =
                                schema.columns.iter().map(|c| c.name.clone()).collect();
                            let fast = FastLayout::new(&schema);
                            let row_layout = RowLayout::new(&schema);

                            if let Some(compiled) =
                                compile_predicate(predicate, &columns, &fast, &schema)
                            {
                                let mut count: i64 = 0;
                                self.catalog
                                    .for_each_row_raw(table, |_rid, data| {
                                        if compiled(data) {
                                            count += 1;
                                        }
                                    })
                                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                                return Ok(QueryResult::Scalar(Value::Int(count)));
                            }

                            let pred_cols = predicate_column_indices(predicate, &columns);
                            let mut count: i64 = 0;
                            self.catalog
                                .for_each_row_raw(table, |_rid, data| {
                                    let pred_row =
                                        decode_selective(&schema, &row_layout, data, &pred_cols);
                                    if eval_predicate(predicate, &pred_row, &columns) {
                                        count += 1;
                                    }
                                })
                                .map_err(|e| QueryError::StorageError(e.to_string()))?;
                            return Ok(QueryResult::Scalar(Value::Int(count)));
                        }
                    }
                }

                // Fast path: sum/avg/min/max over single fixed-size numeric.
                if matches!(
                    function,
                    AggFunc::Sum
                        | AggFunc::Avg
                        | AggFunc::Min
                        | AggFunc::Max
                        | AggFunc::CountDistinct
                ) {
                    if let Some(col) = field.as_ref() {
                        let (table_opt, pred_opt): (Option<&str>, Option<&Expr>) =
                            match input.as_ref() {
                                PlanNode::SeqScan { table } => (Some(table.as_str()), None),
                                PlanNode::Filter {
                                    input: inner,
                                    predicate,
                                } => {
                                    if let PlanNode::SeqScan { table } = inner.as_ref() {
                                        (Some(table.as_str()), Some(predicate))
                                    } else {
                                        (None, None)
                                    }
                                }
                                _ => (None, None),
                            };
                        if let Some(table) = table_opt {
                            if let Some(result) =
                                self.agg_single_col_fast(table, col, *function, pred_opt)?
                            {
                                return Ok(result);
                            }
                        }
                    }
                }

                // Generic path.
                let result = self.execute_plan_readonly(input)?;
                match result {
                    QueryResult::Rows { columns, rows } => match function {
                        AggFunc::Count => Ok(QueryResult::Scalar(Value::Int(rows.len() as i64))),
                        AggFunc::CountDistinct => {
                            let col = field.as_ref().ok_or("count distinct requires field")?;
                            let idx = columns
                                .iter()
                                .position(|c| c == col)
                                .ok_or("col not found")?;
                            let mut seen = std::collections::HashSet::new();
                            for row in &rows {
                                let v = &row[idx];
                                if !v.is_empty() {
                                    seen.insert(v.clone());
                                }
                            }
                            Ok(QueryResult::Scalar(Value::Int(seen.len() as i64)))
                        }
                        AggFunc::Avg => {
                            let col = field.as_ref().ok_or("avg requires field")?;
                            let idx = columns
                                .iter()
                                .position(|c| c == col)
                                .ok_or("col not found")?;
                            let mut count: u64 = 0;
                            let sum: f64 = rows
                                .iter()
                                .filter_map(|r| match &r[idx] {
                                    Value::Int(v) => Some(*v as f64),
                                    Value::Float(v) => Some(*v),
                                    _ => None,
                                })
                                .inspect(|_| count += 1)
                                .sum();
                            if count == 0 {
                                Ok(QueryResult::Scalar(Value::Empty))
                            } else {
                                Ok(QueryResult::Scalar(Value::Float(sum / count as f64)))
                            }
                        }
                        AggFunc::Sum => {
                            let col = field.as_ref().ok_or("sum requires field")?;
                            let idx = columns
                                .iter()
                                .position(|c| c == col)
                                .ok_or("col not found")?;
                            let mut int_sum: i64 = 0;
                            let mut float_sum: f64 = 0.0;
                            let mut saw_float = false;
                            for r in &rows {
                                match &r[idx] {
                                    Value::Int(v) => int_sum += *v,
                                    Value::Float(v) => {
                                        float_sum += *v;
                                        saw_float = true;
                                    }
                                    _ => {}
                                }
                            }
                            let result = if saw_float {
                                Value::Float(float_sum + int_sum as f64)
                            } else {
                                Value::Int(int_sum)
                            };
                            Ok(QueryResult::Scalar(result))
                        }
                        AggFunc::Min | AggFunc::Max => {
                            let col = field.as_ref().ok_or("min/max requires field")?;
                            let idx = columns
                                .iter()
                                .position(|c| c == col)
                                .ok_or("col not found")?;
                            let vals: Vec<&Value> = rows.iter().map(|r| &r[idx]).collect();
                            let result = if *function == AggFunc::Min {
                                vals.into_iter().min().cloned()
                            } else {
                                vals.into_iter().max().cloned()
                            };
                            Ok(QueryResult::Scalar(result.unwrap_or(Value::Empty)))
                        }
                    },
                    _ => Err("aggregate requires row input".into()),
                }
            }

            PlanNode::Distinct { input } => {
                let result = self.execute_plan_readonly(input)?;
                match result {
                    QueryResult::Rows { columns, rows } => {
                        let mut seen = std::collections::HashSet::new();
                        let mut unique_rows = Vec::new();
                        for row in rows {
                            if seen.insert(row.clone()) {
                                unique_rows.push(row);
                            }
                        }
                        Ok(QueryResult::Rows {
                            columns,
                            rows: unique_rows,
                        })
                    }
                    other => Ok(other),
                }
            }

            PlanNode::GroupBy {
                input,
                keys,
                aggregates,
                having,
            } => {
                let result = self.execute_plan_readonly(input)?;
                match result {
                    QueryResult::Rows { columns, rows } => {
                        // WS2: byte-budget guard on the GROUP BY input buffer
                        // (the hash table is bounded by the input it groups).
                        self.charge_rows(&rows)?;
                        let key_indices: Vec<usize> = keys
                            .iter()
                            .map(|k| {
                                columns.iter().position(|c| c == k).ok_or_else(|| {
                                    QueryError::ColumnNotFound {
                                        table: String::new(),
                                        column: k.clone(),
                                    }
                                })
                            })
                            .collect::<Result<Vec<_>, _>>()?;

                        let agg_field_indices: Vec<usize> = aggregates
                            .iter()
                            .map(|a| {
                                if a.field == "*" {
                                    Ok(usize::MAX)
                                } else {
                                    columns.iter().position(|c| c == &a.field).ok_or_else(|| {
                                        QueryError::ColumnNotFound {
                                            table: String::new(),
                                            column: a.field.clone(),
                                        }
                                    })
                                }
                            })
                            .collect::<Result<Vec<_>, _>>()?;

                        let mut group_map: rustc_hash::FxHashMap<Vec<Value>, usize> =
                            rustc_hash::FxHashMap::default();
                        let mut groups: Vec<(Vec<Value>, Vec<usize>)> = Vec::new();
                        for (ri, row) in rows.iter().enumerate() {
                            let key: Vec<Value> =
                                key_indices.iter().map(|&i| row[i].clone()).collect();
                            match group_map.get(&key) {
                                Some(&idx) => groups[idx].1.push(ri),
                                None => {
                                    let idx = groups.len();
                                    group_map.insert(key.clone(), idx);
                                    groups.push((key, vec![ri]));
                                }
                            }
                        }

                        let mut out_columns: Vec<String> = keys.clone();
                        for agg in aggregates.iter() {
                            out_columns.push(agg.output_name.clone());
                        }

                        let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(groups.len());
                        for (key_vals, row_indices) in &groups {
                            let mut row = key_vals.clone();
                            for (ai, agg) in aggregates.iter().enumerate() {
                                let col_idx = agg_field_indices[ai];
                                let val = compute_group_aggregate(
                                    agg.function,
                                    &rows,
                                    row_indices,
                                    col_idx,
                                );
                                row.push(val);
                            }
                            out_rows.push(row);
                        }

                        if let Some(having_expr) = having {
                            out_rows.retain(|row| eval_predicate(having_expr, row, &out_columns));
                        }

                        Ok(QueryResult::Rows {
                            columns: out_columns,
                            rows: out_rows,
                        })
                    }
                    _ => Err("group by requires row input".into()),
                }
            }

            PlanNode::NestedLoopJoin {
                left,
                right,
                on,
                kind,
            } => {
                let left_result = self.execute_plan_readonly(left)?;
                let right_result = self.execute_plan_readonly(right)?;
                let (left_columns, left_rows) = match left_result {
                    QueryResult::Rows { columns, rows } => (columns, rows),
                    _ => return Err("join left side must produce rows".into()),
                };
                let (right_columns, right_rows) = match right_result {
                    QueryResult::Rows { columns, rows } => (columns, rows),
                    _ => return Err("join right side must produce rows".into()),
                };

                // WS2: byte-budget guard on the join build side.
                self.charge_rows(&left_rows)?;
                self.charge_rows(&right_rows)?;

                if !matches!(kind, JoinKind::Cross) {
                    if let Some(pred) = on {
                        if let Some((l_idx, r_idx)) =
                            try_extract_equi_join_keys(pred, &left_columns, &right_columns)
                        {
                            let result = hash_join(
                                left_columns,
                                left_rows,
                                right_columns,
                                right_rows,
                                l_idx,
                                r_idx,
                                *kind,
                            );
                            if let QueryResult::Rows { ref rows, .. } = result {
                                check_join_limit(rows.len())?;
                            }
                            return Ok(result);
                        }
                    }
                }

                let n_left = left_columns.len();
                let n_right = right_columns.len();
                let mut columns = Vec::with_capacity(n_left + n_right);
                columns.extend(left_columns);
                columns.extend(right_columns);

                let mut rows: Vec<Vec<Value>> = Vec::with_capacity(left_rows.len());
                let mut combined: Vec<Value> = Vec::with_capacity(n_left + n_right);

                for left_row in &left_rows {
                    let mut matched = false;
                    for right_row in &right_rows {
                        combined.clear();
                        combined.extend_from_slice(left_row);
                        combined.extend_from_slice(right_row);
                        let keep = match kind {
                            JoinKind::Cross => true,
                            JoinKind::Inner | JoinKind::LeftOuter => match on {
                                Some(pred) => eval_predicate(pred, &combined, &columns),
                                None => true,
                            },
                            JoinKind::RightOuter => {
                                unreachable!("planner rewrites RightOuter to LeftOuter")
                            }
                        };
                        if keep {
                            rows.push(combined.clone());
                            check_join_limit(rows.len())?;
                            matched = true;
                        }
                    }
                    if !matched && matches!(kind, JoinKind::LeftOuter) {
                        let mut row = Vec::with_capacity(n_left + n_right);
                        row.extend_from_slice(left_row);
                        row.resize(n_left + n_right, Value::Empty);
                        rows.push(row);
                        check_join_limit(rows.len())?;
                    }
                }

                Ok(QueryResult::Rows { columns, rows })
            }

            PlanNode::Window { input, windows } => {
                let result = self.execute_plan_readonly(input)?;
                execute_window(result, windows)
            }

            PlanNode::Union { left, right, all } => {
                let left_result = self.execute_plan_readonly(left)?;
                let right_result = self.execute_plan_readonly(right)?;
                let (left_cols, left_rows) = match left_result {
                    QueryResult::Rows { columns, rows } => (columns, rows),
                    _ => return Err("UNION requires query results on left side".into()),
                };
                let (_, right_rows) = match right_result {
                    QueryResult::Rows { columns, rows } => (columns, rows),
                    _ => return Err("UNION requires query results on right side".into()),
                };
                let mut combined = left_rows;
                if *all {
                    combined.extend(right_rows);
                } else {
                    let mut seen = std::collections::HashSet::new();
                    for row in &combined {
                        seen.insert(row.clone());
                    }
                    for row in right_rows {
                        if seen.insert(row.clone()) {
                            combined.push(row);
                        }
                    }
                }
                Ok(QueryResult::Rows {
                    columns: left_cols,
                    rows: combined,
                })
            }

            PlanNode::Explain { input } => {
                let text = format_plan_tree(input, 0);
                Ok(QueryResult::Rows {
                    columns: vec!["plan".to_string()],
                    rows: text
                        .lines()
                        .map(|line| vec![Value::Str(line.to_string())])
                        .collect(),
                })
            }

            // All write variants — caller must escalate to the write lock.
            PlanNode::Insert { .. }
            | PlanNode::Update { .. }
            | PlanNode::Delete { .. }
            | PlanNode::Upsert { .. }
            | PlanNode::CreateTable { .. }
            | PlanNode::AlterTable { .. }
            | PlanNode::DropTable { .. }
            | PlanNode::CreateView { .. }
            | PlanNode::RefreshView { .. }
            | PlanNode::DropView { .. }
            | PlanNode::Begin
            | PlanNode::Commit
            | PlanNode::Rollback => Err(QueryError::ReadonlyNeedsWrite),
        }
    }

    /// `&self` variant of [`Engine::materialize_subqueries`]. Used by the
    /// read path so `Filter` predicates with `InSubquery`/`ExistsSubquery`
    /// children can evaluate their inner queries without taking the write
    /// lock. Inner queries that would themselves need a write (e.g. dirty
    /// view) escalate via [`READONLY_NEEDS_WRITE`] just like the top-level
    /// read path does.
    fn materialize_subqueries_readonly(&self, expr: &Expr) -> Result<Expr, QueryError> {
        match expr {
            Expr::InSubquery {
                expr: inner,
                subquery,
                negated,
            } => {
                if is_correlated_subquery(subquery, &self.catalog) {
                    // Pass through — will be materialized per-row in the
                    // Filter handler's correlated subquery path.
                    let inner = self.materialize_subqueries_readonly(inner)?;
                    return Ok(Expr::InSubquery {
                        expr: Box::new(inner),
                        subquery: subquery.clone(),
                        negated: *negated,
                    });
                }
                let inner = self.materialize_subqueries_readonly(inner)?;
                let sub_plan = crate::planner::plan_statement(Statement::Query(*subquery.clone()))
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                let result = self.execute_plan_readonly(&sub_plan)?;
                let values = match result {
                    QueryResult::Rows { rows, .. } => rows
                        .into_iter()
                        .filter_map(|mut row| {
                            if row.is_empty() {
                                None
                            } else {
                                Some(value_to_expr(row.swap_remove(0)))
                            }
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                // WS2: byte-budget guard on the materialized IN-list.
                self.charge_in_list(&values)?;
                Ok(Expr::InList {
                    expr: Box::new(inner),
                    list: values,
                    negated: *negated,
                })
            }
            Expr::ExistsSubquery { subquery, negated } => {
                if is_correlated_subquery(subquery, &self.catalog) {
                    return Ok(expr.clone());
                }
                let sub_plan = crate::planner::plan_statement(Statement::Query(*subquery.clone()))
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                let result = self.execute_plan_readonly(&sub_plan)?;
                let has_rows = match result {
                    QueryResult::Rows { rows, .. } => !rows.is_empty(),
                    _ => false,
                };
                let truth = if *negated { !has_rows } else { has_rows };
                Ok(Expr::Literal(Literal::Bool(truth)))
            }
            Expr::BinaryOp(l, op, r) => {
                let l = self.materialize_subqueries_readonly(l)?;
                let r = self.materialize_subqueries_readonly(r)?;
                Ok(Expr::BinaryOp(Box::new(l), *op, Box::new(r)))
            }
            Expr::UnaryOp(op, inner) => {
                let inner = self.materialize_subqueries_readonly(inner)?;
                Ok(Expr::UnaryOp(*op, Box::new(inner)))
            }
            Expr::Case { whens, else_expr } => {
                let whens = whens
                    .iter()
                    .map(|(c, r)| {
                        let c = self.materialize_subqueries_readonly(c)?;
                        let r = self.materialize_subqueries_readonly(r)?;
                        Ok((Box::new(c), Box::new(r)))
                    })
                    .collect::<Result<Vec<_>, QueryError>>()?;
                let else_expr = match else_expr {
                    Some(e) => Some(Box::new(self.materialize_subqueries_readonly(e)?)),
                    None => None,
                };
                Ok(Expr::Case { whens, else_expr })
            }
            other => Ok(other.clone()),
        }
    }

    /// Per-row materialisation of correlated subqueries. For each row in the
    /// outer query, substitute outer column references in the subquery's
    /// filter with the current row's literal values, execute the modified
    /// subquery, and return the result as an InList or Bool literal.
    fn materialize_correlated_for_row_readonly(
        &self,
        expr: &Expr,
        outer_row: &[Value],
        outer_columns: &[String],
    ) -> Result<Expr, QueryError> {
        match expr {
            Expr::InSubquery {
                expr: inner,
                subquery,
                negated,
            } => {
                let inner =
                    self.materialize_correlated_for_row_readonly(inner, outer_row, outer_columns)?;
                let mut sub = *subquery.clone();
                if let Some(ref filter) = sub.filter {
                    sub.filter = Some(substitute_outer_refs(
                        filter,
                        &sub.source,
                        &self.catalog,
                        outer_row,
                        outer_columns,
                    ));
                }
                let sub_plan = crate::planner::plan_statement(Statement::Query(sub))
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                let result = self.execute_plan_readonly(&sub_plan)?;
                let values = match result {
                    QueryResult::Rows { rows, .. } => rows
                        .into_iter()
                        .filter_map(|mut row| {
                            if row.is_empty() {
                                None
                            } else {
                                Some(value_to_expr(row.swap_remove(0)))
                            }
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                // WS2: byte-budget guard on the per-row materialized IN-list.
                self.charge_in_list(&values)?;
                Ok(Expr::InList {
                    expr: Box::new(inner),
                    list: values,
                    negated: *negated,
                })
            }
            Expr::ExistsSubquery { subquery, negated } => {
                let mut sub = *subquery.clone();
                if let Some(ref filter) = sub.filter {
                    sub.filter = Some(substitute_outer_refs(
                        filter,
                        &sub.source,
                        &self.catalog,
                        outer_row,
                        outer_columns,
                    ));
                }
                let sub_plan = crate::planner::plan_statement(Statement::Query(sub))
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
                let result = self.execute_plan_readonly(&sub_plan)?;
                let has_rows = match result {
                    QueryResult::Rows { rows, .. } => !rows.is_empty(),
                    _ => false,
                };
                let truth = if *negated { !has_rows } else { has_rows };
                Ok(Expr::Literal(Literal::Bool(truth)))
            }
            Expr::BinaryOp(l, op, r) => {
                let l =
                    self.materialize_correlated_for_row_readonly(l, outer_row, outer_columns)?;
                let r =
                    self.materialize_correlated_for_row_readonly(r, outer_row, outer_columns)?;
                Ok(Expr::BinaryOp(Box::new(l), *op, Box::new(r)))
            }
            Expr::UnaryOp(op, inner) => {
                let inner =
                    self.materialize_correlated_for_row_readonly(inner, outer_row, outer_columns)?;
                Ok(Expr::UnaryOp(*op, Box::new(inner)))
            }
            other => Ok(other.clone()),
        }
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn catalog_mut(&mut self) -> &mut Catalog {
        &mut self.catalog
    }
}
