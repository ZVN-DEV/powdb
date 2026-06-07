# PowDB Smoke Audit — 2026-05-26

> **HISTORICAL — superseded as of v0.4.4 (2026-06-05).** This is a point-in-time
> smoke audit of the pre-release 0.4.0 build. Nearly every finding has since been
> resolved: ROLLBACK now undoes heap writes (verified by transaction tests in
> `crates/query/src/executor/tests.rs`), the CHANGELOG has entries through 0.4.4,
> all four crates are published on crates.io at 0.4.4, version pins and the MSRV
> are current, and issue/PR templates exist. The three later durability P0s that
> 0.4.4 fixed are not in this audit — see `CHANGELOG.md`. Kept unaltered for
> provenance; do not treat its grades or status claims as current.

## Executive Summary

| Surface | Grade | Verdict |
|---|---|---|
| **Engine (CRUD, aggs, indexes)** | A | Core query engine is solid — all CRUD, aggregates, sorts, indexes, group-by work correctly. Benchmarks match claims. |
| **Transactions (0.4.0 feature)** | F | ROLLBACK is broken — does not undo heap writes. Data integrity bug. |
| **GitHub Pages homepage** | A | Professional landing page, loads fast, good content. Minor favicon 404. |
| **GitHub repo** | B+ | Good metadata, topics, CI badges passing, prebuilt binaries. Missing issue/PR templates. |
| **crates.io (4 crates)** | D | All stuck at v0.3.1, no README displayed, no keywords/categories/homepage. Bad first impression. |
| **npm (@zvndev/powdb-client)** | A | Clean install, comprehensive README, proper metadata, 0 dependencies. |
| **Docker (ghcr.io)** | B+ | Image exists and is public. At v0.3.1. Cannot verify pull without daemon. |
| **docs.rs** | B | All 3 library crates build. powdb-query has no README on crates.io page. |
| **Documentation** | C | POWQL.md is comprehensive but transactions undocumented. Stale version strings throughout. |
| **CI/CD** | A- | Exceptional pipeline (clippy+fmt+test+miri+asan+fuzz+bench gate). Bench currently failing on release/0.4.0. |

---

## Issues by Severity

### 1. ROLLBACK Does Not Undo Writes
**Project:** powdb-storage / transactions
**Severity:** CRITICAL
**Verified:** `begin` -> `insert User { name := "TxTest", age := 99, city := "TX" }` -> `rollback` -> `User filter .name = "TxTest"` returns 1 row (should return 0)
**Root cause:** `Catalog::rollback_to_last_sync()` at `crates/storage/src/catalog.rs:525` re-opens the catalog from disk, but the insert already wrote data directly to the heap file. WAL deferral only delays fsync, not the actual write.
**Impact:** Data integrity bug. Users who rely on transactions for atomicity will lose rollback guarantees.
**Fix:** Rollback must also undo heap writes — either buffer writes until commit, or track and reverse heap mutations on rollback.

### 2. CHANGELOG Missing 0.4.0 Entry
**Project:** Documentation
**Severity:** CRITICAL
**Verified:** `CHANGELOG.md` stops at `[0.3.1] - 2026-05-18`. No 0.4.0 entry exists despite workspace version being 0.4.0.
**Root cause:** Release prep not completed yet (branch is `release/0.4.0`, not merged).
**Impact:** Release-blocking — explicit transactions, ALTER TABLE, and all 0.4.0 changes are undocumented.
**Fix:** Add `## [0.4.0] - YYYY-MM-DD` section documenting all changes since 0.3.1.

### 3. Crates.io Pages Show "No README.md"
**Project:** crates.io (all 4 crates)
**Severity:** HIGH
**Verified:** Screenshot `screenshots/crates-io-powdb-cli.png` — "powdb-cli v0.3.1 appears to have no README.md file"
**Root cause:** README bundling was likely added after 0.3.1 publish. Will be fixed when 0.4.0 publishes.
**Impact:** Every new user who finds PowDB on crates.io sees an empty page. Terrible first impression.
**Fix:** Publish 0.4.0 (local Cargo.toml already has `readme` field).

### 4. All Crates Stuck at v0.3.1
**Project:** crates.io
**Severity:** HIGH
**Verified:** `cargo search powdb-cli` -> `powdb-cli = "0.3.1"`. All 4 crates at 0.3.1, workspace at 0.4.0.
**Root cause:** 0.4.0 release not published yet.
**Impact:** `cargo install powdb-cli` gets stale code missing transactions, ALTER TABLE, and all 0.4.0 fixes.
**Fix:** Ship 0.4.0 release after fixing the ROLLBACK bug.

