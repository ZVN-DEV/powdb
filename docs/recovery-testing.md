# Replaying mutation and recovery tests

`cargo test -p powdb-query --test recovery_model --locked` runs a bounded set of
mixed-write traces against an independent in-memory model. It is included in the
normal workspace test suite, including PRs stacked on `codex/*` branches.

Every trace runs in Full, Normal, and Off WAL modes, with both optimized and
forced-generic execution. The default corpus is four seeds, 32 operations each:
768 mutation steps in total. Each trace includes successful inserts, upserts, updates,
deletes, late insert/update uniqueness failures, committed transactions, explicit
rollbacks, and aborted transactions. Payloads vary between inline strings and
multi-page overflow values, including quotes, backslashes, and Unicode.

After each step the test compares all row values and the live row count with the
model, checks real unique/nonunique B-tree entries (including missing keys), and
checks a dependent materialized view. It closes and reopens the engine every
eight steps and at the end, reapplying the selected mode and execution path.

## Replay a failure

The test prints `seed`, WAL mode, generic-path flag, operation, and zero-based
step before execution. Replay one seed using the same step count:

```bash
POWDB_RECOVERY_SEED=24301 POWDB_RECOVERY_SEEDS=1 POWDB_RECOVERY_STEPS=32 \
  cargo test -p powdb-query --test recovery_model --locked -- --nocapture
```

This deliberately replays all six mode/path combinations for that seed. Keep the
reported seed and the failing operation prefix when filing a regression.

## Extended corpus

The existing nightly/manual `fuzz` workflow runs 32 consecutive seeds and 96 steps
(18,432 mutation steps), retaining operation logs for 14 days. Run the same corpus locally with:

```bash
POWDB_RECOVERY_SEED=24301 POWDB_RECOVERY_SEEDS=32 POWDB_RECOVERY_STEPS=96 \
  cargo test -p powdb-query --test recovery_model --locked -- --nocapture
```

The extended Linux job uses `/dev/shm` for disposable databases to bound runtime.
The ordinary PR test uses the platform's default temporary directory. RAM-backed
corpus results are logical-state/reopen evidence, not physical-disk durability evidence.

Inputs are bounded: 1–256 seeds, 8–512 steps per trace, and a starting seed in the
unsigned 32-bit range. Invalid settings fail instead of producing an empty pass.
The generator uses explicit wrapping integer arithmetic, so dependency upgrades
do not silently change an existing trace.

## What this proves—and does not

These traces test live-process rollback and **graceful** reopen. Off mode is not
crash-durable. Independent tests in `statement_crash_recovery.rs` kill processes
around commit; `statement_commit_failures.rs` injects fsync failures. Neither
process death nor injected I/O failure is a physical power-loss test.

The model corpus is not a complete fault matrix for every storage file or DDL
publication window, and passing it is not proof that the engine has no bugs.
