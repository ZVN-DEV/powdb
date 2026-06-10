# scripts/

Helper scripts that aren't part of the production build but make the local
dev loop nicer.

## `dev.sh` — one-command local boot

```bash
scripts/dev.sh up      # build + start powdb-server in a tmp dir, free port, dev defaults
scripts/dev.sh repl    # powdb-cli --remote against the running dev server
scripts/dev.sh bench   # cargo run --release -p powdb-compare (uses docker pg if available)
scripts/dev.sh down    # stop the server, clean the tmp data dir
scripts/dev.sh --help  # all of the above
```

### What `up` promises (and doesn't)

- **Promises:** picks a free TCP port, writes data to a fresh
  `$TMPDIR/powdb-dev-XXXX`, starts `target/release/powdb-server` with
  `RUST_LOG=info`, writes a pidfile + portfile under `target/`, prints a
  copy-pasteable `powdb-cli --remote` command, exits non-zero if the
  server died during startup (and surfaces the log).
- **Does not promise:** no password, no TLS, no auth at all — this is a
  developer convenience, not a production launcher. Do not point it at
  data you care about; the data dir is removed by `down`.

### What `bench` does

Runs `cargo run --release -p powdb-compare`. If `POWDB_BENCH_PG_URL` is
unset and `crates/compare/docker-compose.yml` exists, it brings up the
WS6 Postgres compose first and exports the matching URL. Honest about
which engines participated:

- `POWDB_BENCH_PG_URL` set → uses that.
- compose file + docker available → starts pg + uses it.
- no docker / no compose → SQLite-only run.

### What `down` does

Stops the pid, removes the pidfile/portfile, and `rm -rf`s the tmp data
dir — but only if its path matches a recognized tmp pattern
(`$TMPDIR/powdb-dev-*`, `/tmp/powdb-dev-*`, `/var/folders/.../powdb-dev-*`).
If the path looks wrong, it refuses and prints the path; you remove it
yourself. This is a guardrail against pidfile corruption.

### CI smoke

`.github/workflows/ci.yml` runs `bash scripts/dev.sh up && bash
scripts/dev.sh down` on ubuntu so the script doesn't bit-rot. Any error
in the cycle fails the CI job (`set -euo pipefail` inside the script).

## `update-bench-baseline.sh`

Resets the criterion benchmark baselines after intentional perf changes.
Documented inside the script itself.

## `agent-eval/` — agent-DX falsification harness

A model-agnostic, **offline** harness that scores how well an LLM writes
correct PowQL given only `AGENTS.md` and a schema — and lets you compare that
hit rate against the same model writing SQL for SQLite over identical data.

```bash
bash scripts/agent-eval/setup.sh                       # build CLI + seed .golden-data/
python3 scripts/agent-eval/run.py \
  scripts/agent-eval/examples/golden-candidates.jsonl  # smoke: 6/7 (one intentional fail)
```

- `setup.sh` builds `powdb-cli` and seeds a pristine `.golden-data/` dir
  (gitignored) from `schema.powql` + `seed.powql` — 10 related tables.
- `tasks.json` holds 26 natural-language tasks, each with a deterministic
  `check` (`scalar` / `rowcount` / `rows` / `error` / `ok`), covering the
  AGENTS.md footgun list.
- `run.py` (Python 3 stdlib only) copies the golden dir per candidate, runs
  each candidate statement through `powdb-cli --exec`, scores the output, and
  prints a per-category pass rate. Always exits 0 — it's a measurement tool.
- No model calls anywhere, and **not wired into CI**. See
  `scripts/agent-eval/README.md` for the full contract and the SQLite
  baseline procedure.
