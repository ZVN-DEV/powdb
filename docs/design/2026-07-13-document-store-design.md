# Document-Store Design: Overflow Pages, JSON Type, Grouped Aggregates

Date: 2026-07-13. Status: PROPOSED, awaiting sign-off on two flagged forks (F1, F2).
Scope: Capa dogfood findings P-2, P-3, P-5 (the "primary store" capability gaps).

Provenance: synthesized from two independent design lanes (pragmatic vs destination-first)
over a code-verified recon of the storage and query engines at v0.10.0. Every claim about
current code was verified against the repo; file:line references are to v0.10.0.

## 1. Context and goals

The Capa CMS dogfood run (July 2026, powdb 0.9.0) proved correctness parity with
Prisma/Postgres (21/21) but found PowDB unviable as a primary document-shaped store:

- P-2: the ~4070 byte row cap forced app-side chunking (14,228 chunk rows, 8-chunk max,
  reassembly on every read).
- P-3: no JSON type or path querying forced a 77,519-row EAV hot-column table; an
  unindexed JSON-field filter ran 1.7s vs 45ms on Postgres.
- P-5: no grouped aggregates over their join/EAV shape; app-side reduce ran 518ms vs 37ms.

Goal: make PowDB a viable primary store for CMS/document workloads while preserving its
identity: compiled predicates over raw row bytes, mmap zero-copy scans, engine-owned
everything (no serde_json in the runtime), pure planner with runtime catalog-aware
lowering, PowQL AST = plan tree, SQL as a frontend lowering to the same AST.

The three features are designed together because they interact: JSON documents are the
common large value (P-3 rides on P-2), compiled JSON-path predicates must understand
out-of-line values, and grouped aggregates are what make JSON-shaped relational data
queryable in-engine.

## 2. The structural finding that frames P-2

The 64KB u16 row-format cap and the 4070B page cap are different limits, and only one
matters. An inline row can never exceed MAX_ROW_DATA_SIZE = 4070 bytes because it must
fit in one 4KB page slot (page.rs:34). The u16 length prefix and u16 var-offset table in
row format v1 therefore have headroom forever for the inline representation.

Decision (both lanes converged after analysis): there will never be a u32 row format.
The on-page row stays small and u16-addressed permanently; large values live out of line
behind a fixed-size stub whose logical length field is u64. Bigness is an overflow-chain
property, not a row-format property. Exactly one row-format migration ships (v1 to v2,
adding an overflow bitmap), and the "row format v3" question is closed permanently.

Consequence: there is no 64KB value cap in the design. v0.11 enforces a documented
engine limit MAX_VALUE_SIZE = 64MB as a config-raisable constant, not a format constant.

## 3. P-2: Overflow pages

### 3.1 Row format v2

ROW_FORMAT_VERSION 2. Body layout, delta from v1 in brackets:

```
"PROW" magic (4B) | version u16 = 2
[len u16]
[null bitmap        ceil(n_cols/8) bytes]
[overflow bitmap    ceil(n_var/8)  bytes]   [NEW: 1 bit per var column, declaration order]
[fixed cols packed at precomputed offsets]
[var offset table   (n_var+1) x u16]
[var data]                                  [flagged columns hold a 24B stub, not the value]
```

Write policy: v2 is written only for rows containing at least one spilled value; rows
that fit inline stay v1, byte-identical to today. A database that never spills never
produces a v2 row and remains fully readable by old binaries. (The alternative, writing
v2 for all new rows to get uniform compiled-predicate layout, is deferred to the v0.12
RowCtx migration; see 3.7 and door D10.)

### 3.2 Overflow stub (24 bytes, permanent on-disk pointer)

```
offset  size  field
0       8     total_len      u64   logical byte length of the full value
8       4     first_page     u32   head of overflow chain
12      4     value_crc32    u32   CRC32 of the fully assembled value
16      1     flags          u8    bit0 compressed (RESERVED), bit1 chunk-aligned (RESERVED)
17      1     stub_version   u8    = 1
18      6     reserved             zeroed
```

