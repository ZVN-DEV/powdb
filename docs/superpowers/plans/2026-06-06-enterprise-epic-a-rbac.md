# Enterprise Epic A — Identity & Access (RBAC → TLS → Audit) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]`. Same Continuous-Verification Protocol as the backup plan: per-task `cargo test --workspace` (stay green) + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo fmt --check`; per-phase the CI bench gate (`bench.yml` on PR) confirms no hot-path regression.

**Goal:** Replace PowDB's single shared password with real per-user identity, roles, and least-privilege — the #1 unanimous enterprise-readiness gap (see `docs/design/2026-06-06-enterprise-readiness-roadmap.md`). Then TLS-everywhere and audit logging build on the principal identity this establishes.

**Architecture:** Three substrates in dependency order — **S1 identity** (users/roles + password hashing, a system catalog), **S2 transport** (TLS on wire + CLI), **S3 accountability** (append-only audit log keyed to the authenticated principal). S1 is the root: audit attribution, RLS, and per-user quotas all key off it.

**Why phased & partly gated:** the wire-protocol auth change is **breaking** (changes the deploy/auth model documented as "one shared password"). The plan lands the **non-breaking foundation first** (Slice 1, fully additive), then **pauses at the breaking server-auth flip for explicit human design sign-off** before changing the handshake.

---

## Current state (verified against source)
- Wire `Connect { db_name, password }` (`crates/server/src/protocol.rs`) → server compares candidate to a single `expected_password` via `constant_time_eq` (sha256, `crates/server/src/handler.rs:72`). Per-IP auth-failure rate limiting exists. No username, no users, no roles.
- `sha2` + `zeroize` already in `crates/server/Cargo.toml`.
- Backward-compat note: old clients may omit the password (`protocol.rs:134`).

---

## Slice 1 — Identity foundation (NON-BREAKING, additive) — THIS SLICE

A new `powdb-auth` crate: argon2 password hashing + a persisted user/role store. **Not yet wired into the server** — pure library + data model, fully unit-tested. Shipping this alone changes no runtime behavior.

### Files
- `crates/auth/Cargo.toml` (new crate `powdb-auth`; deps: `argon2`, `serde`, `serde_json`, `zeroize`)
- `crates/auth/src/lib.rs`
- `crates/auth/src/hash.rs` — argon2 hashing
- `crates/auth/src/store.rs` — user/role model + persistence
- `crates/auth/tests/auth_store.rs`
- root `Cargo.toml` — add member

### Task A1: crate + password hashing (TDD)
- [ ] Test `hash_then_verify_roundtrips` + `verify_rejects_wrong_password` + `two_hashes_of_same_password_differ` (salted).
- [ ] Implement `hash_password(&str) -> String` (argon2id PHC string) and `verify_password(hash: &str, candidate: &str) -> bool` using the `argon2` crate's `PasswordHasher`/`PasswordVerifier` with `OsRng` salt. Never log or `Debug`-print secrets; accept `Zeroizing<String>` candidates where practical.
- [ ] Verify protocol + commit.

### Task A2: Role + permission model (TDD)
- [ ] `Permission` (enum: `Read`, `Write`, `Ddl`, `Admin` — coarse to start), `Role { name, permissions: BTreeSet<Permission> }`, builtin roles `admin` (all), `readwrite` (Read+Write), `readonly` (Read).
- [ ] Test: builtin roles expose expected permissions; `role.allows(Permission)` correct.

### Task A3: User store with persistence (TDD)
- [ ] `User { name, password_hash, role: String }`; `UserStore { users: Map<String,User> }` serialized to `auth.json` in the data dir (0600 perms where supported).
- [ ] Methods: `create_user(name, password, role) -> Result` (rejects dup), `authenticate(name, candidate) -> Option<&User>` (argon2 verify), `set_role`, `delete_user`, `list_users` (no hashes leaked), `load(dir)`/`save(dir)`.
- [ ] Tests: create→authenticate roundtrip; wrong password fails; unknown user fails; persistence (save→load→authenticate); duplicate rejected; `auth.json` never contains plaintext.

### Slice 1 exit
- [ ] Full workspace suite green (new crate adds tests, nothing else changes); clippy/fmt clean. New crate is not referenced by server/cli yet → zero runtime/behavior change. PR.

---

## Slice 2 — Server auth enforcement (BREAKING — REQUIRES HUMAN DESIGN SIGN-OFF BEFORE IMPLEMENTING)

> ⚠️ Do not implement Slice 2 without explicit approval of this design. It changes the wire protocol and the documented auth model.

Proposed design (for review):
- Extend `Connect` with an optional `username`. New wire field, version-negotiated; keep backward compatibility.
- Server startup: if `auth.json` has users → authenticate `(username, password)` against the `UserStore`; bind the session's **principal + role**. If NO users exist → fall back to today's single-shared-password behavior (so existing deployments keep working). A bootstrap path creates the first `admin` user (e.g. `POWDB_ADMIN_PASSWORD` on first start, or a `powdb-cli useradd` against the embedded dir).
- Keep the per-IP rate limiter; switch the comparison to `UserStore::authenticate` (argon2) when users exist.
- CLI `--user <name> --password <...>`; admin commands `useradd`/`userdel`/`passwd`/`roles` (embedded or over an authed admin session).
- Tasks (expanded after sign-off): protocol field + version negotiation; handler auth path; bootstrap admin; CLI flags + user-admin commands; backward-compat tests; docs (deploy model update).

---

## Slice 3 — RBAC enforcement + audit (after Slice 2)
- Executor checks the session principal's role permission per operation (Read for queries, Write for insert/update/delete, Ddl for type/alter/drop, Admin for user mgmt). Deny → clean error, audited.
- **TLS (S2 of the roadmap):** the CLI already lacks TLS; add client-side TLS so `--user`/password never transit cleartext (server TLS env already exists). Pairs naturally with enforcing auth.
- **Audit log (S3):** append-only, hash-chained sink (separate from WAL) recording `(principal, statement, timestamp, outcome)`. Keyed off the Slice-2 principal.

---

## Self-review
- Slice 1 is fully additive and independently shippable (no server/cli changes) — safe to land now. ✓
- The breaking change (wire protocol / auth model) is explicitly gated on human sign-off. ✓
- Backward compatibility (no-users → shared-password fallback) preserves existing deployments. ✓
- Builds on verified current auth (`handler.rs` constant_time_eq, `protocol.rs` Connect). ✓