### 5. Explicit Transactions Undocumented
**Project:** docs/POWQL.md, docs/getting-started.md, README.md
**Severity:** HIGH
**Verified:** `grep -c "BEGIN\|COMMIT\|ROLLBACK\|transaction" docs/POWQL.md` -> 0 matches
**Root cause:** Feature was merged (`a89552c`) but docs not updated.
**Impact:** 0.4.0's headline feature has zero documentation. Users won't know it exists.
**Fix:** Add transactions section to POWQL.md and getting-started.md.

### 6. getting-started.md Says "Rust 1.80+"
**Project:** docs/getting-started.md:16
**Severity:** HIGH
**Verified:** Line 16: `requires Rust 1.80+`. Actual MSRV: `rust-version = "1.93"` in Cargo.toml.
**Root cause:** MSRV was bumped but tutorial not updated.
**Impact:** Users on Rust 1.80-1.92 hit build failures after following the tutorial.
**Fix:** Change `1.80+` to `1.93+` on line 16.

### 7. README Production Checklist Pins v0.3.1
**Project:** README.md:177
**Severity:** HIGH
**Verified:** Line 177: `cargo install powdb-server --version 0.3.1 --locked`
**Root cause:** Not updated for 0.4.0.
**Impact:** Users following production advice install a stale version.
**Fix:** Update to `0.4.0` when released.

### 8. Bench CI Failing on release/0.4.0
**Project:** .github/workflows/bench.yml
**Severity:** HIGH
**Verified:** `gh run list` shows bench conclusion: failure. `powql_filter_only` regressed 11.7% (threshold 10%).
**Root cause:** Performance regression in the filter-only workload, likely from transaction overhead.
**Impact:** Required status check will block merge to main.
**Fix:** Either optimize the regression or update the baseline with `./scripts/update-bench-baseline.sh` if intentional.

### 9. Homepage Claims "8-14x Faster" — README Shows 5.7-9.9x
**Project:** site/index.html
**Severity:** MAJOR
**Verified:** Homepage: `aggregate and scan workloads run 8-14x faster than SQLite`. README benchmark table max is 9.9x (MIN), actual aggregate range is 5.7x-9.9x.
**Root cause:** Aspirational number from early development not updated to match actual benchmarks.
**Impact:** Overstated claim undermines credibility if anyone cross-references.
**Fix:** Change to "up to 10x faster" to match actual data.

### 10. SECURITY.md Supported Versions Table Stale
**Project:** SECURITY.md
**Severity:** MAJOR
**Verified:** Lists 0.3.x and 0.2.x. No 0.4.x entry.
**Root cause:** Not updated for upcoming release.
**Impact:** Users don't know if 0.4.x gets security patches.
**Fix:** Add 0.4.x row when releasing.

### 11. getting-started.md Shows "PowDB v0.2.0" in REPL Output
**Project:** docs/getting-started.md:47,444,449
**Severity:** MAJOR
**Verified:** Line 47: `PowDB v0.2.0 — embedded mode`. Line 444: `PowDB v0.2.0 — remote mode`.
**Root cause:** Tutorial REPL output never updated past v0.2.0.
**Impact:** Confusing — user sees v0.4.0 from `--version` but v0.2.0 in the tutorial.
**Fix:** Update all version strings to `v0.4.0`.

### 12. No Issue Templates or PR Template on GitHub
**Project:** GitHub repo
**Severity:** MAJOR
**Verified:** `.github/ISSUE_TEMPLATE/` directory does not exist. No `PULL_REQUEST_TEMPLATE.md`.
**Root cause:** Never created.
**Impact:** Contributors get a blank text box with no guidance.
**Fix:** Add bug_report.yml and feature_request.yml issue templates + PR template.

### 13. Crates.io Missing Keywords, Categories, Homepage
**Project:** crates.io (all 4 crates)
**Severity:** MAJOR
**Verified:** API returns `keywords: []`, `categories: []`, `homepage: null` for all crates at v0.3.1.
**Root cause:** Added locally after 0.3.1 publish. Will propagate with 0.4.0.
**Impact:** Invisible in crates.io category browsing and search.
**Fix:** Publish 0.4.0.