u64 length is the point of the design; the whole-value CRC detects torn or cross-linked
chains at read time; flags/version/reserved are the escape hatch for compression and
layout evolution without another row-format bump.

### 3.3 Overflow page format

PageType::Overflow = 3 exists reserved and unused since the page-type enum was born
(page.rs:67-88); it is finally used. Standard 20-byte page header (type=3, checksum flag
set), then a chunk header instead of a slot directory:

```
offset  size  field
20      4     next_page   u32   next chunk, u32::MAX = end of chain
24      2     chunk_len   u16
26      2     reserved
28      4068  payload
```

Chains are singly linked, written head-first. No per-chunk owner back-pointer: RIDs can
change on update (delete+insert path, heap.rs:998-999), so back-pointers would go stale;
orphan detection is sweep's job (3.6). Overflow pages are excluded from pages_with_space.

### 3.4 Spill policy

Deterministic, at encode time, no user knobs:

1. Fixed-size types never spill.
2. If the encoded row fits 4070B, store fully inline (v1). Most rows never touch overflow.
3. Otherwise evict var values (Str, Bytes, Json) largest-first until the stub row fits.
4. Never evict a value under OVERFLOW_MIN = 256B unless full eviction is the only way to
   fit; error only if even that fails (unreachable with sane schemas).

### 3.5 WAL strategy: physical logging

Insert/Update currently log the entire encoded row inline (catalog.rs:1220-1237,
u32-framed, 256MB read cap). Logical logging of assembled big rows was rejected by both
lanes: replay-time chain allocation cannot reproduce page ids deterministically, which
breaks the insert_at(rid) + per-page LSN idempotent replay contract.

Two new record types (additive; current range Insert=1..Begin=10):

```
OverflowWrite = 11   page_id u32 | next_page u32 | chunk_len u16 | chunk bytes
OverflowFree  = 12   count u32 | page_id u32 x count
```

Write ordering inside the transaction: allocate chain pages, emit one OverflowWrite per
chunk, then emit Insert/Update with the inline stub row exactly as today (row records
shrink). Redo applies OverflowWrite by page id under the existing per-page LSN skip;
fully idempotent. Deletes and chain-replacing updates emit OverflowFree; frees are
applied at commit (per-tx pending-free list) and discarded at rollback, because freeing
before commit corrupts redo-only recovery.

### 3.6 Crash safety, rollback, and the sweep primitive

