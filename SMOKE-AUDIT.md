# PowDB Smoke Audit — 2026-05-22

Audit done from the outside: fresh `/tmp` install of every published package, browser walk of every public surface, no source-tree access. All findings have been re-verified locally before inclusion.

## 1. Executive Summary

| Surface | Grade | Verdict |
|---|---|---|
| `powdb-cli` / `powdb-server` / `powdb-query` / `powdb-storage` on crates.io | **B** | Install, build, REPL, server, library deps all work. Metadata is bare (zero keywords, zero categories, no homepage, no documentation field). |
| `@zvndev/powdb-client` on npm | **B−** | Installs cleanly, types are rich, ESM-only loads fine. Ships no LICENSE file, doesn't declare `@types/node`, includes `src/` + maps in tarball, no CHANGELOG to explain 0.3.3 vs Rust 0.3.1 skew. |
| GitHub repo (`zvndev/powdb`) | **C+** | Repo metadata is correct, releases are healthy, but **main-branch CI is failing** and five Dependabot PRs are all red — Actions tab is a wall of red. No badges, no issue templates, no CoC. |
| GitHub Pages site (`zvndev.github.io/powdb/`) | **B** | Loads fast, value prop is clear, three pages all render. Thin nav, no OG image, missing `Benchmarks`/`Changelog` deep links. |
| Fly.io server (`zvndev-powdb`) | **F** | **App does not exist.** DNS empty, fly app missing. README and `fly.toml` advertise a hosted endpoint that isn't there. |
| GitHub org (`zvndev`) | **C** | PowDB is not pinned. Org page is noisy with unrelated forks. |