### 14. type_name_to_id() Silently Maps Unknown Types to Str
**Project:** crates/query/src/executor/compiled.rs:45-56
**Severity:** MAJOR
**Verified:** `type Foo { age: Int }` creates `age` as a string column with no error. Only lowercase `int` works.
**Root cause:** Match arms are lowercase-only with `_ => TypeId::Str` catch-all.
**Impact:** Silent data corruption — capitalized type names create wrong column types.
**Fix:** Either make matching case-insensitive or return an error on unknown type names.

### 15. Update/Delete Error Message Unhelpful
**Project:** PowQL parser
**Severity:** MAJOR
**Verified:** `update User filter .name = "Alice" { age := 31 }` fails with `expected statement, got 'update'`. No hint about correct pipeline syntax.
**Root cause:** Pipeline design puts mutation verbs at the end. Error doesn't suggest correct form.
**Impact:** Every SQL user's first instinct will fail with an unhelpful error.
**Fix:** Improve error message to suggest correct pipeline syntax when `update`/`delete` appears at statement start.

### 16. CONTRIBUTING.md Says "4 Status Checks" — There Are 6+
**Project:** CONTRIBUTING.md
**Severity:** MINOR
**Verified:** Lists 6+ items described as "4 status checks."
**Root cause:** Count not updated as checks were added.
**Fix:** Update the count.

### 17. DDL Response Differs Between Embedded and Remote Mode
**Project:** powdb-server
**Severity:** MINOR
**Verified:** Embedded: `type Item created`. Remote: `0 rows affected`.
**Root cause:** Server wire protocol returns row-affected count; CLI formats it differently.
**Fix:** Have the server return the same descriptive message.

### 18. Meta-commands Fail with -c Flag
**Project:** powdb-cli
**Severity:** MINOR
**Verified:** `powdb-cli -c ".schema User"` returns `expected statement, got field '.schema'`
**Root cause:** Meta-commands are REPL-only by design. Error message doesn't indicate this.
**Fix:** Detect `.` prefix in `-c` mode and show a helpful message.

### 19. Server/Client Version Tracks Diverge
**Project:** npm + crates.io
**Severity:** MINOR
**Verified:** Server crate: v0.3.1. npm client: v0.3.3.
**Root cause:** Independent release cycles.
**Fix:** Document version compatibility policy.

### 20. Missing Favicon on GitHub Pages
**Project:** site/
**Severity:** MINOR
**Verified:** Console error: `Failed to load resource: 404 @ https://zvndev.github.io/favicon.ico`
**Fix:** Add a favicon.

### 21. No Windows Release Binary
**Project:** .github/workflows/release.yml
**Severity:** MINOR
**Verified:** Only linux-x86_64 and macos-aarch64 targets in the matrix.
**Fix:** Add `x86_64-pc-windows-msvc` to the release matrix when ready.

### 22. PowQL HAVING Example Inconsistency
**Project:** README.md vs docs/POWQL.md
**Severity:** COSMETIC
**Verified:** README: `having count(*) > 5`. POWQL.md cheat sheet: `having count(.name) > 5`.
**Fix:** Align examples.

### 23. GitHub Profile Metadata Sparse
**Project:** github.com/zvndev
**Severity:** COSMETIC
**Verified:** No bio, no website link, no company/location set.
**Fix:** Add bio and website link.

---

## What's Working Well

- **Core engine is rock-solid** — All CRUD, aggregates, indexes, group-by, sort, limit, DISTINCT, BETWEEN, IN, LIKE, IS NULL work correctly
- **Benchmark numbers are honest** — Actual results match or exceed README claims (3-10x range verified)
- **CI pipeline is exceptional** — clippy + fmt + test + miri + asan + fuzz + bench regression gate + MSRV check + cargo audit, all with SHA-pinned actions
- **GitHub Pages site is polished** — Professional landing page with benchmarks, syntax comparisons, feature cards
- **npm client is best-in-class** — Zero deps, comprehensive README, TypeScript types, proper metadata
- **Error messages are mostly helpful** — Duplicate tables, unknown columns, SQL rejection all give clear errors
- **Release engineering is mature** — Publish workflow with dependency ordering, release workflow with binaries + Docker + GHCR
- **PowQL.md is comprehensive** — Covers the full language including joins, subqueries, window functions, UPSERT, EXPLAIN
- **Server mode works** — TCP server with password auth, TLS warning, remote CLI connection all functional
- **Advanced features work** — ALTER ADD/DROP COLUMN, required fields, NULL handling, HAVING in both positions

---

## Fix Plan

### Do Now (release-blocking)