- Explicit rollback: the tx tracks its overflow allocations; rollback returns them to
  the in-memory free list and must not leave them in any structure a Drop-time
  checkpoint could flush. This mirrors the v0.10.0 rollback bug class (the dropped
  catalog's checkpoint flushed rolled-back index state); a regression test for the
  overflow analog is written before the feature (TDD).
- Crash orphans: a crashed tx may have flushed chain pages that no committed row
  references. They are leaked, not corrupt. Reclamation is sweep:

sweep (mark-and-sweep overflow reclaimer, the minimal primitive PowDB will inevitably
need and the seed of future compaction):

1. Record an allocation watermark (younger pages exempt).
2. Mark: scan data pages via iter_page_slots reading only version word, bitmaps, and
   stubs (no full decode); walk each referenced chain into a referenced-set bitmap.
3. Sweep: any Overflow-typed page below the watermark not in the set goes to the free
   list; the batch is logged as one OverflowFree record so reclamation is crash-safe.
4. v0.11: manual (powdb-cli sweep) plus automatic after recovery completes (crash is
   exactly when orphans appear). Runs under the table write lock; sequential I/O.
   v0.12: free-ratio auto-trigger.

Sweep deliberately does not compact tombstoned row space; it reclaims whole overflow
pages only, which is safe because nothing addresses into a freed overflow page.

### 3.7 Compiled-predicate safety

v0.11: per-row version routing. The fused scan fast paths check the row version word
(bytes 4..6, adjacent to data already read): v1 and legacy v0 rows take the compiled
path unchanged; v2 rows route to decode_selective for that row. compiled.rs internals
stay untouched; one u16 compare per row (bench-gated, expected noise; escape hatch is a
per-page "has v2 rows" flag bit in the page flags nibble).

v0.12 (with the JSON leaf, door D10): the CompiledPredicate signature migrates once to
its destination shape:

```rust
struct RowCtx<'a> { row: &'a [u8], ovf: &'a dyn OverflowFetch }
enum Tri { True, False, Fallback }
type CompiledPredicate = Box<dyn Fn(&RowCtx) -> Tri + Send + Sync>;
```

Existing leaves wrap mechanically (True/False). Fallback is per-row, so one huge row in
a million does not disable a compiled scan. Fixed columns never spill, so Int/Float
leaves change only by layout shift when the bitmap is present.

### 3.8 Format and back-compat

- Row: gate accepts versions 1 and 2 (row.rs:12 currently rejects >1).
- Heap: HEAP_FORMAT_VERSION 2 -> 3, lazily bumped on the first chain write; never-spilling
  databases stay v2 and old-binary-readable. Old binaries refuse v3 outright (correct:
  they would misread Overflow pages mid-scan).
- WAL: stays v1; record types 11/12 are additive and only appear alongside heap v3.
- Policy line for docs: files are forward-openable always; backward-openable only if
  they never used features newer than that binary.

## 4. P-3: JSON type

### 4.1 FORK F1 (needs sign-off): storage encoding, text vs binary

This is the largest one-way door in the design: encoded values persist in heaps and
index keys forever, and the semantics (key-order preservation or not) are user-visible
from the first release.

- Lane A argued validated minified UTF-8 text: reversible later behind a tag byte,
  ~300 LoC validator, zero wire cost, and path indexes (not per-value structure) as the
  perf answer. Fallback scans land ~300ms on the Capa shape: 5x better than their EAV,
  but 7x behind PG's 45ms.
- Lane B argued a binary order-defined encoding (PJ1): sorted-key directories make a
  path probe an O(depth x log fanout) pointer walk over raw mmap bytes with zero parse
  and zero allocation. It is the only choice under which a compiled JSON-path leaf can
  contest or beat PG's detoasted jsonb walk, and canonical bytes (equal documents have
  equal bytes) make equality, group keys, and index keys cheap everywhere else.

RECOMMENDATION: PJ1 binary. Compiled predicates over raw bytes are PowDB's entire
identity and headline benchmark story; a text encoding concedes the unindexed case
permanently and makes canonical equality impossible. The costs are real and accepted:
key insertion order is not preserved (JSONB precedent), and the encoding spec is
forever, so it ships with a fuzz target, canonicalization property tests, and a model-
based total-order test before the first persisted byte.

### 4.2 PJ1 encoding (under F1 = binary)

TypeId 8 = Json (next free slot; from_u8 returns None for >= 8 today).
Value::Json(Box<[u8]>) holds PJ1 bytes; to_wire_string renders canonical JSON text.

```
tag u8 (low 3 bits): 0 null | 1 false | 2 true | 3 int | 4 float | 5 string
                     6 array | 7 object     (tags 8..15 RESERVED: datetime, uuid, decimal, ...)
null/false/true:  [tag]
int:              [tag][i64 LE]
float:            [tag][f64 LE]
string:           [tag][len u32][UTF-8]
array:            [tag][count u32][elem_off u32 x (count+1)][elements]
object:           [tag][count u32][(key_off u32, val_off u32) x count][end u32]
                  [key data: len u32 + bytes, sorted][values]
```

Canonicalization (permanent semantics): object keys sorted bytewise (insertion order NOT
preserved); duplicate keys last-wins, deduplicated at encode; int/float distinction kept
from input text; total order null < false < true < numbers < strings < arrays < objects,
defined once and used for min/max, index keys, and group keys. Equal documents have
equal bytes. Floats are f64 (documented precision limits; reserved tag space is the
decimal escape). Insert validation rejects invalid JSON/UTF-8 with a typed error and
enforces a depth cap. Encoder/decoder are hand-rolled in the storage crate (engine-owned,
no serde_json in the runtime; differential tests against a dev-dependency parser are fine).

### 4.3 Path syntax and AST

