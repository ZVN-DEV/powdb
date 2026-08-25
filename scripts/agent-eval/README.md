# agent-eval — PowDB agent-DX falsification harness

A model-agnostic, **offline** harness that measures how well an LLM can write
correct PowQL given only PowDB's own docs. It exists to falsify the claim
"an agent can pick up PowQL from `AGENTS.md` and get common queries right on
the first try" — and to compare that hit rate against the same model writing
SQL for SQLite over identical data.

Nothing here calls a model. Nothing here runs in CI. It is scaffolding: you
supply the model's answers as a JSONL file; `run.py` scores them against a
freshly seeded database, entirely locally.

## What's in here

| File | Purpose |
|---|---|
| `schema.powql` | 10 related tables (`type` DDL, one per line) |
| `seed.powql` | deterministic seed rows (one `insert` per line) |
| `sqlite-baseline/schema.sql` + `seed.sql` | the same data in SQLite, for the baseline pass |
| `tasks.json` | 26 natural-language tasks, each with a deterministic `check` |
| `setup.sh` | builds `powdb-cli`, seeds the pristine golden data dir |
| `run.py` | offline scorer (Python 3 stdlib only) |
| `examples/golden-candidates.jsonl` | hand-written known-good answers, for a self-smoke |
| `.golden-data/` | the seeded source-of-truth DB (gitignored; recreated by `setup.sh`) |

## The harness contract

The unit of evaluation is one task → one PowQL statement.

1. Give the model **only**: `AGENTS.md` (the 5-minute PowQL guide),
   `schema.powql` (the table definitions it may reference), and **one** task
   `prompt` from `tasks.json`. Do **not** give it the seed data, the
   expected answer, or other tasks.
2. The model returns **exactly one** PowQL statement.
3. Append a line to your candidates file:
   ```json
   {"task_id": "agg-02", "statement": "sum(orders filter .status = \"paid\" { .total })"}
   ```
4. Score the whole file offline:
   ```bash
   bash scripts/agent-eval/setup.sh                 # once: build CLI + seed golden data
   python3 scripts/agent-eval/run.py candidates.jsonl
   ```

`run.py` copies the golden data dir for **each** candidate (so a mutating
statement can never pollute the next one), runs the statement through
`powdb-cli --exec`, and scores stdout/exit-code against the task's `check`.
It prints a per-category pass rate and writes `results.json`. It always
exits 0 — it is a measurement tool, not a gate.

## Check types

Each task in `tasks.json` carries one `check`:

| `type` | passes when |
|---|---|
| `scalar` | the single output value equals `expected` (exact string compare; numbers compared as printed, e.g. `"4.25"`, `"3036"`) |
| `rowcount` | the result has exactly `expected` data rows |
| `rows` | the result rows, sorted, equal `expected` (sorted); use only for small results |
| `error` | the statement is **rejected** (non-zero exit) — used for the gotcha tasks (e.g. `create table`, `count:`-as-alias) |
| `ok` | the statement runs successfully (DDL / upsert that has no row output to assert) |

The output extractor in `run.py` mirrors the CLI's print format
(`crates/cli/src/output.rs` → `print_local_result` / `print_table`): a scalar
is a lone line; a table is `header` + `---+---` + data rows + `(N rows)`;
empty results print `(empty set)`; mutations print `N row(s) affected`.

## Tasks cover the AGENTS.md footgun list

The 26 tasks deliberately probe the documented gotchas: `:=` vs `=`,
`==` vs `=`, `type` (not `create table`), leading-dot field refs,
trailing-brace projection, `n:`-style aliases (plus an `error` task that
asserts `count:` as an alias is rejected), `group`/`having`, inner/left
join (with the "smaller table on the right" note), IN-subquery, null checks
(`= null`), `between`, `distinct` + `count(distinct …)`, `case`,
`order`/`limit`/`offset`, transactions (`begin`/`insert`/`rollback`/count),
`alter add column` / `add index`, upsert, and count-all (`count(Table)`,
since there is no bare `count(*)`).

## SQLite baseline (side-by-side number)

To get the comparison figure, run the **same prompts** with the **same
model** against the SQLite mirror, and score with the same check semantics:

1. Build the baseline DB:
   ```bash
   sqlite3 /tmp/agent-eval-baseline.db < scripts/agent-eval/sqlite-baseline/schema.sql
   sqlite3 /tmp/agent-eval-baseline.db < scripts/agent-eval/sqlite-baseline/seed.sql
   ```
2. For each task, give the model SQLite's docs + the same `tables_hint`
   schema + the task `prompt`; collect one SQL statement per task.
3. Score each with `sqlite3 /tmp/agent-eval-baseline.db "<sql>"` and the same
   `check` (scalar = the single cell; rowcount = number of result rows;
   `error` = non-zero `sqlite3` exit). A tiny SQL-side scorer is left as an
   exercise — the check semantics are identical; only the runner changes.
4. Report the two pass rates side by side, e.g.
   `PowQL 24/26 (92%) vs SQLite 25/26 (96%)`.

The interesting outcome is not "PowDB wins"; it's whether a model that has
never seen PowQL lands within a few points of its SQL baseline given only
`AGENTS.md`. A large gap is a docs bug, not a model bug — fix `AGENTS.md`.

## Follow-ups

- **`unique` constraints.** `schema.powql` does **not** use the `unique`
  field modifier because it is not merged on this branch. Several columns
  are naturally unique (`users.email`, `products.sku`, `orders.id`, …); once
  the UNIQUE-constraints work lands, declare them `unique`, add a
  `unique`-violation `error` task, and change `upsert-01` to key on a
  genuinely-unique column. Until then `upsert` keys on a plain column
  (which currently works on any column on this branch).
- **Batch seeding.** `setup.sh` runs one CLI process per statement (~60
  total). Fine at this scale; if it ever bites, feed all statements through
  one REPL stdin once multi-line REPL input lands.

## Not wired into CI

By design. No model calls happen anywhere in CI. This harness is run on
demand by a human (or an agent) to measure docs/DX quality.