1. **Fix ROLLBACK bug**
   File: `crates/storage/src/catalog.rs` (rollback_to_last_sync) + heap write path
   Issue: Heap writes are applied immediately; rollback only re-opens catalog
   Fix: Buffer writes until COMMIT, or track and reverse heap mutations on rollback

2. **Fix type_name_to_id catch-all**
   File: `crates/query/src/executor/compiled.rs:45-56`
   From: `_ => TypeId::Str`
   To: Case-insensitive match or return `Result<TypeId, Error>`

3. **Fix bench regression**
   Command: `./scripts/update-bench-baseline.sh` (if intentional) or optimize

4. **Add CHANGELOG 0.4.0 entry**
   File: `CHANGELOG.md`

5. **Fix getting-started.md MSRV**
   File: `docs/getting-started.md:16`
   From: `Rust 1.80+` To: `Rust 1.93+`

6. **Fix getting-started.md version strings**
   File: `docs/getting-started.md:47,444,449`
   From: `PowDB v0.2.0` To: `PowDB v0.4.0`

7. **Update SECURITY.md versions table**
   File: `SECURITY.md` — add 0.4.x row

### Do This Week (before or with 0.4.0 publish)

8. **Document transactions in POWQL.md** — Add BEGIN/COMMIT/ROLLBACK section
9. **Fix homepage "8-14x" claim** — `site/index.html` -> "up to 10x faster"
10. **Update README version pin** — `README.md:177` -> `0.4.0`
11. **Publish 0.4.0 to crates.io** — Fixes README, keywords, categories, homepage in one shot
12. **Add issue templates** — `.github/ISSUE_TEMPLATE/`
13. **Add PR template** — `.github/PULL_REQUEST_TEMPLATE.md`
14. **Add favicon** — `site/favicon.ico`

### Do When Ready

15. **Improve update/delete error messages** — Suggest pipeline syntax
16. **Improve meta-command error in -c mode** — Explain REPL-only limitation
17. **Align DDL responses between embedded/remote**
18. **Add Windows release binary**
19. **Fix CONTRIBUTING.md status check count**
20. **Fill out GitHub profile metadata**

---

## Implementation Checklist

- [ ] 1. Fix ROLLBACK to actually undo heap writes (`crates/storage/src/catalog.rs`)
- [ ] 2. Fix `type_name_to_id` catch-all at `crates/query/src/executor/compiled.rs:45-56`
- [ ] 3. Fix or baseline the bench regression (`powql_filter_only` +11.7%)
- [ ] 4. Add CHANGELOG 0.4.0 entry
- [ ] 5. Fix MSRV in `docs/getting-started.md:16` — change `1.80+` to `1.93+`
- [ ] 6. Fix version strings in `docs/getting-started.md:47,444,449` — change `v0.2.0` to `v0.4.0`
- [ ] 7. Update SECURITY.md supported versions table
- [ ] 8. Document BEGIN/COMMIT/ROLLBACK in `docs/POWQL.md`
- [ ] 9. Fix "8-14x" claim in `site/index.html` to match actual data
- [ ] 10. Update README.md:177 version pin from `0.3.1` to `0.4.0`
- [ ] 11. Publish 0.4.0 to crates.io (fixes README, keywords, categories, homepage)
- [ ] 12. Add GitHub issue templates
- [ ] 13. Add GitHub PR template
- [ ] 14. Add favicon to `site/`
- [ ] 15. Improve parser error when update/delete at statement start
- [ ] 16. Improve `.schema` error message in `-c` mode
- [ ] 17. Align DDL responses between embedded/remote modes
- [ ] 18. Add Windows target to release workflow
- [ ] 19. Fix CONTRIBUTING.md status check count
- [ ] 20. Fill out GitHub profile metadata

---

## Screenshots

All 12 screenshots saved to `screenshots/`:
- `github-pages-homepage.png` — Landing page (polished, professional)
- `github-repo.png` — Repository page (good metadata)
- `github-org.png` — User profile page
- `github-releases.png` — Releases page (v0.3.1 latest)
- `crates-io-powdb-cli.png` — Shows "No README.md" (bad first impression)
- `docs-rs-powdb-query.png` — API docs (working)
- `docs-rs-powdb-server.png` — API docs (working)
- `docs-rs-powdb-storage.png` — API docs (working)
- `npm-powdb-client.png` — Comprehensive README (great first impression)
- `ghcr-docker-package.png` — Public, v0.3.1
- `getting-started-page.png` — Tutorial page
- `powql-reference-page.png` — Language reference page