Token::Arrow (->) is lexed and consumed nowhere in the grammar today (lexer.rs:438,
token.rs:126): it is free. New tightest-binding postfix level above primary:

```
.data->author->name       object key            .data->'weird key!'   string-form key
.data->tags->0            array index           posts.data->author    qualified base
```

AST: Expr::JsonPath { base: Box<Expr>, segments: Vec<PathSeg> }, PathSeg = Key(String) |
Index(u32). Base restricted to Field/QualifiedField/JsonPath at parse time.

Plan-cache safety (#137 rules): path segments are STRUCTURAL. They hash into the
canonical token shape and are never collected as literal slots: count_expr and
substitute_expr recurse into base only. Same path + different comparison literal shares
a plan; different path is a different plan. A property test asserts
count_literal_slots(plan) == source literal count over path-bearing queries, and the
existing insert-refusal guard remains the backstop.

### 4.4 Extraction and predicate semantics

-> extracts and scalarizes: JSON string -> Str, integral number -> Int else Float,
bool -> Bool, object/array -> Json (sub-document), JSON null and missing path both ->
Empty. No implicit cross-type coercion: .data->age > 21 compares whatever extraction
yields under existing Value rules; stringly numbers use cast. json_type(expr) (a ~40 LoC
ScalarFunc returning 'null'|'string'|'number'|'bool'|'object'|'array'|Empty-when-missing)
is the escape hatch to distinguish null from missing. No mutation operators (json_set).

Compiled leaf (v0.12): JsonPath-over-Json-column CMP literal compiles to a directory
walk over the column's PJ1 bytes. Inline documents: straight over mmap, zero parse, zero
alloc; this is where PowDB can beat PG (which detoasts and dispatches operators even for
small jsonb). Out-of-line documents: fetch-assemble via RowCtx.ovf into a reusable
per-scan buffer, then the same binary walk; parity with PG's detoast cost. Chunk-aligned
layout (stub flags bit1) is the reserved escape if profiling ever demands touching only
needed chunks; nothing ships until proven necessary. Everything else falls back to
decode_selective, safe by construction because leaves gate on type_id.

### 4.5 Path indexes: general catalog format, restricted v1 mechanism

Expression indexes are not representable today (IndexedCol is keyed by stored column
name; maintenance decodes that column). Synthesis of the two lanes:

- Catalog FORMAT is the general concept: IndexKeySource { Column(String), Expression
  { canon_text: String } }, CATALOG_VERSION 5 -> 6, additive staircase, lazily bumped on
  first use. Index identity = canonical-text equality.
- v1 MECHANISM is restricted to JSON paths, avoiding a crate-boundary inversion: the
  query crate validates and canonicalizes the expression at CREATE INDEX and hands the
  storage crate a compiled PathSeg list (column + segments). The path walker lives in
  the storage crate (it operates on bytes and segments, no Expr), so insert/update/
  delete/rebuild extract keys exactly where column extraction happens today. Extracted
  scalars key the existing btree machinery; Empty (missing/null) rows are not indexed
  (partial-index semantics; `is null` on a path never uses the index). Accepting
  arbitrary expressions later is an additive widening of what CREATE INDEX validates,
  not a format change.
- Planner stays pure: try_extract_eq_index_key and the range extractor gain a JsonPath
  arm emitting speculative IndexScan/RangeScan carrying the canonical signature; runtime
  lower_unindexed_scans matches signatures against the catalog and lowers to
  Filter(SeqScan) when absent (the existing pattern, plan_exec.rs:3813-3862).
- Syntax: create index on posts (.data->author->name); unique allowed.
- Overflow interaction: extraction happens on the logical value before spill on the
  write path; only rebuild reads back through chain reassembly.

### 4.6 Wire and clients

- v0.12: Json cells render as canonical JSON text in the existing string cells (zero
  protocol change); TS client ColumnType gains "json" (JSON.parse in queryTyped).
- v0.13: additive column-level type hints (Vec<TypeId> alongside columns in ResultRows,
  absent = untyped) which incidentally fixes Bytes' lossy "<N bytes>" rendering.
- Per-cell type tags / binary results: deferred; that door couples to any future
  nested-results decision and is explicitly not spent here.
- SQL frontend (v0.13): SQL lexer gains -> and ->> tokens; source-to-source lowering to
  the PowQL form, ->> adding a to-text cast (PG semantics divergence documented in
  docs/SQL.md).

## 5. P-5: Grouped aggregates

### 5.1 Table stakes: bug fixes (v0.11)

Three verified defects:

1. Qualified group keys unparseable (group keys accept only Token::DotIdent,
   parser.rs:1238-1262). Fix: accept Ident DotIdent; keys become
   Unqualified(String) | Qualified(alias, field).
2. Qualified aggregate args silently null: rewrite_agg_expr extracts only Expr::Field
   inners (planner.rs:718); count(o.total) survives to eval_expr, which evaluates
   FunctionCall to Empty. Fix: extract QualifiedField (and JsonPath) inners, and make
   any aggregate surviving to eval a hard error. A database must not have
   silent-wrong-answer paths.
3. Key resolution under joins: joined columns are named "alias.field"; qualified keys
   resolve exactly, unqualified keys resolve by unique suffix match with an ambiguity
   error naming the candidates.

SQL GROUP BY lowers to PowQL group and inherits all fixes. count_distinct over joined
columns is tested as the sanctioned fan-out-safe count in the interim, and docs/POWQL.md
gains a fan-out section with the inflated-avg example until F2 ships.

### 5.2 FORK F2 (needs sign-off): symmetric aggregates as PowQL's default semantics

The strategy docs rank correct-by-default aggregation over relationships as the #1
differentiation wedge: on a one-to-many join, SQL's sum/avg silently double-count under
fan-out (the 8.67-vs-true-12.92 demo).

- Lane A: keep SQL semantics, document loudly, ship the wedge later (v0.13+) as an
  opt-in optimization gated on unique-index evidence.
- Lane B: make PowQL aggregates over joins symmetric BY DEFAULT, now. Definition: an
  aggregate whose argument has provenance alias A aggregates the distinct
  (group, rid_A) pairs; fan-out cannot inflate it. count(*) stays raw (no provenance).
  raw modifier opts out (sum(raw o.total)). The SQL frontend keeps SQL semantics, so
  SQL stays a compatibility surface while PowQL is the correct-by-default language.
  Key insight: dedup-by-RID needs no schema cardinality metadata; the join
  materializer already holds the source rows and only needs to carry each side's RID
  through as an internal column. Cardinality metadata (unique-index proof) enters later
  purely as an optimization that elides the dedup set.

RECOMMENDATION: Lane B, shipped in v0.13 with the expression-index release. The timing
argument is decisive: v0.11 makes grouped-join queries work for the first time, so
almost no user depends on fan-out semantics yet; a default is cheap to set now and
ruinous to change later. This is also the wedge demo the strategy asked for, with zero
new schema surface. Execution: per-aggregate FxHashSet<(group_idx, rid_u64)> within the
existing memory budget; Min/Max skip it (duplicate-insensitive); unique-index elision
and two-phase aggregate pushdown follow as optimizations. Hash-and-subtract rejected
(only invertible aggregates; dominated by dedup + pushdown).

### 5.3 Grouped fast path (v0.13, bench-gated)

Single-pass raw-byte grouped aggregation for the fast-path shape: GroupBy with
fixed-width keys and fixed int/float aggregate columns (or count(*)) over
SeqScan/compiled-Filter(SeqScan). Keys read at FastLayout offsets, hashed without
decode; aggregate columns read raw like agg_single_col_fast. Str keys are a second tier
(hash the inline var slice, materialize once per distinct group). Built only if the
v0.11/v0.12 benchmarks miss the 37ms-class target, per measure-first practice.

## 6. One-way-door register

| # | Decision | Why permanent | Direction | Escape hatch |
|---|---|---|---|---|
| D1 | No u32 row format, ever; row v2 = v1 + overflow bitmap | Row bytes persist in every heap | Bigness is out-of-line; stub carries u64 length | None needed: inline rows physically cap at 4070B |
| D2 | 24-byte stub layout | Persisted in rows | total_len u64, first_page, value crc32, flags, version, reserved | flags bits + stub_version + 6 reserved bytes |
| D3 | Overflow chunk format on PageType 3 | Persisted pages | 28B header, 4068B payload, singly linked, no back-pointers | reserved u16; page-format nibble |
| D4 | WAL logs overflow physically (types 11/12); row records stay inline-only | Replay compatibility | Page-id addressed, LSN-idempotent | WAL type space open-ended |
| D5 (F1) | JSON encoding: PJ1 binary, sorted keys, canonical bytes, key order not preserved | Heap + index bytes and user-visible semantics | RECOMMENDED binary; needs sign-off | 8 reserved tags; text was the reversible-but-identity-conceding alternative |
| D6 | TypeId 8 = Json | u8 ids persist | 8 = Json | 247 free slots |
| D7 | -> grammar, Expr::JsonPath, structural (non-slot) segments in plan cache | Language surface + #137 invariant | Postfix, tightest binding | Grammar is additive |
| D8 (F2) | PowQL aggregates over joins symmetric by default; SQL frontend stays raw | Default answers are the least reversible surface | RECOMMENDED symmetric, ship v0.13; needs sign-off | raw modifier; SQL surface unchanged |
| D9 | IndexKeySource { Column, Expression{canon_text} }, catalog v6 | Catalog format + index identity | General format now, JSON-path mechanism first | Staircase absorbs additive change |
| D10 | CompiledPredicate -> RowCtx/Tri, migrated once in v0.12 | Every fast path hangs off it; churn twice is waste | v0.11 routes v2 rows to fallback untouched; v0.12 migrates with the JSON leaf | Internal API (soft door), listed to prevent double churn |
| D11 | Wire: column-level type hints additive v0.13; per-cell tags deferred | Per-cell tags couple to nested-results | Stage it | Absent-field interop |
| D12 | sweep is the reclamation primitive; no vacuum/compaction beyond it | Operational surface | Mark-and-sweep overflow pages only | Seed for future compaction |

## 7. Phasing

Neither lane's phasing survives contact: JSON before overflow is half a feature (Capa's
documents exceed 4070B, so a JSON type they cannot store does not dechunk anything), and
everything-in-one-release is too heavy. Overflow and the P-5 fixes are disjoint crates
and parallelize as lanes.

