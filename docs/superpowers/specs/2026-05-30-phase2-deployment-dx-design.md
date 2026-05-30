# Phase 2 — Deployment + DX

**Status:** design draft (2026-05-30)
**Driver:** Kirby wants PowDB ready to deploy to AWS / Cloudflare / Railway + a great one-command local boot. README + competitive positioning need to lead with the real differentiator (compiled-predicate engine), not the negative framing.
**Blocks:** Phase 3 (Risky engine upgrades — separate research-first doc)

## Review rigor (every workstream)

Every PR in Phase 2 must demonstrate, with captured evidence:
1. **No regression** — full `cargo test --workspace` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo fmt --all -- --check` green; bench regression gate green on CI.
2. **Coverage gain** — new tests for new code (smoke tests for deploy artifacts, doc tests for docs that include code, a local-boot test). Net test count goes UP.
3. **Feature gain** — every PR delivers concrete new capability (new deploy target, new docs page, local-boot script, etc.). No refactor-only commits.
4. **Support gain** — more platforms / more deploy targets / more clients / clearer docs. Phase 2 is mostly support gain by definition.

No claim of "works" without a captured run command/output.

## Scope (4 workstreams)

### WS-D1: Cloud deploy examples (AWS ECS Fargate + EFS, Cloudflare Tunnel, Railway)

**Problem:** `examples/deploy/` currently has only Fly.io (`fly.toml`). Kirby explicitly named AWS, Cloudflare, Railway as deployment targets.

**Design:**
- **AWS ECS Fargate + EFS** under `examples/deploy/aws-ecs/`: Terraform module (single `main.tf` + `variables.tf` + `README.md`) that provisions: VPC fragment, ECS cluster, Fargate service running `ghcr.io/zvndev/powdb:latest`, EFS for `POWDB_DATA`, security group for the TCP port, Secrets Manager binding for `POWDB_PASSWORD`. Defaults wired for `POWDB_REQUIRE_TLS` + TLS termination via ACM (or behind ALB if the user prefers). Documented as a starting point, not a turnkey production deploy.
- **Cloudflare Tunnel** under `examples/deploy/cloudflare-tunnel/`: a `cloudflared` config (`config.yml`) + a docker-compose snippet that runs `powdb-server` and `cloudflared` side-by-side. Docs explain the "self-host anywhere, expose via Tunnel" pattern with zero ingress port and access policy via Cloudflare Access (optional).
- **Railway** under `examples/deploy/railway/`: a `railway.toml` (or just docs pointing at the Dockerfile) + a one-screen README covering volume mounting for `POWDB_DATA`, env-var setup including `POWDB_PASSWORD` + `POWDB_REQUIRE_TLS`, and the trade-offs of Railway's volume-backed storage at scale.

**Success criteria:**
- All three examples exist, each with its own README that a new user can follow.
- A smoke check in CI for at least the AWS Terraform (`terraform validate` + `terraform plan` on a stubbed backend) and the Cloudflare compose (`docker compose config` parse check). Catches breakage when someone edits an example.
- The existing Fly.io example is updated to reference Phase 1's new env vars (`POWDB_REQUIRE_TLS`, `POWDB_QUERY_MEMORY_LIMIT`).
- No engine code changes. Pure additions.

**Verification:**
```bash
# AWS example builds: terraform validate + plan (with backend=local stub)
cd examples/deploy/aws-ecs && terraform init -backend=false && terraform validate
# Cloudflare compose parses:
cd examples/deploy/cloudflare-tunnel && docker compose -f docker-compose.yml config -q
# Workspace still green:
cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings
```
Goal reached when all three examples exist, the new smoke checks pass, and the README of each tells a deployer exactly what to do.

---

### WS-D2: Local-dev one-command boot

**Problem:** Today, "boot it locally" means `cargo run --release -p powdb-server -- --port 5433 --data-dir ./powdb_data`, separately running the CLI, separately spinning up Postgres for comparison benches. Kirby wants this to be one command.

**Design:**
- Add `scripts/dev.sh` (or a `justfile` if the project already has one — check first; if not, a plain script is fine to avoid a new tool dep):
  - `scripts/dev.sh up` — boots `powdb-server` in a tmp data dir on a free port with sensible dev defaults (no password, no TLS, verbose logging), prints the connection string + a one-liner CLI invocation to connect.
  - `scripts/dev.sh repl` — launches `powdb-cli` against the running dev server, OR embedded mode if no server is up.
  - `scripts/dev.sh bench` — runs `cargo run --release -p powdb-compare` against a freshly-built server (PG optional via the new docker-compose from Phase 1 WS6).
  - `scripts/dev.sh down` — kills the dev server, cleans the tmp data dir.
- A `scripts/README.md` explaining each command.
- A CI smoke test that runs `dev.sh up && dev.sh down` on Linux to make sure the bootstrap script doesn't bit-rot.

**Success criteria:**
- A user with a fresh clone can `git clone … && cd powdb && ./scripts/dev.sh up` and have a working server + a printed copy-pasteable CLI command in under 30 seconds (after the first cargo build).
- The smoke test catches breakage in CI.
- No new tool dependency beyond what's already in CI (bash, docker compose).

**Verification:**
```bash
./scripts/dev.sh up           # boots, prints connection info
./scripts/dev.sh repl --help  # cli reachable (or non-interactive smoke)
./scripts/dev.sh down         # clean shutdown, no orphans, tmp dir cleaned
# CI smoke (Linux only):
bash scripts/dev.sh up && bash scripts/dev.sh down
```
Goal reached when the up/down cycle works locally on macOS + Linux and the CI smoke passes.

---

### WS-D3: README reframe — lead with the compiled-predicate engine

**Problem:** Today's README headline framing is *"PowQL — a query language designed for how developers actually think"* and the negative *"no SQL parsing layer."* The competitive research (2026-05-27) showed every database that led with "we replaced SQL" has hit adoption friction (Gel/EdgeDB, SurrealDB); and the real technical differentiator is the **compiled predicate engine + zero-copy mmap + tight planner-executor contract**, not the absence of SQL. Kirby chose: lead with the compiled-predicate engine, keep PowQL as an ergonomic feature.

**Design:**
- Rewrite the top of `README.md`:
  - **Headline**: PowDB is a pure-Rust embedded database with a compiled query execution engine that delivers 3–10× SQLite on aggregate/scan workloads.
  - **Sub-head**: One performance bullet (compiled byte-level predicates + zero-copy mmap scans), one platform bullet (pure-Rust, no C deps, embeddable + server modes), one DX bullet (PowQL — left-to-right pipeline syntax that reads like an iterator chain).
  - Keep the benchmark table where it is, but lead with the workloads that show the architecture (aggregates, scans), not with the noisy ones.
  - Replace the "Why PowQL?" section with a "How it works" section that explains the compiled predicate engine + plan cache + mmap, then introduces PowQL as the front door.
- Update the production checklist to include `POWDB_REQUIRE_TLS` and `POWDB_QUERY_MEMORY_LIMIT` (Phase 1 additions).
- Keep all current factual claims (benchmark numbers, install instructions, language reference link). Don't break any existing links.

**Success criteria:**
- The top fold of the README no longer leads with "no SQL." Compiled-engine framing is the headline.
- No factual regressions (benchmark numbers, install commands, links all still accurate — verify the install commands actually work).
- All existing tests + markdown link checks pass.

**Verification:**
```bash
# Verify the install commands in the new README still work:
cargo install --path crates/cli --locked --root /tmp/powdb-install-smoke
ls /tmp/powdb-install-smoke/bin/powdb-cli
# Verify markdown links (use a checker — markdown-link-check or similar; if no CI step, add one)
# Workspace green:
cargo test --workspace
```
Goal reached when the README leads with the engine architecture, all install/link checks pass, and the production checklist mentions the Phase 1 env vars.

---

### WS-D4: "PowDB vs SQLite — when to use which" guide

**Problem:** Users evaluating PowDB need an honest comparison. The product review explicitly called for this: *"Users need to understand that PowDB is faster on scan-heavy reads and competitive on writes, but SQLite has 25 years of battle-testing."*

**Design:**
- New `docs/powdb-vs-sqlite.md`, ~600-1000 words, structured as:
  - **When to choose PowDB:** pure-Rust stack, no FFI, scan/agg-heavy workloads where the 3-10× wins fire, projects already on tokio/Rust async, anywhere you want to embed without a C toolchain.
  - **When to choose SQLite:** anywhere SQL compatibility matters, anywhere battle-testing matters more than peak perf, broad tool ecosystem (analytics, DB browsers, ORMs), platforms where you already have the C toolchain.
  - **Side-by-side feature table** sourced from Phase 1's competitive research (storage model, query language, MVCC, fuzz testing, mmap, etc.). Honest about both columns.
  - **Honest benchmark table** with the stable Phase 1 numbers (insert numbers now reproducible, agg/scan numbers calibrated on CI). Disclose methodology + WalSyncMode setting.
- Link from README's headline section so a curious evaluator can find it in one click.

**Success criteria:**
- The guide exists, links work, benchmark numbers cite the run that produced them.
- Both columns are written with equal care — no straw-manning SQLite.

**Verification:**
```bash
# Link sanity (manual or markdown-link-check):
grep -c '\[.*\](' docs/powdb-vs-sqlite.md
# README links to the new guide:
grep -q 'powdb-vs-sqlite' README.md
```
Goal reached when the doc exists, the README links to it, and the benchmark numbers cited are from a run anchored in the PR's CI artifacts (or `/tmp` evidence captured by the dev).

---

## Workstream coordination (no overlap)

| Lane | Workstreams | Files |
|---|---|---|
| **Dev 1 (Ops)** | WS-D1 + WS-D2 | `examples/deploy/`, `scripts/`, CI workflow additions |
| **Dev 2 (Docs)** | WS-D3 + WS-D4 | `README.md`, `docs/powdb-vs-sqlite.md`, header refresh |

Both lanes are non-Rust (mostly): deploy/script/doc edits. Zero Rust code changes expected; if either lane finds it needs to touch a crate, STOP and report.

Dev 1 and Dev 2 run sequentially on the shared checkout (no concurrent edits, no worktrees, per [[multiagent-orchestration]]). One integration branch `phase2`, one stacked PR for Kirby's single approval at the end. Reviewer + bug-hunter capstone as in Phase 1.

## Out of scope for Phase 2

- **Phase 3 (risky engine upgrades)** is research-first in `docs/superpowers/specs/2026-05-30-phase3-risky-research-plan.md` — Windows pread/pwrite, disk-spill external sort, cost-based optimizer with catalog access, true multi-writer MVCC. Do not touch in Phase 2.
- New language features, new query operations, new index types. Phase 2 ships zero engine changes.
