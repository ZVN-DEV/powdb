# PowQL differentiation experiment decision

_Date: 2026-06-16 · Follows the v0.5.0 release-gate commit `7f995ef`._

## Decision

Lead the next production lane with the **FTS-first consolidation-engine path**, not a full M.* PowQL
language buildout.

PowQL stays strategically valuable as an optional DX surface, but the experiment evidence says the
highest-confidence production differentiation is engine-owned consolidation: native full-text search
maintained transactionally with base rows, then vectors/hybrid search later. The M.* track should be
kept as a focused enabling substrate only where it is needed by engine features or a later proven demo
(`Value::List`/`Value::Struct`, structured protocol, relationship metadata), not as the headline bet.

## Inputs

- `docs/strategy/2026-06-14-direction-and-hardening-roadmap.md`
- `docs/strategy/2026-06-14-worklist-and-build-map.md`
- Current v0.5.0 release-gate state at commit `7f995ef`
- Executable probe log: `.omx/reports/g008-parser-probe-20260616.log`

The PR #96 master doc named in the original brief (`docs/strategy/2026-06-15-remaining-work-master.md`)
was not present in the local checkout during this run, so this decision is grounded in the two strategy
docs above plus the current code.

## Experiment run

### Probe setup

A local embedded PowDB database was populated with a one-to-many shape:

- `User(id, name, visits)`
- `Post(id, user_id, title)`
- Alice has two posts; Bob has one post.

The flat relational surface works:

```powql
User as u inner join Post as p on u.id = p.user_id { u.name, u.visits, p.title }
```

returns the expected fan-out rows.

### Finding 1 — fan-out aggregate wedge is real, but not buildable as syntax-only

The desired killer demo is still valid: one-to-many fan-out makes naive aggregation over a joined rowset
wrong by default, and a graph-aware aggregate can be right by default. But the current shipped engine has
no relationship graph for the aggregate to inspect:

- no link schema type;
- no link traversal grammar;
- no cardinality metadata in the plan;
- no graph-aware aggregate operator.

The probe confirms the current surface remains flat-row relational. It can join the tables, but the
right-by-default graph aggregate requires M.4–M.6 plus M.10, not just a small parser tweak.

### Finding 2 — nested fetch is currently unrepresentable

The proposed nested fetch:

```powql
User { .name, posts: .posts { .title } }
```

fails today with:

```text
Error: expected field, got '{'
```

The code also confirms nested results are not representable in storage/runtime values today:

```rust
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    DateTime(i64),
    Uuid([u8; 16]),
    Bytes(Vec<u8>),
    Empty,
}
```

There is no `Value::List` or `Value::Struct`. Therefore the nested-fetch experiment is not a quick
post-v0.5 validation; it starts with the permanent nested-value and wire-protocol decisions called out
as M.1/M.2.

### Finding 3 — the strongest wedge is engine-owned, not language-owned

Both PowQL wedges remain DX/correctness improvements, but their implementation value comes from engine
state PowDB owns: relationship metadata, result shaping, and aggregate semantics. The same precedent
analysis in the strategy docs points to the stronger near-term moat: remove the second-system sync tax.
Native FTS is the cleanest first instance because it is:

- directly useful to application developers;
- difficult for an external search system to make transactionally consistent with base data;
- unlocked by v0.5.0 transaction and format-versioning work;
- valuable through both SQL and PowQL, avoiding a language-adoption tax.

## Production path selected

### P0 — do not publish M.* as the next headline

Do not make `link`, nested results, or graph-aware aggregation the next production promise. They remain
valid R&D, but the proof-of-value slice is not cheap enough to justify leading with it before a stronger
engine-owned feature.

### P1 — FTS-first consolidation-engine MVP

Track these tasks next:

1. **FTS decision lock** — record an ADR for `match` keyword ownership and SQL/PowQL query surfaces.
   Decide whether PowQL `match` is search-match, relationship pattern-match, or explicitly split.
2. **Versioned FTS index format** — design an inverted-index file with magic/version/reject-unknown
   behavior following `docs/FORMAT.md` discipline.
3. **Analyzer MVP** — tokenizer, lowercase normalization, stopword policy, and deterministic tests.
4. **Transactional maintenance** — update posting lists in the same transaction as base row writes;
   prove commit makes terms visible and rollback removes them.
5. **Query MVP** — return matching rows ranked by a simple deterministic score first; leave BM25 tuning
   behind a correctness gate.
6. **Killer demo** — in one transaction, insert a row and search it; rollback and prove it disappears;
   commit and prove it persists across restart.
7. **Docs/GTM update** — position PowDB as a compiled pure-Rust engine with SQL on-ramp, optional PowQL
   DX, and native transactionally consistent search.

### P2 — keep a narrow M.* substrate track

Only build M.* pieces when they directly support the selected engine path or a separately approved demo:

1. `Value::List` / `Value::Struct` design spike, with wire-format compatibility constraints.
2. Relationship metadata spike for graph-aware aggregates, after FTS MVP is demonstrably useful.
3. Nested fetch revisit only after structured protocol decisions are stable.

## Revisit trigger

Reconsider full M.* priority only if one of these becomes true:

- a one-week vertical slice can prove fan-out aggregate correctness without committing to the full nested
  value/protocol/link stack;
- early users explicitly ask for nested object results more than native search;
- FTS MVP fails to produce a compelling transactionally-consistent demo.

## Verification

- `cargo run -p powdb-cli -- --help` — confirms v0.5.0 CLI build.
- `.omx/reports/g008-parser-probe-20260616.log` — captures the User/Post probe, flat join success,
  nested link syntax rejection, and aggregate-surface limitations.
- Code inspection confirms current `Value` is flat and link/let/match are not implemented as the M.*
  relationship/nested-result system.