- v0.11 "big rows, right answers" (~3 weeks, two parallel lanes)
  - Overflow complete: row v2 + stub, chains, WAL 11/12, spill policy, commit-time
    frees, rollback compensation (with the Drop-checkpoint regression test), per-row
    version routing, sweep (manual + post-recovery), heap v3 lazy bump, durability
    matrix. Storage crate, ~1,800 LoC.
  - P-5 fixes: qualified keys/args, suffix resolution, silent-null becomes error, docs
    + count_distinct tests. Query crate, ~300 LoC.
- v0.12 "json" (~3.5 weeks)
  - TypeId Json + PJ1 codec (+fuzz +property tests), -> grammar + Expr::JsonPath +
    structural cache hashing + #137 property test, eval walker, json_type(),
    RowCtx/Tri migration, compiled inline-doc leaf + overflow fetch-assemble walk,
    wire text cells + TS "json".
- v0.13 "fast and right by default" (~3 weeks)
  - Expression indexes (catalog v6, storage path-key maintenance, planner arm, runtime
    signature lowering, CREATE INDEX syntax). Symmetric aggregates (F2) + raw modifier
    + count(alias) + unique-index elision + the wedge demo. Grouped fast path if
    benchmarks demand. Wire type hints. SQL ->/->> lowering. sweep auto-trigger.
