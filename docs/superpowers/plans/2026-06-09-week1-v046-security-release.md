# Week 1 — v0.4.6 Security Release + TS Client Multi-User Auth

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship v0.4.6 (oversized-row DoS fix + readonly enforcement + NULL wire fix + window-frame fix) to crates.io/ghcr/GitHub, un-strand multi-user auth in the TS client, and clear the release-hygiene backlog.

**Architecture:** All engine fixes are already merged-ready on PR #81. This plan is release engineering plus one contained TS client feature (send the optional username the server protocol already accepts).

**Tech Stack:** Rust workspace (six published crates), TS client (Node ≥22, pnpm, zero deps), GitHub Actions release.yml, ghcr.io, crates.io, npm.

**Hard rules:** Never push to main (branch + PR only). Never rebaseline `baseline/main.json` from a laptop. Smoke-test release artifacts with the documented flow BEFORE publishing (v0.4.1/0.4.2 shipped a P0 because gates were self-referential).

---

### Task 0: Merge PR #81 (BLOCKS EVERYTHING)

- [ ] **Step 1:** Kirby reviews + approves https://github.com/ZVN-DEV/powdb/pull/81 (one approval merges the whole sweep: 2 security fixes + 2 correctness fixes + ~50 doc fixes).
- [ ] **Step 2:** Merge via GitHub (squash or merge per repo convention). Confirm required checks green first.
- [ ] **Step 3:** `git checkout main && git pull` locally.

### Task 1: TS client — send username for multi-user auth

The server's Connect frame already carries an optional username (`crates/server/src/protocol.rs:38-41`: length-prefixed, appended after the password field; `crates/server/src/handler.rs` rejects missing usernames when users are defined). The TS client never sends it, so multi-user servers reject every Node client.

**Files:**
- Modify: `clients/ts/src/protocol.ts` (Connect message variant + encoder, ~line 34 and ~line 56)
- Modify: `clients/ts/src/index.ts` (`ClientOptions` ~line 38, handshake call site)
- Modify: `clients/ts/src/pool.ts` (pass-through option)
- Test: `clients/ts/test/protocol.test.ts`, `clients/ts/test/client.test.ts`

- [ ] **Step 1: Branch** — `git checkout -b feat/ts-client-user-auth`
- [ ] **Step 2: Write failing protocol tests** in `clients/ts/test/protocol.test.ts`, matching the existing `test(name, fn)` style:

```ts
await test("encodes Connect with username after password", async () => {
  const buf = encodeMessage({ type: "Connect", dbName: "main", password: "pw", username: "alice" });
  const decoded = decodeMessage(buf); // round-trip
  assert.equal(decoded.username, "alice");
});
await test("encodes Connect with null username as legacy frame", async () => {
  const buf = encodeMessage({ type: "Connect", dbName: "main", password: "pw", username: null });
  // must be byte-identical to the pre-username frame so old servers accept it
});
```

- [ ] **Step 3:** Run `pnpm test` in `clients/ts` — expect the two new tests to FAIL (unknown field `username`).
- [ ] **Step 4: Implement protocol change** — extend the Connect variant in `protocol.ts`:

```ts
| { type: "Connect"; dbName: string; password: string | null; username: string | null }
```

Encoder: when `username !== null`, append `encodeString(username)` after the password field (mirror the server: it reads username only `if pos < payload.len()`, so omitting the field entirely when null preserves byte-compat with 0.4.x servers and the legacy frame shape). Decoder: parse optional trailing username the same way the server does.

- [ ] **Step 5:** Add `user?: string` to `ClientOptions` in `index.ts`; thread to the handshake (`username: opts.user ?? null`). Same pass-through in `pool.ts` options.
- [ ] **Step 6:** Run `pnpm test` — protocol tests PASS.
- [ ] **Step 7: Live integration test** — start `target/release/powdb-server` (from merged main) with a data dir seeded via `powdb-cli useradd alice --role readwrite --password s3cret` and `useradd bob --role readonly --password hunter2`. Test in `client.test.ts` style: connect as alice → insert OK; connect as bob → select OK, insert rejected with `permission denied` PowDBError (NOT a crash); connect with no user → `auth_failed`. (Server must be ≥0.4.6 for the readonly-rejection assertions.)
- [ ] **Step 8: Tarball hygiene (rides along):** set `"declarationMap": false, "sourceMap": false` in `clients/ts/tsconfig.json` (kills the 12 dead `.map` files contradicting the 0.3.4 changelog claim). Run `npm pack --dry-run` — confirm no `.map` files.
- [ ] **Step 9: Version + docs.** Bump `clients/ts/package.json` to **0.4.0** (semver-minor for a feature in 0.x; also intentionally aligns the client's minor with the server's — note in CHANGELOG). Update `clients/ts/CHANGELOG.md` (username support, map-file removal, compat table: multi-user mode now requires client ≥0.4.0 AND server ≥0.4.6 for enforced roles) and `clients/ts/README.md` (document `user` option with example; remove the multi-user incompatibility caveat added 2026-06-09, replacing it with the version matrix).
- [ ] **Step 10:** Full gate: `pnpm run build && pnpm test` (all suites). Commit: `feat(ts-client): send username for multi-user auth; drop dead source maps (0.4.0)`.
- [ ] **Step 11:** PR → review → merge. **Do not `npm publish` until Task 2's v0.4.6 server is live** (the integration assertions reference enforced roles).