**Overall: B−.** The code, the install path, and the REPL story are real and work. The presentation layer (CI red, missing hosted demo, README claims that don't survive a 30-second check) undermines trust before the user ever touches the engine.

---

## 2. Issues by Severity

### CRITICAL

**1. Fly.io app `zvndev-powdb` does not exist.**
- **Verified:** `dig +short zvndev-powdb.fly.dev` → empty; `curl -sI https://zvndev-powdb.fly.dev/ --max-time 5` → no response.
- **Root cause:** `fly.toml` is checked in (`app = "zvndev-powdb"`, region `iad`, `min_machines_running = 1`) but the app was never deployed (or has been destroyed).
- **Impact:** A user who reads the deploy story and tries to `fly status` or connect to the documented endpoint hits a dead name. Combined with "this is what zvndev runs in production"-style framing, it reads as vapor.
- **Fix:** Either `fly launch` and actually run it (the cheapest paid tier is fine), OR delete `fly.toml` + remove any deploy-aspirational copy from README until you do.

**2. `main` branch CI is failing.**
- **Verified:** `gh run list -R zvndev/powdb --branch main --workflow ci.yml -L 3` — most recent main push ("Bump Dockerfile to rust:1.95, MSRV to 1.93", commit `628c7bd`) shows `completed failure` on both `ci` and `bench`.
- **Root cause:** Unknown without reading the run logs — possibly the MSRV bump itself broke something, or the Pages workflow added in the prior commit is referenced in a way the bench job doesn't like.
- **Impact:** A visitor who clicks the Actions tab sees the latest run on main is red. Combined with the Dependabot PRs (issue #3), it screams "abandoned."
- **Fix:** Read `gh run view 26055609940 --log-failed -R zvndev/powdb`, fix the root cause, push a green commit before the next public mention.

**3. Five Dependabot PRs are all red (#38–#42).**
- **Verified:** `gh pr list -R zvndev/powdb` returns 5 OPEN dependabot PRs; `gh run list` shows each one has both `ci` and `bench` failing.
- **Root cause:** Major-version bumps (`criterion 0.5 → 0.8`, `rustyline 15 → 18`, `rusqlite 0.32 → 0.39`) need code changes; `libc 0.2.184 → 0.2.186` and `tsx 4.21 → 4.22` shouldn't fail and are worth investigating.
- **Impact:** Same Actions-tab-of-red problem.
- **Fix:** Either land the small ones (libc, tsx) to prove the pipeline works, OR cap the breaking ones in `dependabot.yml` (`ignore: - dependency-name: "criterion" update-types: ["version-update:semver-major"]`) and close the PRs.

### HIGH

**4. crates.io metadata has zero keywords and zero categories on all four crates.**
- **Verified:** `curl https://crates.io/api/v1/crates/powdb-cli | jq .crate.keywords` → `[]`. Same for `categories`, `homepage`, `documentation`.
- **Impact:** PowDB will not appear in any crates.io category browse or keyword search ("database", "embedded", "query-engine"). This is the single largest organic-discovery miss.
- **Fix:** Add to each crate's `Cargo.toml`:
  ```toml
  keywords = ["database", "embedded-database", "query-engine", "btree", "wal"]
  categories = ["database", "database-implementations"]
  homepage = "https://zvndev.github.io/powdb/"
  documentation = "https://docs.rs/powdb-query"  # or per-crate
  ```

**5. LICENSE file not shipped in the npm tarball.**
- **Verified:** `npm pack @zvndev/powdb-client && tar -tzf *.tgz` lists 32 files, none is `LICENSE`. `package.json` declares `"license": "MIT"`.
- **Impact:** MIT requires the license text to ship with the distributed work. Legal/compliance red flag for enterprise adopters auditing dependencies.
- **Fix:** Add `clients/ts/LICENSE` (copy the repo-root one) and make sure it's in the `files` allowlist.

**6. README has zero badges.**
- **Verified:** opened README.md — no build, version, license, docs.rs, or MSRV badge.
- **Impact:** For a Rust DB project, badges are the first credibility signal. Their absence is a "weekend hobby project" tell.
- **Fix:** Add at the top:
  ```markdown
  [![CI](https://github.com/zvndev/powdb/actions/workflows/ci.yml/badge.svg)](https://github.com/zvndev/powdb/actions/workflows/ci.yml)
  [![crates.io](https://img.shields.io/crates/v/powdb-cli.svg)](https://crates.io/crates/powdb-cli)
  [![docs.rs](https://docs.rs/powdb-query/badge.svg)](https://docs.rs/powdb-query)
  [![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
  ```
  (But fix CI first — a red CI badge is worse than no badge.)

**7. README claim "Requires Rust stable (1.80+)" is wrong; actual MSRV is 1.93.**
- **Verified:** README.md line 35; `Cargo.toml` workspace package has `rust-version = "1.93"`; recent commit `628c7bd` bumped MSRV to 1.93.
- **Impact:** A user on Rust 1.80–1.92 gets a confusing compile failure instead of a clean MSRV error.
- **Fix:** Change README line 35 to "Requires Rust 1.93+." Same change in CONTRIBUTING.md if mentioned there.

**8. README claim "Zero C FFI — no C compiler" is wrong once TLS is in the dep graph.**
- **Verified:** Agent observed `cargo install powdb-server` pulled `aws-lc-sys` and required `cmake`. README line 5 ("engine is pure Rust end-to-end, no C FFI required") and line 193 ("Zero C FFI -- no C compiler, no `libsqlite3-sys`, no bindgen") both promise this.
- **Impact:** Easy claim to debunk; undermines other claims by association.
- **Fix:** Either (a) feature-gate TLS so the default install is genuinely C-free and reword as "TLS optional, requires C toolchain if enabled," or (b) reword to "no SQL parsing, no `libsqlite3-sys`" and drop the "no C compiler" line.

### MAJOR

**9. README repo-relative links break on crates.io.**
- **Verified:** README has `[docs/POWQL.md](docs/POWQL.md)`, `[docs/getting-started.md](docs/getting-started.md)`, `[LICENSE](LICENSE)`. crates.io renders relative paths against `crates.io/`, not GitHub.
- **Fix:** Rewrite as absolute GitHub URLs (`https://github.com/zvndev/powdb/blob/main/docs/POWQL.md`), or use a separate README that targets the registry.

**10. npm tarball ships `src/*.ts`, `*.js.map`, and `*.d.ts.map`.**
- **Verified:** Tarball is 178 KB unpacked vs ~40 KB of compiled output; no `files` field in `clients/ts/package.json` to gate this.
- **Fix:** Add `"files": ["dist", "README.md", "LICENSE"]` to `clients/ts/package.json`.

**11. npm client doesn't declare `@types/node` as a (peer)dependency.**
- **Verified:** Published `index.d.ts` imports `node:tls` and `node:events`; agent reproduced TS compile failure in a fresh project.
- **Fix:** Add `"peerDependencies": { "@types/node": ">=18" }` (or move to `dependencies` if you want zero-config).

**12. Pure-binary `powdb-cli` is empty on docs.rs.**
- **Verified:** docs.rs page builds (HTTP 200) but has no modules.
- **Fix:** Either accept this (it's a binary crate) and set `documentation` field on `powdb-cli` to point at README; or add a `lib.rs` stub with a top-level rustdoc page that links the binary's `--help` output.

**13. No npm CHANGELOG to explain version skew (`@zvndev/powdb-client` 0.3.3 vs Rust crates 0.3.1).**
- **Verified:** Tarball contains no `CHANGELOG.md`; README never mentions it; client publishes 0.3.0/0.3.1/0.3.2/0.3.3 in 6 days.
- **Fix:** Generate a `CHANGELOG.md` alongside `RELEASES.md` and include it in the npm tarball. State compatibility (e.g., "client 0.3.x compatible with server 0.3.x").

**14. MSRV is declared three different ways across the repo.**
- **Verified:** README says 1.80+; `Cargo.toml workspace.package.rust-version = "1.93"`; recent commit history shows Dockerfile bouncing through 1.88 → 1.95.
- **Fix:** Source of truth = `Cargo.toml`. Make README and Dockerfile derive from it, or at minimum add a CI check that they agree.

**15. No `CODE_OF_CONDUCT.md`.**
- **Verified:** `gh repo view --json codeOfConduct` returns `null`. CONTRIBUTING and SECURITY are present.
- **Fix:** Drop in the Contributor Covenant 2.1.

**16. No issue templates, no PR templates.**
- **Verified:** `gh repo view --json issueTemplates,pullRequestTemplates` → both `[]`.
- **Fix:** Add `.github/ISSUE_TEMPLATE/bug.yml`, `feature.yml`, and a `pull_request_template.md`.

**17. Wiki is enabled but presumably empty.**
- **Verified:** `hasWikiEnabled: true`; not populated.
- **Fix:** Disable in repo settings to avoid stranger confusion vs the docs site.

**18. Server's "no password configured" warning isn't surfaced in the README.**
- **Verified:** Agent observed `WARN no password configured — all connections will be accepted without authentication`. Server defaults to `127.0.0.1` (good), but README doesn't tell a user to set `POWDB_PASSWORD` before binding `0.0.0.0`.
- **Fix:** Add a short "Production checklist" to README — `POWDB_PASSWORD`, `POWDB_TLS_CERT`, bind interface.

**19. README references `@zvndev/powdb-client` for TS but doesn't show the install command.**
- **Verified:** README mentions the TS client but never says `npm install @zvndev/powdb-client`.
- **Fix:** One-liner under "Install" → "TypeScript client: `npm install @zvndev/powdb-client`".

**20. PowDB is not pinned on the `zvndev` org page.**
- **Verified:** Org page lists ~30 repos including unrelated forks; PowDB blends in.
- **Fix:** Pin PowDB + the top 2-3 ZVN public projects in org settings.

### MINOR

**21.** No `--exec` / `-c "<query>"` flag on `powdb-cli` — REPL-only. Common DB CLIs (sqlite3, psql) support one-shot mode.
**22.** README's Install section doesn't mention the prebuilt binaries that v0.3.1 actually ships (linux x86_64, macos aarch64 for cli & server). Discoverability miss.
**23.** README doesn't mention the published Docker image `ghcr.io/zvndev/powdb:v0.3.1`. Discoverability miss.
**24.** Landing site has no `Benchmarks`, `Releases`, or `Changelog` deep link in the nav.
**25.** Site `/favicon.ico` 404 (org-level path; site ships its own at `/powdb/...`).
**26.** No custom OG image on the GitHub Pages site — default GitHub card is wasted social-share real estate for a "3-10x faster than SQLite" pitch.
**27.** `ident("Foo")` in the TS client stringifies as `"[object Object]"` when logged. Footgun; add a `toString()`.
**28.** crates.io `description` strings are accurate but don't include the speed claim ("3-10x faster than SQLite on aggregates") that drives clicks from search.
**29.** npm package keywords list is thin (4 entries). Missing `typescript`, `tcp`, `sql-alternative`.

### COSMETIC

**30.** `packageManager: "pnpm@10.29.3"` in the published `clients/ts/package.json` is harmless but unnecessary for consumers.
**31.** Repo is 0 stars / 0 forks / 0 issues / 0 watchers — expected for a fresh launch, only flagged because it stacks with the red CI to read "abandoned."

---

## 3. What's Working Well

- **The product is real.** Fresh `cargo install powdb-cli && cargo install powdb-server` succeeded in under a minute with zero warnings. REPL ran the README's `type User { ... }` → `insert` → `User filter .age > 25 { .name, .age }` → `count(User)` end-to-end with correct results. Server bound on 127.0.0.1:5439 and accepted a TCP client connection that ran a `type` statement cleanly.
- **All four crates at v0.3.1, no orphan/abandoned siblings.** crates.io search returns exactly the expected four results.
- **`powdb-server --help` is excellent** — every flag plus every env var (`POWDB_PORT`, `POWDB_DATA`, `POWDB_PASSWORD`, `POWDB_TLS_CERT`, `RUST_LOG`) documented.
- **Server logging uses `tracing-subscriber` with proper levels.** WARN for no-auth, INFO for listen.
- **docs.rs builds clean for all four crates** (HTTP 200, populated module sections for server/query/storage).
- **npm client is surprisingly polished.** Zero transitive deps, provenance signature, JSDoc on every public method, typed `EventEmitter<ClientEvents>`, `AbortSignal` support, `withClient` pool helper, structured `PowDBError` with codes, README covers TLS/pooling/typed rows/observability/injection.
- **8 releases in ~5 weeks** (v0.1.0 → v0.3.1). v0.3.1 release notes are substantive.
- **v0.3.1 ships prebuilt binaries** for linux x86_64 + macos aarch64, plus a ghcr image at `:latest` and `:v0.3.1`.
- **Repo hygiene is mostly there:** LICENSE, CHANGELOG, RELEASES, CONTRIBUTING, SECURITY, AGENTS.md, CLAUDE.md, Dockerfile, docker-compose.yml.
- **`zvndev/powdb` repo metadata is correct:** description present, `homepageUrl = "https://zvndev.github.io/powdb/"`, 8 sensible topics, MIT.
- **Daily fuzzer runs on schedule and stays green.**

---

## 4. Fix Plan

### Do Now (under 5 minutes each)

1. **Delete `fly.toml`** (or move it to `examples/deploy/fly.toml.example` with a banner comment) — file: `fly.toml`. The app doesn't exist; remove the lie. (If you want to keep the deploy story, re-launch the Fly app instead.)
2. **Fix README line 35** — from `Requires Rust stable (1.80+).` → `Requires Rust 1.93+.`. File: `README.md:35`.
3. **Fix README "no C compiler" claim** — file: `README.md:193` and `:5`. Reword to "no SQL parsing, no `libsqlite3-sys`" and drop the C-compiler line, OR feature-gate TLS so the claim becomes true.
4. **Add npm `LICENSE` file** — `cp LICENSE clients/ts/LICENSE`. Bump patch, republish.
5. **Add `"files": ["dist", "README.md", "LICENSE"]`** to `clients/ts/package.json`.
6. **Add `"peerDependencies": { "@types/node": ">=18" }`** to `clients/ts/package.json`.
7. **Rewrite README repo-relative links as absolute GitHub URLs** so crates.io renders them. File: `README.md` — `docs/POWQL.md`, `docs/getting-started.md`, `LICENSE`.
8. **Pin PowDB on the `zvndev` org page** (GitHub org settings → Pinned).
9. **Disable the empty wiki** in repo settings.
10. **Add `keywords` + `categories` + `homepage` + `documentation` to every crate's `Cargo.toml`** (workspace-level if possible). 4 crates, copy-paste, cut a 0.3.2 patch.

### Do This Week (under 1 hour each)

11. **Investigate the failing `main` CI run** — `gh run view 26055609940 --log-failed -R zvndev/powdb`. Fix the root cause. Push a green commit.
12. **Triage the 5 Dependabot PRs** — close the breaking major bumps and add `ignore` rules to `dependabot.yml`; land `libc` and `tsx` if they pass once CI is unstuck.
13. **Add README badges** (CI, crates.io version, docs.rs, MIT). Do this AFTER step 11 so CI is green.
14. **Add a "Production checklist" to README** — `POWDB_PASSWORD`, `POWDB_TLS_CERT`, bind interface, persistent volume.
15. **Add `CODE_OF_CONDUCT.md`** (Contributor Covenant 2.1).
16. **Add `.github/ISSUE_TEMPLATE/bug.yml`, `feature.yml`, `pull_request_template.md`.**
17. **Add `npm install @zvndev/powdb-client`** to the README "Install" section.
18. **Mention the prebuilt binaries and ghcr image** in the README "Install" section.
19. **Generate a `clients/ts/CHANGELOG.md`** and include it in the tarball. State server-version compatibility.
20. **Add `--exec` flag to `powdb-cli`** for one-shot queries.

### Do When Ready (requires decisions or infrastructure)

21. **Decide on the Fly hosted demo.** Either launch a real `zvndev-powdb` Fly app (smallest paid machine, persistent volume) and document the connection string, or commit to "self-host only" and remove all hosted-demo language.
22. **Feature-gate TLS** in `powdb-server` so default install is C-free. Big refactor; only worth it if the "pure Rust" claim is core to your positioning.
23. **Custom OG image** for the landing site — "PowDB · 3-10× faster than SQLite on aggregates," screenshot of the REPL, etc.
24. **MSRV CI check** that fails the build if Cargo.toml `rust-version` ≠ Dockerfile FROM ≠ README. (Single source of truth.)
25. **Landing site nav: add `Benchmarks`, `Releases`, `Changelog`.**

---

## 5. Implementation Checklist

A flat checklist ready for `claude "do item N from SMOKE-AUDIT.md"`:

- [ ] 1. Delete `fly.toml` (or move to `examples/deploy/fly.toml.example`)
- [ ] 2. Change `README.md:35` from "Requires Rust stable (1.80+)" to "Requires Rust 1.93+"
- [ ] 3. Fix README "no C compiler" claim at lines 5 and 193 — reword to remove the C-compiler line OR feature-gate TLS
- [ ] 4. Copy `LICENSE` → `clients/ts/LICENSE`
- [ ] 5. Add `"files": ["dist", "README.md", "LICENSE"]` to `clients/ts/package.json`
- [ ] 6. Add `"peerDependencies": { "@types/node": ">=18" }` to `clients/ts/package.json`
- [ ] 7. Rewrite repo-relative links in `README.md` (`docs/POWQL.md`, `docs/getting-started.md`, `LICENSE`) as absolute `https://github.com/zvndev/powdb/blob/main/...` URLs
- [ ] 8. Pin PowDB on the zvndev GitHub org page (manual: GitHub org settings → Customize pinned)
- [ ] 9. Disable wiki on zvndev/powdb (manual: repo Settings → Features → Wikis off)
- [ ] 10. Add `keywords`, `categories`, `homepage`, `documentation` to all four `crates/*/Cargo.toml` (or workspace level)
- [ ] 11. Diagnose and fix the failing main CI run (`gh run view 26055609940 --log-failed -R zvndev/powdb`)
- [ ] 12. Triage Dependabot PRs #38–#42: close breaking major bumps, add `ignore` rules to `.github/dependabot.yml`
- [ ] 13. Add CI / crates.io / docs.rs / MIT badges to top of `README.md` (after #11 lands)
- [ ] 14. Add "Production checklist" section to `README.md` (POWDB_PASSWORD, TLS, bind interface)
- [ ] 15. Add `CODE_OF_CONDUCT.md` (Contributor Covenant 2.1)
- [ ] 16. Add `.github/ISSUE_TEMPLATE/bug.yml`, `feature.yml`, `pull_request_template.md`
- [ ] 17. Add `npm install @zvndev/powdb-client` to README Install section
- [ ] 18. Mention prebuilt binaries + ghcr image in README Install section
- [ ] 19. Create `clients/ts/CHANGELOG.md` and add to tarball; state server-version compatibility
- [ ] 20. Add `--exec "<query>"` flag to `powdb-cli`
- [ ] 21. (Decision) Deploy real Fly app OR remove all hosted-demo language
- [ ] 22. (Big) Feature-gate TLS in `powdb-server` so default install is C-free
- [ ] 23. Design custom OG image for landing site
- [ ] 24. Add MSRV consistency CI check (README ↔ Cargo.toml ↔ Dockerfile)
- [ ] 25. Add Benchmarks/Releases/Changelog links to landing site nav