- Later, doors already open, no format changes: two-phase aggregate pushdown,
  compression flag, chunk-aligned PJ1, per-cell typed wire (with any nested-results
  decision), tombstone compaction on sweep's bones.

## 8. Test and bench plan

Correctness:
- Overflow round-trips at 4069/4070/4071/8K/16K/1M/64MB(cap error) per var type;
  multi-spill rows; inline<->spilled update transitions; delete frees; chain reuse;
  CRC fault injection surfaces typed errors.
- Durability (extend durability.rs suite): kill -9 mid-chain/post-commit matrix; double
  replay idempotency; rollback-then-Drop-checkpoint must not flush rolled-back chains
  (v0.10.0 regression class); sweep reclaims exactly the orphans (page accounting).
- Format: format_versioning.rs additions for row v2 / heap v3 / catalog v6 lazy bumps
  and old-gate refusals.
- PJ1: decoder fuzz target (fuzz.yml pattern); canonicalization property tests (dup
  keys, key-order permutations yield identical bytes); total-order model tests;
  differential validation against a dev-dependency JSON parser.
- Plan cache: #137 slot-count property test over path queries; different-path MISS,
  different-literal HIT.
- Aggregates: silent-null regression (error or correct, never Empty); qualified
  keys/args over hash and nested-loop joins; suffix ambiguity errors; SQL parity;
  symmetric-vs-raw golden tests on the 8.67/12.92 dataset when F2 ships.