### Task 2: Cut v0.4.6

**Files:**
- Modify: root `Cargo.toml` (workspace version → 0.4.6), `Cargo.lock`
- Modify: `CHANGELOG.md` (promote `[Unreleased]` → `[0.4.6] - <date>`)
- Modify: `SECURITY.md` (supported-versions: add 0.4.6 row, 0.4.5 → superseded; the "ships in the next release" readonly note → "enforced as of 0.4.6")
- Modify: `RELEASES.md` checklist if any step drifted

- [ ] **Step 1:** Branch `release/0.4.6` from updated main. Bump workspace version; `cargo build --workspace` to refresh the lockfile.
- [ ] **Step 2:** CHANGELOG: rename `[Unreleased]` section to `[0.4.6] - <today>`; add fresh empty `[Unreleased]`.
- [ ] **Step 3:** SECURITY.md edits above. Sanity-grep: `grep -rn "0\.4\.5" README.md docs/ site/ examples/` — bump user-facing version pins (README install line, powdb-vs-sqlite pin, aws-ecs image tag, deploy README docker tag) to 0.4.6.
- [ ] **Step 4:** Full gate: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check`.
- [ ] **Step 5: Pre-publish smoke (MANDATORY, non-self-referential):** `cargo build --release`, then with the built binaries in a fresh temp dir run, exactly as documented in README/getting-started: the full PowQL flow (type/insert/filter/group/transactions), kill -9 + restart WAL replay (all rows recovered), **both attack repros** (oversized insert → clean error + server alive; readonly user → writes denied), backup → restore roundtrip.
- [ ] **Step 6:** PR `release/0.4.6` → approve → merge. Tag `v0.4.6` on main; push tag. Confirm `release.yml` builds binaries + pushes `ghcr.io/zvn-dev/powdb:v0.4.6` + `latest`.
- [ ] **Step 7: Publish to crates.io in dependency order:** `powdb-storage` → `powdb-auth` → `powdb-query` → `powdb-backup` → `powdb-server` → `powdb-cli` (per RELEASES.md; wait for each index propagation).
- [ ] **Step 8: Post-publish verification:** on a clean machine/dir, `cargo install powdb-cli --version 0.4.6` and re-run the Step-5 smoke against the *installed* binary. Then `npm publish` the TS client 0.4.0 (Task 1) and re-run its integration script against the published package.
- [ ] **Step 9:** GitHub release notes from CHANGELOG; verify crates.io pages render READMEs and the homepage link works (it was a 404 until this week).

### Task 3: ghcr package visibility

- [ ] **Step 1:** GitHub → zvn-dev org → Packages → `powdb` → Package settings → Change visibility → **Public**. (Leftover from the org transfer; the new package defaulted private.)
- [ ] **Step 2:** Verify logged-out: `docker logout ghcr.io && docker pull ghcr.io/zvn-dev/powdb:v0.4.6` succeeds (or `curl -s https://ghcr.io/v2/zvn-dev/powdb/tags/list` returns 401-with-public-token flow rather than 404).

### Task 4: Hygiene backlog

- [ ] **Step 1:** Review + merge the 5 open dependabot PRs (batch-merge after CI; rebase any that conflict with the sweep merge).
- [ ] **Step 2:** Depot bench migration: install the Depot GitHub app (Kirby, manual), then merge `chore/bench-depot-runner`, run `gh workflow run bench.yml` on the Depot runner, and **rebaseline `baseline/main.json` from that Depot run only** (never from a laptop).
- [ ] **Step 3:** Kirby runs `! flyctl auth login`, then `fly apps list` — confirm no stray PowDB apps (expected: none; fly.toml was always a template).
- [ ] **Step 4:** Delete local test residue now that the sweep is merged: `rm -rf powdb_data clients/ts/powdb_data node_modules` at repo root (all gitignored; root `node_modules` is a stale MCP cache, the real one lives in `clients/ts/`).

### Exit criteria

- v0.4.6 live on crates.io (6 crates), ghcr (public), GitHub releases — verified from a clean install.
- TS client 0.4.0 on npm; a Node client can authenticate to a multi-user server and readonly is enforced end-to-end.
- Zero open dependabot PRs; bench gate running on Depot; no stray deployments anywhere.
