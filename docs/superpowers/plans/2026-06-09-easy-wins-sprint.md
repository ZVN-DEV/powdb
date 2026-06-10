# PowDB Easy-Wins Sprint Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship six high-leverage improvements: truthful EXPLAIN, real B+tree range scans, UNIQUE constraints, wire-level parameter binding, multi-line REPL input, and a repeatable agent-DX eval harness — each landing green and separately committed.

**Architecture:** PowQL pipeline is lexer → parser → planner (pure, no catalog) → executor (lowers speculative plans at runtime against the catalog). Storage is heap + B+tree + WAL + catalog in `crates/storage`. Server is a length-prefixed binary TCP protocol in `crates/server`. TS client in `clients/ts`.

**Tech Stack:** Rust workspace, criterion bench, TS client (Node 22, pnpm).

**Branch:** All work happens on a new branch `feat/easy-wins-sprint`, cut from main after PR #81 merges. Never push to `main`. Never touch `crates/bench/baseline/main.json` (CI-hardware only).

```bash
cd /Users/macbookpro-kirby/Desktop/Coding/ZVN/PowDB
git checkout main && git pull
git checkout -b feat/easy-wins-sprint
```

**Per-task gate (applies to every task, referenced as "GATE" below):**
```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

**Key reconnaissance facts the implementer must know (verified against source):**
- `lower_unindexed_range_scans` (`crates/query/src/executor/plan_exec.rs:3372`) ALREADY recurses into `PlanNode::Explain` (line 3462) and already lowers unindexed `RangeScan` → `Filter(SeqScan)`. The actual EXPLAIN lie is `IndexScan`: the lowering pass treats it as a leaf (falls through `_ => plan.clone()` at 3477), while the executor's `IndexScan` arm (plan_exec.rs:1588) silently falls back to a compiled-predicate scan at 1611–1636 when `!tbl.has_index(column)`.
- `RangeScan` execution ALREADY walks the btree — but only for unique indexes (`plan_exec.rs:1687`, gated by `tbl.is_index_unique(column) == Some(true)`; same gate in the lowering pass at 3385). Non-unique indexes store composite keys `(col_val, rid)` (`crates/storage/src/btree.rs:754`, `make_composite_key`) with order-preserving big-endian encodings, plus `make_prefix_start`/`make_prefix_end`/`rid_from_composite` helpers (btree.rs:800–892). `alter T add index .c` creates NON-unique indexes (`plan_exec.rs:1451` → `Catalog::create_index` → `create_index_unique(…, false)`).
- Uniqueness infrastructure half-exists: `IndexedCol.unique` (`crates/storage/src/table.rs:35`), `IndexedColMeta { name, unique }` persisted by `Catalog::persist()` (table.rs:177–185, catalog.rs:1173–1186), `Table::create_index_with_unique` (table.rs:1012). But unique-index insert OVERWRITES duplicates ("correct for PKs", table.rs:405–413) — no rejection anywhere. Nulls are never indexed (table.rs:402).
- Prepared machinery: `Engine::prepare` / `execute_prepared(prep, &[Literal])` (`crates/query/src/executor/prepared.rs:98,260`). `Literal` has NO Null variant (`ast.rs:322`), but `Token::Null` exists (`token.rs:64`) — token-level substitution sidesteps that gap.
- Protocol msg ids `0x04–0x06` are free (`crates/server/src/protocol.rs:4-14`). Query dispatch: `handler.rs:466` (`Message::Query` arm) → `dispatch_query` (handler.rs:262).
- TS client `query(query, opts?)` already takes an options object as arg 2 (`clients/ts/src/index.ts:226`) — the params overload must disambiguate with `Array.isArray`.
- REPL loops: embedded at `crates/cli/src/main.rs:919`, remote at 1092; `--exec` one-shot mode exists (main.rs:520-543) — the eval harness uses it.
- Executor test style: `crates/query/src/executor/tests.rs:10-27` (`test_engine()` helper, temp dir + counter). Btree test style: `btree.rs:1904+` (`temp_btree`). Protocol test style: `protocol.rs:339+`.

---

## Task 1: EXPLAIN shows the lowered plan (unindexed IndexScan → Filter(SeqScan))

**Files:**
- Modify: `crates/query/src/executor/plan_exec.rs` (`lower_unindexed_range_scans` at 3372, rename to `lower_unindexed_scans`; doc comment 3360–3371)
- Modify: `crates/query/src/executor/mod.rs` (import at lines 223–224; call sites at 438, 461, 477, 501, 579, 590, 597)
- Test: `crates/query/src/executor/tests.rs` (EXPLAIN section starts at 3254)

**Steps:**

- [ ] 1. Write failing tests in `crates/query/src/executor/tests.rs` after `test_explain_filter` (line ~3293), matching the existing helper style:

```rust
fn explain_text(engine: &mut Engine, q: &str) -> String {
    match engine.execute_powql(q).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r[0] {
                Value::Str(s) => s.as_str(),
                _ => "",
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_explain_eq_filter_unindexed_shows_seqscan_not_indexscan() {
    let mut engine = test_engine();
    // `email` has NO index in test_engine; the planner folds
    // `.email = lit` to IndexScan speculatively. EXPLAIN must show
    // what actually runs: Filter over SeqScan.
    let text = explain_text(&mut engine, r#"explain User filter .email = "alice@ex.com""#);
    assert!(!text.contains("IndexScan"), "got: {text}");
    assert!(text.contains("Filter"), "got: {text}");
    assert!(text.contains("SeqScan"), "got: {text}");
}

#[test]
fn test_explain_eq_filter_indexed_shows_indexscan() {
    let mut engine = test_engine();
    engine.execute_powql("alter User add index .email").unwrap();
    let text = explain_text(&mut engine, r#"explain User filter .email = "alice@ex.com""#);
    assert!(text.contains("IndexScan"), "got: {text}");
}
```

- [ ] 2. Run them and confirm the failure shape:
```bash
cargo test -p powdb-query test_explain_eq_filter -- --nocapture
```
Expected: `test_explain_eq_filter_unindexed_shows_seqscan_not_indexscan` FAILS (text contains `IndexScan table=User column=email …`); the indexed test passes already.

- [ ] 3. Implement: in `plan_exec.rs`, rename `lower_unindexed_range_scans` → `lower_unindexed_scans` (it now covers both speculative leaf kinds) and add an `IndexScan` arm before the catch-all, reusing the exact predicate shape the runtime fallback synthesizes at 1620–1624:

```rust
PlanNode::IndexScan { table, column, key } => {
    if let Some(tbl) = catalog.get_table(table) {
        if tbl.has_index(column) {
            return plan.clone();
        }
    }
    PlanNode::Filter {
        input: Box::new(PlanNode::SeqScan { table: table.clone() }),
        predicate: Expr::BinaryOp(
            Box::new(Expr::Field(column.clone())),
            BinOp::Eq,
            Box::new(key.clone()),
        ),
    }
}
```
Update the function doc comment (3360–3371) to say it lowers BOTH unindexed `RangeScan` and unindexed `IndexScan`. Update the `use` in `executor/mod.rs:224` and all 7 call sites to the new name.

- [ ] 4. Behavioral note to verify while implementing (not a TBD — a check): `Update(IndexScan)` plans (planner.rs:447) on unindexed columns now lower to `Update(Filter(SeqScan))`, which hits the fused scan+update path (plan_exec.rs:844–870) instead of `collect_rids_for_mutation`'s IndexScan fallback (plan_exec.rs:2715). Both are correct; run the executor update tests explicitly: `cargo test -p powdb-query -- update`.

- [ ] 5. Run the full GATE. The existing `test_explain_filter` (tests.rs:3271, expects `Filter` for an unindexed range) must still pass — the RangeScan lowering for `Explain` was already in place.

- [ ] 6. Commit:
```bash
git add crates/query
git commit -m "fix(query): lower unindexed IndexScan in plan lowering so EXPLAIN shows the executed plan

EXPLAIN previously printed the planner's speculative IndexScan even when
no index existed and execution fell back to a filtered scan. The lowering
pass (renamed lower_unindexed_scans) now rewrites unindexed IndexScan to
Filter(SeqScan), same as it already did for RangeScan."
```

---

## Task 2: Range scans use B+tree indexes (non-unique composite-key traversal)

**Files:**
- Modify: `crates/storage/src/btree.rs` (new `range_rids` method near the non-unique section, after `lookup_prefix_int` at 925; tests in the `Non-unique index tests` section at 1904+)
- Modify: `crates/query/src/executor/plan_exec.rs` (RangeScan exec arm 1659–1699+; lowering gate at 3385; comment 3381–3384)
- Modify: `crates/bench/benches/powql.rs` (new bench fn; register in `criterion_group!` at 655)
- Modify docs: `docs/POWQL.md:1035`, `docs/getting-started.md:368`, `AGENTS.md:130` (perf section item 1)
- Test: `crates/storage/src/btree.rs` tests, `crates/query/src/executor/tests.rs`

**Steps:**

- [ ] 1. Write failing btree unit test in `btree.rs` tests (style of `test_non_unique_insert_and_lookup_prefix` at 1907, using `temp_btree`):

```rust
#[test]
fn test_non_unique_range_rids() {
    let mut bt = temp_btree("nonunique_range");
    let rids: Vec<RowId> = (0..6u32)
        .map(|i| RowId { page_id: i, slot_index: 0 })
        .collect();
    for (i, rid) in rids.iter().enumerate() {
        bt.insert_non_unique_int((i as i64) * 10, *rid); // 0,10,20,30,40,50
    }
    // 10 <= v <= 30 → rids[1..=3]
    let hits = bt.range_rids(Some(&Value::Int(10)), Some(&Value::Int(30)));
    assert_eq!(hits, vec![rids[1], rids[2], rids[3]]);
    // unbounded below
    let hits = bt.range_rids(None, Some(&Value::Int(10)));
    assert_eq!(hits, vec![rids[0], rids[1]]);
    // unbounded above
    let hits = bt.range_rids(Some(&Value::Int(40)), None);
    assert_eq!(hits, vec![rids[4], rids[5]]);
    // duplicates within the range all come back
    bt.insert_non_unique_int(20, RowId { page_id: 99, slot_index: 7 });
    let hits = bt.range_rids(Some(&Value::Int(20)), Some(&Value::Int(20)));
    assert_eq!(hits.len(), 2);
}
```

- [ ] 2. `cargo test -p powdb-storage test_non_unique_range_rids` — expected failure: `no method named range_rids found for struct BTree` (compile error).

- [ ] 3. Implement `range_rids` in `btree.rs` (after `lookup_prefix_int`, line ~927). Bounds are always INCLUSIVE at the composite level — exclusivity is enforced by the executor's per-row recheck (step 7):

```rust
/// Range scan over a NON-unique index: return RowIds for all entries
/// whose column value lies in [start, end] (inclusive; pass None for
/// an unbounded side). Composite-key bounds reuse the prefix encoding:
/// (start, RowId::MIN) .. (end, RowId::MAX). Caller rechecks exclusive
/// bounds against the decoded row.
pub fn range_rids(&self, start: Option<&Value>, end: Option<&Value>) -> Vec<RowId> {
    let collect = |pairs: Vec<(Value, RowId)>| {
        pairs
            .into_iter()
            .filter_map(|(k, _)| Self::rid_from_composite(&k))
            .collect()
    };
    match (start, end) {
        (Some(s), Some(e)) => {
            let lo = Self::make_prefix_start(s);
            let hi = Self::make_prefix_end(e);
            self.range(&lo, &hi)
                .filter_map(|(k, _)| Self::rid_from_composite(&k))
                .collect()
        }
        (Some(s), None) => collect(self.range_from(&Self::make_prefix_start(s))),
        (None, Some(e)) => collect(self.range_to(&Self::make_prefix_end(e))),
        (None, None) => collect(self.range_from(&Self::make_prefix_start(&Value::Empty))),
    }
}
```
Note on `(None, None)`: a single-column index holds one value type per tree, so full-tree iteration via the leftmost leaf is fine; but the executor never sends `(None, None)` (it short-circuits to a heap scan at plan_exec.rs:1693–1697). Implement the simple correct thing and keep the executor short-circuit.

- [ ] 4. Run `cargo test -p powdb-storage` — green.

- [ ] 5. Write failing executor tests in `executor/tests.rs`:

```rust
#[test]
fn test_range_scan_uses_nonunique_index_same_results() {
    let mut engine = test_engine(); // Alice 30, Bob 25, Charlie 35
    let unindexed = engine.execute_powql("User filter .age > 26 and .age <= 35 { .name }").unwrap();
    engine.execute_powql("alter User add index .age").unwrap();
    let indexed = engine.execute_powql("User filter .age > 26 and .age <= 35 { .name }").unwrap();
    let names = |r: QueryResult| match r {
        QueryResult::Rows { rows, .. } => {
            let mut v: Vec<String> = rows.iter().map(|r| format!("{:?}", r[0])).collect();
            v.sort();
            v
        }
        _ => panic!("expected rows"),
    };
    assert_eq!(names(unindexed), names(indexed)); // Alice, Charlie
}

#[test]
fn test_range_scan_indexed_excludes_nulls() {
    let mut engine = test_engine();
    engine.execute_powql(r#"insert User { name := "Dana", email := "d@ex.com" }"#).unwrap(); // age null
    engine.execute_powql("alter User add index .age").unwrap();
    match engine.execute_powql("User filter .age < 100 { .name }").unwrap() {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 3, "null age must not match"),
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_explain_range_indexed_shows_rangescan() {
    let mut engine = test_engine();
    engine.execute_powql("alter User add index .age").unwrap();
    let text = explain_text(&mut engine, "explain User filter .age > 26");
    assert!(text.contains("RangeScan"), "got: {text}");
}
```
Also add a `between` case (`User filter .age between 25 and 30`) and an exclusive-bound case asserting Bob(25) is excluded by `.age > 25`.

- [ ] 6. Run `cargo test -p powdb-query test_range_scan -- --nocapture` and `test_explain_range_indexed` — expected: `test_explain_range_indexed_shows_rangescan` fails with `Filter`/`SeqScan` text (non-unique indexes get lowered today); the equality-of-results test passes trivially pre-change (both lowered) — it becomes the regression net post-change.

- [ ] 7. Implement executor support in `plan_exec.rs` RangeScan arm: after the existing unique-index branch (1687), add:

```rust
// Non-unique index: composite-key leaf walk, then heap fetch + recheck.
// The recheck enforces exclusive bounds (range_rids is inclusive) and
// defensively skips any decoded null.
if tbl.is_index_unique(column) == Some(false) {
    if let Some(btree) = tbl.index(column) {
        if start_val.is_some() || end_val.is_some() {
            let col_idx = schema.column_index(column).ok_or_else(|| {
                QueryError::ColumnNotFound { table: String::new(), column: column.clone() }
            })?;
            let rids = btree.range_rids(start_val.as_ref(), end_val.as_ref());
            let mut rows: Vec<Vec<Value>> = Vec::with_capacity(rids.len());
            for rid in rids {
                if let Some(data) = tbl.heap.get(rid) {
                    let row = decode_row(&tbl.schema, &data);
                    if !row[col_idx].is_empty()
                        && range_matches(&row[col_idx], &start_val, start_inclusive, &end_val, end_inclusive)
                    {
                        rows.push(row);
                    }
                }
            }
            return Ok(QueryResult::Rows { columns, rows });
        }
    }
}
```
Then flip the lowering gate at 3385 from `tbl.is_index_unique(column) == Some(true)` to `tbl.has_index(column)`, and rewrite the comment at 3381–3384 (non-unique indexes now traverse composite keys natively).

- [ ] 8. Run `cargo test -p powdb-query` — all range/explain tests green, including Task 1's tests and the pre-existing `test_explain_filter` (unindexed range still lowers).

- [ ] 9. Bench: add to `crates/bench/benches/powql.rs` (mirror `bench_powql_filter_only` at 218 — copy its setup, add `alter ... add index .age` during setup) a `bench_range_scan_indexed` and register it in `criterion_group!` (line 655). Informal check only:
```bash
cargo bench -p powdb-bench -- range_scan_indexed filter_only point_lookup
```
Decision criteria: indexed range scan on a selective range should beat `filter_only` for selectivity well under ~10%; `filter_only` and `point_lookup` numbers must be within noise of the pre-change run (re-run the pre-change baseline once before starting the task and keep the console output). Do NOT modify `crates/bench/baseline/main.json`.

- [ ] 10. Docs in the SAME commit: update `docs/POWQL.md:1035` and `docs/getting-started.md:368` (the docs sweep just corrected these to say range scans do NOT use indexes — flip them back once true); update `AGENTS.md:130` item 1 to: planner emits `RangeScan`/`IndexScan` speculatively; executor lowers to `Filter(SeqScan)` only when no index exists, otherwise walks the B+tree (unique: raw keys; non-unique: composite `(value, rid)` keys).

- [ ] 11. Full GATE, then commit:
```bash
git add crates/storage crates/query crates/bench docs AGENTS.md
git commit -m "feat(query,storage): RangeScan executes against non-unique B+tree indexes

BTree::range_rids walks composite (value,rid) keys between prefix bounds;
the executor fetches rows from the heap by rid and rechecks bounds (which
also preserves null-exclusion semantics). Plan lowering now keeps RangeScan
whenever ANY index exists on the column. Adds range_scan_indexed criterion
workload; docs updated to match."
```

---

## Task 3: UNIQUE constraints (`unique` field modifier, enforced insert/update/upsert, `alter T add unique .col`)

Design (decided): `unique` modifier only — no `primary` keyword this sprint. Uniqueness lives where it already half-exists: `IndexedCol.unique` / persisted `IndexedColMeta`. Declaring `unique` auto-creates a unique B+tree index. Enforcement is in the storage layer (`Table::insert` / update paths) so every write path — plain, prepared (`insert_by_slot`), upsert — passes one choke point. `upsert ... on .col` now REQUIRES `.col` to be unique (breaking change; fixes the known duplicate-id bug). No new `ColumnDef` flag needed.

**Files:**
- Modify: `crates/query/src/token.rs` (new `Token::Unique`; `display_name` at 253-area), `crates/query/src/lexer.rs` (keyword map at ~263), `crates/query/src/canonicalize.rs` (token hash match at ~250–275 — exhaustive, compiler will force the arm; pick the next unused hash byte after auditing `0x7B+`)
- Modify: `crates/query/src/ast.rs` (`FieldDef` gets `unique: bool` at 195–198; `AlterAction::AddUnique { column }` at 31)
- Modify: `crates/query/src/parser.rs` (`parse_create_type` modifier loop at 1771–1777; `parse_alter_table` at 1544–1563; `tokens_to_text` at 1849-area)
- Modify: `crates/query/src/plan.rs` (`CreateTable.fields: Vec<(String, String, bool)>` at 116–119 → `Vec<CreateField>` struct with `name/type_name/required/unique`)
- Modify: `crates/query/src/planner.rs` (CreateType arm), `crates/query/src/executor/plan_exec.rs` (CreateTable arm at 1393; Upsert arm at 695; AlterTable arm at 1418)
- Modify: `crates/storage/src/table.rs` (`Table::insert` at 374 — pre-check BEFORE heap insert; `update`/`update_hinted` at 799/813)
- Test: `crates/query/src/parser.rs` tests, `crates/query/src/executor/tests.rs`, `crates/storage/src/table.rs`/`catalog.rs` tests
- Docs (same commit): `docs/POWQL.md` (type DDL + alter + cheat sheet), `AGENTS.md` (cheat-sheet row + footguns), `site/powql.html`

**Steps:**

- [ ] 1. **Investigation (numbered, explicit):**
   1. Confirm `IndexedColMeta.unique` round-trips through `Catalog::persist()`/`Catalog::open` (read the catalog.bin serializer near catalog.rs:1816 where index-list back-compat is handled). Decision: if `unique` is already serialized, nothing to do; if it is serialized as name-only, extend the record with a trailing unique byte using the same append-only back-compat pattern as the Connect message.
   2. Enumerate every write path that can change an indexed column's value and confirm each flows through `Table::insert`, `Table::update`, or `Table::update_hinted`: the byte-patch fast paths are excluded by construction (`no_indexed` guard at plan_exec.rs:891–893; `has_indexed_col` guard at prepared.rs:225); check `scan_patch_matching_with_hook` (table.rs:767) — if its hook updates indexes, add the same guard there. Decision criteria: any path that can write a duplicate into a unique btree must either be guarded or be unreachable for indexed columns; document the audit in the commit message.
   3. Confirm there is no `drop index` statement (grep `DropIndex` in `crates/query/src`). Consequence: `alter T add unique .col` on a column that already has a NON-unique index must be a clean error ("column already indexed"), not an in-place upgrade.

- [ ] 2. Failing parser tests (style of `test_parse_alter_add_required_column` at parser.rs:2870):

```rust
#[test]
fn test_parse_type_with_unique_modifier() {
    let stmt = parse("type User { required unique email: str, age: int }").unwrap();
    match stmt {
        Statement::CreateType(ct) => {
            assert!(ct.fields[0].required && ct.fields[0].unique);
            assert!(!ct.fields[1].unique);
        }
        other => panic!("expected CreateType, got {other:?}"),
    }
}

#[test]
fn test_parse_alter_add_unique() {
    let stmt = parse("alter User add unique .email").unwrap();
    match stmt {
        Statement::AlterTable(at) => assert!(matches!(
            at.action,
            AlterAction::AddUnique { ref column } if column == "email"
        )),
        other => panic!("expected AlterTable, got {other:?}"),
    }
}
```
Run: `cargo test -p powdb-query test_parse_type_with_unique` → compile error (`no field unique on FieldDef`) — that is the expected failure.

- [ ] 3. Implement lexer/token/AST/parser:
  - `token.rs`: `Unique, // unique` next to `Index` (line 26); `Token::Unique => "'unique'".into()` in `display_name`.
  - `lexer.rs` keyword map (~263): `"unique" => Token::Unique,`.
  - `canonicalize.rs`: add `Token::Unique => hash_byte(h, <next-free-byte>)` (audit the match for the first unused value > 0x7B).
  - `parser.rs::parse_create_type`: replace the single `required` check (1772–1777) with a small modifier loop accepting `required` and `unique` in either order:
    ```rust
    let (mut required, mut unique) = (false, false);
    loop {
        match self.peek() {
            Token::Required => { self.advance(); required = true; }
            Token::Unique => { self.advance(); unique = true; }
            _ => break,
        }
    }
    ```
  - `parse_alter_table`: after `Token::Add` (1545), before the `index` check, handle `Token::Unique` → expect `DotIdent` → `AlterAction::AddUnique { column }`.
  - `tokens_to_text` (1849-area): `Token::Unique => out.push_str("unique")`.
  - `ast.rs`: `FieldDef { name, type_name, required, unique }`; `AlterAction::AddUnique { column: String }`. Fix all construction sites the compiler flags (parser.rs:1797–1801 and tests).

- [ ] 4. Plan + planner + executor DDL: change `PlanNode::CreateTable.fields` to a named struct in `plan.rs`:
```rust
#[derive(Debug, Clone)]
pub struct CreateField {
    pub name: String,
    pub type_name: String,
    pub required: bool,
    pub unique: bool,
}
```
Update the planner's CreateType arm and the executor's CreateTable arm (plan_exec.rs:1393): after `catalog.create_table(schema)`, loop unique fields → `self.catalog.create_index_unique(name, &f.name, true)`. Add the `AlterAction::AddUnique` executor arm (next to AddIndex at 1451): scan the table, collect values into a `std::collections::HashSet`, skip `Value::Empty`; on first duplicate return `Err(QueryError::Execution(format!("cannot add unique on {table}.{column}: duplicate value {v:?} exists")))`; if `tbl.has_index(column)` already → `Err(... "column already indexed")`; else `catalog.create_index_unique(table, column, true)`. (`plan_cache.rs` CreateTable arms at 234/365/711 are `{ .. }` wildcards — no change.)

- [ ] 5. Failing enforcement tests in `executor/tests.rs` (write all five BEFORE the storage change):

```rust
fn unique_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_uniq_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine.execute_powql("type Acct { required unique email: str, id: int }").unwrap();
    engine.execute_powql(r#"insert Acct { email := "a@x.com", id := 1 }"#).unwrap();
    engine
}

#[test]
fn test_unique_dup_insert_rejected() {
    let mut engine = unique_engine();
    let err = engine
        .execute_powql(r#"insert Acct { email := "a@x.com", id := 2 }"#)
        .unwrap_err();
    assert!(err.to_string().contains("unique constraint violation on Acct.email"), "{err}");
    match engine.execute_powql("count(Acct)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
        other => panic!("expected scalar, got {other:?}"),
    }
}

#[test]
fn test_unique_update_into_dup_rejected() {
    let mut engine = unique_engine();
    engine.execute_powql(r#"insert Acct { email := "b@x.com", id := 2 }"#).unwrap();
    let err = engine
        .execute_powql(r#"Acct filter .id = 2 update { email := "a@x.com" }"#)
        .unwrap_err();
    assert!(err.to_string().contains("unique constraint violation"), "{err}");
}

#[test]
fn test_upsert_requires_unique_and_no_dup_ids() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_ups_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine.execute_powql("type W { unique id: int, v: str }").unwrap();
    engine.execute_powql(r#"upsert W on .id { id := 1, v := "first" }"#).unwrap();
    // Known bug regression: a plain insert of the same id must now fail
    // instead of silently creating a second id=1 row.
    assert!(engine.execute_powql(r#"insert W { id := 1, v := "second" }"#).is_err());
    engine.execute_powql(r#"upsert W on .id { id := 1, v := "third" }"#).unwrap();
    match engine.execute_powql("count(W)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 1),
        other => panic!("expected scalar, got {other:?}"),
    }
    // upsert on a NON-unique column is a clean error.
    engine.execute_powql("type W2 { id: int }").unwrap();
    let err = engine.execute_powql("upsert W2 on .id { id := 1 }").unwrap_err();
    assert!(err.to_string().contains("requires a unique column"), "{err}");
}

#[test]
fn test_alter_add_unique_fails_on_existing_dups() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_audup_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine.execute_powql("type L { e: str }").unwrap();
    engine.execute_powql(r#"insert L { e := "x" }"#).unwrap();
    engine.execute_powql(r#"insert L { e := "x" }"#).unwrap();
    assert!(engine.execute_powql("alter L add unique .e").is_err());
}

#[test]
fn test_unique_constraint_survives_reopen() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_uniq_re_{}_{}", std::process::id(), id));
    {
        let mut engine = Engine::new(&dir).unwrap();
        engine.execute_powql("type Acct { required unique email: str }").unwrap();
        engine.execute_powql(r#"insert Acct { email := "a@x.com" }"#).unwrap();
        // Dropped here without explicit checkpoint — recovery path must
        // restore the unique flag from catalog.bin + WAL replay.
    }
    let mut engine = Engine::new(&dir).unwrap();
    assert!(engine.execute_powql(r#"insert Acct { email := "a@x.com" }"#).is_err());
}
```

- [ ] 6. Run: `cargo test -p powdb-query test_unique -- --nocapture` and `test_upsert_requires` — expected failures: dup insert currently succeeds (count = 2), upsert on non-unique col currently succeeds, alter accepts dups (no AddUnique arm yet → compile error first; fix arms in step 4 before this run).

- [ ] 7. Implement storage enforcement in `table.rs`:
  - In `Table::insert` (374): BEFORE `self.heap.insert(...)` (line 381), pre-check unique columns:
    ```rust
    for entry in &self.indexed_cols {
        if !entry.unique { continue; }
        let val = &values[entry.col_idx];
        if !val.is_empty() && entry.btree.lookup(val).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unique constraint violation on {}.{}",
                    self.schema.table_name, entry.col_name),
            ));
        }
    }
    ```
  - In `Table::update` / `update_hinted` (799/813): where the old row's index entries are diffed against new values, add the same check but allow `existing_rid == rid` (updating a row to its own current value is legal). Apply the audit findings from step 1.2 to `scan_patch_matching_with_hook` if needed.
  - Verify the error surfaces usefully: the executor wraps as `QueryError::StorageError(e.to_string())` — check `QueryError`'s `Display` in `result.rs`; the assertion is `contains("unique constraint violation on …")`, so a prefix is acceptable.

- [ ] 8. Implement Upsert gate in plan_exec.rs:695: at the top of the arm (after schema resolution), require `tbl.is_index_unique(key_column) == Some(true)`, else `Err(QueryError::Execution(format!("upsert on .{key_column} requires a unique column (declare it with `unique {key_column}: <type>` or `alter {table} add unique .{key_column}`)")))`. The probe at 739–749 then always uses the unique-btree `index_lookup_all` path; the no-index linear scan branch (750–760) becomes unreachable — delete it.

- [ ] 9. Run all of step 5's tests plus the full `cargo test -p powdb-query` and `cargo test -p powdb-storage`. Existing tests that upsert on non-unique columns (grep `upsert` in `executor/tests.rs` and `clients/ts/test/client.test.ts`) must be updated to declare the key column `unique` — that's part of the breaking change, do it deliberately, and update `clients/ts` tests in this commit too if they exercise upsert against the dev server.

- [ ] 10. Docs in the same commit: `docs/POWQL.md` type/alter sections + cheat-sheet row (`unique email: str` / `CREATE TABLE ... UNIQUE`); `AGENTS.md` cheat-sheet row + a footgun note ("upsert requires the `on` column to be unique since 0.4.7"); `site/powql.html` matching row.

- [ ] 11. Full GATE; commit:
```bash
git add crates docs site AGENTS.md clients/ts
git commit -m "feat(powql): unique constraints — unique field modifier, alter add unique, enforced on insert/update/upsert

Declaring unique auto-creates a unique B+tree index; enforcement is a
storage-layer pre-check before any heap write, so plain, prepared, and
upsert paths share one choke point. upsert on .col now requires .col to
be unique (fixes upsert-then-insert duplicate-id bug). alter T add unique
.col scans for existing duplicates first. Constraint survives restart via
persisted IndexedColMeta + WAL replay index rebuild."
```

---

## Task 4: Parameter binding over the wire (`$1..$N` placeholders)

Design (decided): placeholders are `$1`-style (1-based; `?` is unusable because `??` is the COALESCE token, lexer.rs:302). Binding happens at TOKEN level inside the query crate: lex the template, replace each `Token::Param(n)` with the literal token for `params[n-1]`, then parse normally. Values are never re-lexed or string-interpolated — a string param becomes a `Token::StrLit` payload byte-for-byte, so injection shapes are inert data. This also sidesteps `Literal` having no Null variant (`Token::Null` substitutes directly). v1 bypasses the plan cache (template caching is a follow-up).

**Files:**
- Modify: `crates/query/src/token.rs` (`Token::Param(u32)`), `crates/query/src/lexer.rs` (`$` + digits), `crates/query/src/canonicalize.rs` (new arm), `crates/query/src/parser.rs` (`pub fn parse_with_params`), `crates/query/src/ast.rs` or `result.rs` (`pub enum ParamValue { Null, Int(i64), Float(f64), Bool(bool), Str(String) }`), `crates/query/src/executor/mod.rs` (`execute_powql_with_params` + `execute_powql_readonly_with_params`, mirroring the non-cached paths at 475–487 and 556–598)
- Modify: `crates/server/src/protocol.rs` (`MSG_QUERY_PARAMS = 0x04`, `Message::QueryWithParams`), `crates/server/src/handler.rs` (new match arm next to `Message::Query` at 466; `dispatch_query_with_params` beside `dispatch_query` at 262 — parse via `parse_with_params` for role enforcement, then route read/write exactly like the existing fn)
- Modify: `clients/ts/src/protocol.ts` (MSG_QUERY_PARAMS + encode), `clients/ts/src/index.ts` (`query(query, params?, opts?)`), `clients/ts/README.md`
- Test: `crates/query/src/executor/tests.rs`, `crates/server/src/protocol.rs` tests, `clients/ts/test/client.test.ts`, `clients/ts/test/protocol.test.ts`
- Docs: `AGENTS.md:186` (replace the escape-it-yourself paragraph), `docs/POWQL.md` (placeholders note)

**Steps:**

- [ ] 1. Failing engine-level test in `executor/tests.rs`:

```rust
#[test]
fn test_params_bind_injection_shaped_strings_byte_faithfully() {
    let mut engine = test_engine();
    let evil = r#"x"; drop User; filter .age > "0"#;
    engine
        .execute_powql_with_params(
            "insert User { name := $1, email := $2, age := $3 }",
            &[
                ParamValue::Str(evil.to_string()),
                ParamValue::Str("e@x.com".into()),
                ParamValue::Int(40),
            ],
        )
        .unwrap();
    let r = engine
        .execute_powql_with_params(
            "User filter .email = $1 { .name }",
            &[ParamValue::Str("e@x.com".into())],
        )
        .unwrap();
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Str(evil.to_string()));
        }
        other => panic!("expected rows, got {other:?}"),
    }
    // Table survived; 4 rows total.
    match engine.execute_powql("count(User)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 4),
        other => panic!("{other:?}"),
    }
}

#[test]
fn test_params_errors() {
    let mut engine = test_engine();
    // Out-of-range and unbound placeholders are clean errors.
    assert!(engine.execute_powql_with_params("User filter .age > $2", &[ParamValue::Int(1)]).is_err());
    assert!(engine.execute_powql("User filter .age > $1").is_err()); // no-params API
    // Null param round-trips as PowQL null.
    engine.execute_powql_with_params(
        "insert User { name := $1, email := $2, age := $3 }",
        &[ParamValue::Str("N".into()), ParamValue::Str("n@x.com".into()), ParamValue::Null],
    ).unwrap();
    match engine.execute_powql("User filter .age = null { .name }").unwrap() {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("{other:?}"),
    }
}
```
Run `cargo test -p powdb-query test_params` → compile errors (no `ParamValue`, no method) — expected.

- [ ] 2. Implement the query-crate half:
  - `token.rs`: `Param(u32), // $1` + `display_name` → `format!("parameter ${n}")`.
  - `lexer.rs`: on `'$'`, consume digits; empty digits or value 0 → lex error `"expected parameter number after '$'"`; emit `Token::Param(n)`.
  - `canonicalize.rs`: `Token::Param(n) => { hash_byte(h, <next free byte>); hash for n bytes }` — match how `IntLit` is hashed in that file (read the IntLit arm and mirror its structure).
  - `parser.rs`:
    ```rust
    pub fn parse_with_params(input: &str, params: &[ParamValue]) -> Result<Statement, ParseError> {
        let mut tokens = lex(input).map_err(|e| ParseError::Lex { message: e.message, position: e.position })?;
        for tok in tokens.iter_mut() {
            if let Token::Param(n) = tok {
                let idx = (*n as usize) - 1;
                let p = params.get(idx).ok_or_else(|| ParseError::Syntax {
                    message: format!("query references ${n} but only {} parameter(s) were supplied", params.len()),
                })?;
                *tok = match p {
                    ParamValue::Null => Token::Null,
                    ParamValue::Int(v) => Token::IntLit(*v),
                    ParamValue::Float(v) => Token::FloatLit(*v),
                    ParamValue::Bool(v) => Token::BoolLit(*v),
                    ParamValue::Str(s) => Token::StrLit(s.clone()),
                };
            }
        }
        // …construct Parser { tokens, pos: 0, depth: 0 }, parse_statement, trailing-token check —
        // factor the shared tail of `parse` (parser.rs:99-124) into a helper used by both.
    }
    ```
    (Verify the exact string-token variant name — `tokens_to_text` at parser.rs:1814+ shows the canonical spellings; use what `token.rs:6-12` declares.)
    In plain `parse`, leave `Token::Param` unhandled by expression parsing so it produces the existing "unexpected token" error path; the new `display_name` makes the error self-explanatory.
  - `executor/mod.rs`: `pub fn execute_powql_with_params(&mut self, input: &str, params: &[ParamValue])` = `parse_with_params` → `planner::plan_statement` → `lower_unindexed_scans` → `execute_plan` → `sync_wal` if `!self.in_transaction` (clone of the lex-error fallback path at 475–487). `pub fn execute_powql_readonly_with_params(&self, ...)` mirrors 556–598: parse, `is_read_only_statement` check → `ReadonlyNeedsWrite`, plan, lower, `execute_plan_readonly`. No plan-cache interaction.

- [ ] 3. Run step 1 tests → green. `cargo test -p powdb-query` → green (canonicalize/lexer tests unaffected).

- [ ] 4. Failing protocol round-trip test in `protocol.rs` tests:

```rust
#[test]
fn test_encode_decode_query_with_params() {
    let msg = Message::QueryWithParams {
        query: "insert User { name := $1, age := $2, ok := $3, note := $4 }".into(),
        params: vec![
            WireParam::Str(r#"a"b\c; drop User"#.into()),
            WireParam::Int(-7),
            WireParam::Bool(true),
            WireParam::Null,
        ],
    };
    let bytes = msg.encode();
    match Message::decode(&bytes).unwrap() {
        Message::QueryWithParams { query, params } => {
            assert!(query.contains("$1"));
            assert_eq!(params.len(), 4);
            assert!(matches!(&params[0], WireParam::Str(s) if s == r#"a"b\c; drop User"#));
        }
        other => panic!("expected QueryWithParams, got {other:?}"),
    }
}
```
Plus extend `test_decode_garbage_never_panics` with truncated `0x04` frames (bad tag byte, truncated i64, truncated string).

- [ ] 5. Implement protocol + handler:
  - `protocol.rs`: `const MSG_QUERY_PARAMS: u8 = 0x04;`; `pub enum WireParam { Null, Int(i64), Float(f64), Bool(bool), Str(String) }`; encode = query string, `u16` count LE, then per param `tag u8` (0 null, 1 int + 8B LE, 2 float + 8B LE, 3 bool + 1B, 4 str + length-prefixed). Decode strictly, unknown tag → `Err("unknown param tag")`. Version-gating: this is a NEW message type — old clients never send it (unchanged frames), old servers answer it with the existing `unknown message type: 0x4` error; no existing message changes shape.
  - `handler.rs`: `Message::QueryWithParams { query, params }` arm cloned from the `Query` arm at 466–501 (same `MAX_QUERY_LENGTH` check, same `spawn_blocking` + timeout), calling a new `dispatch_query_with_params(&engine, &query, &params, principal)` that converts `WireParam` → `ParamValue`, parses once via `parse_with_params` for `check_statement_permitted` + `is_read_only_statement`, then `execute_powql_readonly_with_params` under `.read()` with `ReadonlyNeedsWrite` escalation to `.write()` + `execute_powql_with_params` — structurally identical to `dispatch_query` (262–297).

- [ ] 6. `cargo test -p powdb-server` → green; full GATE; commit the Rust half:
```bash
git add crates/query crates/server docs/POWQL.md AGENTS.md
git commit -m "feat(query,server): parameter binding — \$N placeholders bound at token level, QueryWithParams wire message

Params are substituted as literal tokens before parsing (never re-lexed,
never string-interpolated), so untrusted input cannot change query shape.
New MSG_QUERY_PARAMS (0x04) is a pure protocol addition; existing
messages and old clients are untouched."
```
(AGENTS.md edit: replace the "**No parameter binding yet.** … escape it yourself" paragraph with a `$1` usage example; POWQL.md gets a placeholders subsection.)

- [ ] 7. TS client: failing tests first — in `clients/ts/test/client.test.ts` (existing homegrown `test()` harness):
```ts
await test("query with params stores injection-shaped strings byte-faithfully", async () => {
  await client.query(`type ${tbl("P")} { required name: str, age: int }`);
  const evil = `x"; drop ${tbl("P")}; filter .age > "0`;
  const ins = await client.query(`insert ${tbl("P")} { name := $1, age := $2 }`, [evil, 9]);
  assert.equal(ins.kind, "ok");
  const r = await client.query(`${tbl("P")} filter .age = $1 { .name }`, [9]);
  assert.equal(r.kind, "rows");
  if (r.kind === "rows") assert.deepEqual(r.rows, [[evil]]);
});
await test("old no-params query path still works", async () => {
  const r = await client.query(`${tbl("P")} { .name }`);
  assert.equal(r.kind, "rows");
});
```
Plus a `protocol.test.ts` encode/decode round-trip for the new frame (existing `tryDecode` style). Implement: `protocol.ts` add `MSG_QUERY_PARAMS = 0x04`, a `QueryWithParams` message variant and encode branch; `index.ts`:
```ts
export type QueryParam = string | number | boolean | null;
async query(query: string, paramsOrOpts?: QueryParam[] | { signal?: AbortSignal }, maybeOpts?: { signal?: AbortSignal }): Promise<QueryResult>
```
with `Array.isArray(paramsOrOpts)` disambiguation (back-compat for the existing 2-arg opts form); numbers encode as int when `Number.isInteger`, float otherwise. Run:
```bash
cd clients/ts && pnpm build && pnpm test && pnpm test:protocol
```
Update `clients/ts/README.md` (params example + "requires powdb-server >= 0.4.7 for the params form") in the same commit. Commit:
```bash
git add clients/ts
git commit -m "feat(ts-client): client.query(powql, params) using QueryWithParams wire message"
```

---

## Task 5: Multi-line REPL input

**Files:**
- Modify: `crates/cli/src/main.rs` (embedded loop at 919–933; remote loop at 1092+; new `needs_continuation` helper + `#[cfg(test)]` module)
- Modify docs: `AGENTS.md:161` ("The REPL is line-oriented…"), `docs/getting-started.md` if it repeats the claim

**Steps:**

- [ ] 1. **Investigation:** confirm there is no CLI test harness (`ls crates/cli` shows only `src/main.rs`; check `Cargo.toml` for `[[test]]`). Decision: unit-test the helper inside `main.rs`; no integration harness this sprint. Also read the lexer's string rules (`crates/query/src/lexer.rs`, string branch) to match escape handling exactly — decision criteria: if the lexer supports `\"` escapes inside strings, `needs_continuation` must skip a quote preceded by a backslash; if it does not, a bare `"` always toggles.

- [ ] 2. Failing unit tests at the bottom of `main.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::needs_continuation;

    #[test]
    fn continuation_tracking() {
        assert!(needs_continuation("type User {"));
        assert!(needs_continuation("type User {\n  required name: str,"));
        assert!(!needs_continuation("type User { required name: str }"));
        // Brace inside a string literal must not count.
        assert!(!needs_continuation(r#"insert U { s := "}" }"#));
        assert!(needs_continuation(r#"insert U { s := "}" "#));
        // Parens.
        assert!(needs_continuation("count(User filter ("));
        assert!(!needs_continuation("count(User)"));
        // Nested.
        assert!(needs_continuation("insert U { a := (1 + "));
        // Over-closed input is NOT a continuation — let the parser error.
        assert!(!needs_continuation("User }"));
    }
}
```
Run: `cargo test -p powdb-cli` → compile error (no `needs_continuation`) — expected.

- [ ] 3. Implement:

```rust
/// True when `buffer` has unbalanced `{`/`(` outside string literals,
/// i.e. the REPL should read another line before executing.
fn needs_continuation(buffer: &str) -> bool {
    let mut depth: i64 = 0;
    let mut in_str = false;
    let mut chars = buffer.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_str => in_str = false,
            '"' => in_str = true,
            '\\' if in_str => { chars.next(); } // adjust per step-1 lexer findings
            '{' | '(' if !in_str => depth += 1,
            '}' | ')' if !in_str => depth -= 1,
            _ => {}
        }
    }
    depth > 0 && !in_str
}
```
Wire into `run_embedded` (919) and `run_remote` (1092): keep a `String` buffer; prompt `"powql> "` when empty, `"  ...> "` otherwise; on each line, append + `\n`, and only execute when `!needs_continuation(&buffer)`; meta-commands (`.help` etc., 938) only recognized when the buffer is empty; `ReadlineError::Interrupted` clears the buffer and continues; add the full multi-line statement (joined) to history, not the fragments.

- [ ] 4. Manual verification (real terminal): paste the multi-line `type` example from `docs/POWQL.md` and a multi-line `insert` into `cargo run -p powdb-cli` and confirm execution on the closing brace.

- [ ] 5. Update `AGENTS.md:161` to: "The REPL buffers lines until braces/parens balance — multi-line `type`/`insert` paste works; a statement still cannot span two separately-submitted balanced lines." Same-commit doc rule applies.

- [ ] 6. Full GATE; commit:
```bash
git add crates/cli AGENTS.md docs
git commit -m "feat(cli): multi-line REPL input — buffer until braces/parens balance outside string literals"
```

---

## Task 6: Agent-DX falsification eval (10-table schema)

Scaffolding + docs only; no model calls anywhere in CI. Runner is Python 3 stdlib (no new repo deps; jq not guaranteed on runners).

**Files:**
- Create: `scripts/agent-eval/README.md` (how to run with ANY model given only AGENTS.md; baseline procedure vs SQLite)
- Create: `scripts/agent-eval/schema.powql` (10 tables: users, orders, order_items, products, categories, reviews, addresses, payments, sessions, inventory — using `type`, `required`, `unique` where natural, e.g. `unique email` on users after Task 3)
- Create: `scripts/agent-eval/seed.powql` (deterministic seed rows, enough for non-trivial group/join answers)
- Create: `scripts/agent-eval/sqlite-baseline/schema.sql` + `seed.sql` (same data, for the comparison number)
- Create: `scripts/agent-eval/tasks.json` (~25 tasks)
- Create: `scripts/agent-eval/setup.sh` (builds CLI, creates pristine seeded data dir)
- Create: `scripts/agent-eval/run.py` (scores a candidates JSONL offline)

**Steps:**

- [ ] 1. `setup.sh`: `cargo build --release -p powdb-cli`, then create `scripts/agent-eval/.golden-data/` by streaming `schema.powql` + `seed.powql` one statement per line through `target/release/powdb-cli --data-dir scripts/agent-eval/.golden-data --exec "<stmt>"` (the `--exec` one-shot path exists, main.rs:541-543; one process per statement is fine at seed scale — if startup cost bites, batch via the REPL stdin once Task 5 lands). `.golden-data/` is gitignored (add to `.gitignore`).

- [ ] 2. `tasks.json` schema (each entry):
```json
{
  "id": "agg-03",
  "prompt": "How many orders does each city have? Return city and order count, only cities with at least 2 orders.",
  "tables_hint": ["users", "orders", "addresses"],
  "check": { "type": "rowcount", "expected": 3 }
}
```
`check.type` ∈ `rowcount` | `scalar` (exact string compare of the single value) | `rows` (sorted exact match, small results only) | `error` (statement must be rejected — e.g. the unique-violation and `count:`-alias-fails tasks). ~25 tasks covering the gotcha list from AGENTS.md: `:=` vs `=`, `type` not `create table`, aliases (`n: count(.name)` — plus one `error` task asserting `count: count(.name)` fails), group+having, inner/left join with table-order note, IN-subquery, upsert (requires unique after Task 3), null checks (`= null`), between, distinct, transactions (begin/insert/rollback then count), `alter add column` / `add index` / `add unique`, order+limit+offset, case expression, parameterless count(*).

- [ ] 3. `run.py` (stdlib only): for each line of `candidates.jsonl` (`{"task_id": ..., "statement": ...}`), `shutil.copytree(.golden-data, tmpdir)`, `subprocess.run([cli, "--data-dir", tmpdir, "--exec", stmt], capture_output=True, timeout=30)`, parse stdout (the CLI's table/scalar/affected output — implement a tolerant extractor: scalar = last numeric token of last line; rowcount = count of data lines; verify the exact print format from `print_local_result` in main.rs while implementing), score against `check`, emit `results.json` + a pass-rate summary line per category. Exit code 0 always (scoring tool, not a gate).

- [ ] 4. `README.md` — the harness contract: "Give the model ONLY: AGENTS.md, `schema.powql`, and one task prompt. The model returns exactly one PowQL statement. Append to `candidates.jsonl`. Run `python3 scripts/agent-eval/run.py candidates.jsonl`. Scoring is offline and model-agnostic." Baseline procedure: same prompts against `sqlite-baseline/schema.sql` with the same model, scored with `sqlite3` and the same check semantics; report the two pass rates side by side. Note explicitly: not wired into CI.

- [ ] 5. Smoke-test the harness end to end by hand-writing a known-good `candidates.jsonl` (save as `scripts/agent-eval/examples/golden-candidates.jsonl`) for 3 tasks (copy correct statements from POWQL.md) and confirming `run.py` scores 3/3, plus one deliberately wrong statement scoring 0/1.

- [ ] 6. Update `scripts/README.md` with an `agent-eval/` section. Full GATE (workspace untouched, but run it anyway — the gate is per-task policy); commit:
```bash
git add scripts/agent-eval scripts/README.md .gitignore
git commit -m "feat(scripts): agent-DX falsification eval — 10-table schema, 25 scored tasks, model-agnostic offline runner"
```

---

## Task 7: Integration pass

**Files:**
- Modify: `CHANGELOG.md` (`[Unreleased]` section)

**Steps:**

- [ ] 1. Full workspace gate from a clean state:
```bash
cargo clean -p powdb-query -p powdb-storage -p powdb-server -p powdb-cli
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```
- [ ] 2. TS client gate: `cd clients/ts && pnpm build && pnpm test && pnpm test:protocol && pnpm test:pool`.
- [ ] 3. Sanity perf run (informal, no baselines touched): `cargo run --release -p powdb-compare` — confirm no workload regressed by an order of magnitude vs a pre-sprint run; `cargo bench -p powdb-bench` console-only.
- [ ] 4. Eval harness smoke: `scripts/agent-eval/setup.sh && python3 scripts/agent-eval/run.py scripts/agent-eval/examples/golden-candidates.jsonl`.
- [ ] 5. Update `CHANGELOG.md` `[Unreleased]`: `### Added` — EXPLAIN shows executed (lowered) plan; B+tree range scans on all indexed columns; `unique` constraints (`unique` modifier, `alter T add unique .col`); wire parameter binding (`$N`, new `QueryWithParams` message, TS `query(q, params)`); multi-line REPL input; agent-DX eval harness. `### Changed` (breaking) — `upsert ... on .col` now requires `.col` to be unique.
- [ ] 6. Commit: `git add CHANGELOG.md && git commit -m "chore: easy-wins sprint integration pass — changelog for all six features"`.
- [ ] 7. Do NOT merge or push to `main`; leave `feat/easy-wins-sprint` for review (`superpowers:finishing-a-development-branch`).

---

### Critical Files for Implementation
- `crates/query/src/executor/plan_exec.rs`
- `crates/storage/src/table.rs`
- `crates/storage/src/btree.rs`
- `crates/query/src/parser.rs`
- `crates/server/src/protocol.rs`