Benchmarks (powdb-bench + powdb-compare, Depot-only baselines per standing policy):

| Bench | Shape | Baseline | Target |
|---|---|---|---|
| B1 | Capa 14,228-doc ingest UNCHUNKED + point reads | app-side chunking | point read < 100us; ingest >= chunked baseline |
| B2 | JSON-field filter, 77K docs | 1.7s EAV / PG 45ms | v0.12 compiled scan: inline beats 45ms, out-of-line within 1.5x; v0.13 indexed < 5ms |
| B3 | grouped agg over one-to-many join (S6) | 518ms app-side / PG 37ms | v0.11 in-engine < 100ms; v0.13 37ms-class |
| B4 | existing 20-workload suite | baseline/main.json | zero regression; v2 row branch budget < 2% on scan-heavy |

## 9. Risks

- PJ1 spec bugs are forever (F1). Fuzz + property + model tests before the first
  persisted byte; encoding reviewed against JSONB's known regrets; int/float split kept
  deliberately; decimal escape reserved.
- v0.11 storage scope is heavy. Overflow is one lane with its own durability matrix;
  if the release must split, overflow ships alone first (JSON depends on it, not vice
  versa).
- RowCtx migration touches every fast path (plan_exec.rs is 4,227 LoC). Mechanical Tri
  wrapper first; bench gate on the per-row branch.
- Symmetric-by-default (F2) could surprise SQL-minded users. It differs only where SQL
  is silently wrong; raw opt-out; SQL surface untouched; loud docs; and it ships while
  the grouped-join user base is near zero.
- Sweep under the table write lock stalls writers on huge files. Acceptable at v0.11
  frequency (post-recovery + manual); incremental ranges if it bites.
- Free-list/orphan edge cases. sweep is the backstop for any leak bug; powdb-cli verify
  gains a chain-walk checker (referenced vs allocated accounting).

## 10. Non-goals

- Non-1NF nested results, Value::List/Struct in rows, link/graph syntax (strategy
  deprioritized; PJ1's reserved tags keep the door open without building it).
- u32 row format (closed permanently by D1).
- Compression implementation (flag reserved only), JSON mutation operators,
  containment/GIN-style indexes (expression B-trees only), arbitrary-precision numerics.
- General VACUUM or tombstone compaction (sweep reclaims overflow pages only).
- Per-cell typed wire protocol now; Postgres/MySQL wire compatibility (standing rule).
- Grouping BY a JSON path expression (fast-follow once Expr-valued group keys are
  designed; aggregating OVER a JSON path works via the 5.1 extraction machinery).

## 11. Sign-off checklist

- [ ] F1: JSON storage encoding = PJ1 binary (recommended) vs minified text
- [ ] F2: PowQL grouped-join aggregates symmetric by default in v0.13 (recommended) vs
      SQL semantics + opt-in wedge later
- [ ] Phasing and release mapping (v0.11 overflow + P-5 fixes, v0.12 JSON, v0.13
      indexes + symmetric + fast paths)
- [ ] Everything else in the door register ships as specified
