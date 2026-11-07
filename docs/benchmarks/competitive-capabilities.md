# Capability evidence accompanying relational reads v1

Reviewed 2026-11-06. These are scoped requirements, not a claim that a database
is secure, standards-compliant or suitable for every workload. Runtime probe
outcomes come from the collector's scorecard; documentation evidence is labeled
separately. Unknown behavior earns no asserted capability.

## Query and delivery coverage

| Requirement | Evidence | What remains untested |
| --- | --- | --- |
| Point lookup, bound parameters, ranges | Distinct-key timed probes with typed result checks | Plan quality across skew, distributions, composite indexes |
| Combined filters, joins, grouping, sorting | Equivalent SQL/PowQL with ordered digest checks | Large join fanout, more than two tables, complex correlated plans |
| Deep pagination | Same logical page through offset/keyset, indexed ID and unindexed `(score,id)` | Concurrent mutations, snapshot consistency, very deep pages at scale |
| Large results | Narrow/wide materialization and bounded consumption, first-row/completion latency and PSS | Slow consumers, cancellation during transfer, TCP/TLS and backpressure |
| Types | Exact int64 beyond JavaScript safe range, nulls, text, aggregate float | Decimal, timestamps/timezones, UUID, binary/JSON and nested shape semantics |
| SQL support | Every SQL surface is tested independently | Full SQL compliance; passing one probe does not establish all syntax variants |

SQLite, DuckDB and PowDB expose embedded APIs. PostgreSQL and MySQL here are
servers. DuckDB also has multiple [concurrency modes](https://duckdb.org/docs/2.0/connect/concurrency),
including multiple writer threads inside a single writer process. SQLite's
[threading modes](https://www.sqlite.org/threadsafe.html) determine connection
sharing rules. These are different from distributed replication or from the
throughput of concurrent clients, which this read profile does not measure.

## Documented server authorization requirements

The following four equally weighted requirements describe an application that
needs named accounts, encrypted transport and fine-grained grants. Coverage is
an inventory of controls, not a security-strength percentage. SQLite and DuckDB
are outside this server-account profile; their host application supplies identity
and isolation.

| Requirement | PowDB 0.29.2 | PostgreSQL 18 | MySQL 8.4 |
| --- | --- | --- | --- |
| Named authenticated accounts | Present, coarse admin/readwrite/readonly roles | Present | Present |
| Native TLS transport | Present, opt-in configuration | Present | Present |
| Per-table privileges | Absent | Present | Present |
| Per-column privileges | Absent | Present | Present |
| Documented requirement coverage | **2 / 4** | **4 / 4** | **4 / 4** |

Sources: PowDB's [security policy](../../SECURITY.md) and [language/feature guide](../../AGENTS.md);
PostgreSQL [client authentication](https://www.postgresql.org/docs/18/client-authentication.html),
[TLS](https://www.postgresql.org/docs/18/ssl-tcp.html) and
[GRANT](https://www.postgresql.org/docs/18/sql-grant.html);
MySQL [privilege system](https://dev.mysql.com/doc/refman/8.4/en/privileges-provided.html)
and [encrypted connections](https://dev.mysql.com/doc/refman/8.4/en/using-encrypted-connections.html).
These controls were reviewed in documentation; they were not exercised by the
performance collector. The score does not assess defaults, deployment mistakes,
privilege escalation, authentication timing or isolation under attack.

PostgreSQL additionally documents native
[row-security policies](https://www.postgresql.org/docs/18/ddl-rowsecurity.html),
including owner/superuser bypass rules. PowDB has no row-level policy engine.
MySQL row-policy equivalents were not evaluated in this review. This separate
requirement is not silently inferred from table grants or included in the four
requirements above.

## Embedded untrusted-input requirements

[SQLite's security guidance](https://www.sqlite.org/security.html) describes
fuzzing, defensive mode, runtime limits, authorization hooks and interrupt/progress
controls. Those mechanisms require application configuration; SQLite's presence
inside an app does not establish safe handling of arbitrary SQL or files.

[DuckDB's security guidance](https://duckdb.org/docs/2.0/operations_manual/securing_duckdb/overview)
treats untrusted SQL as code running with the process user's privileges. Its
external-access and extension settings provide additional controls, not an OS
sandbox. The benchmark disables external access and extension autoload/install.

PowDB documents bounded frames, query resource limits, cooperative cancellation,
coarse roles and readonly enforcement in its [security policy](../../SECURITY.md).
This review does not establish a sandbox, per-table isolation, a complete hostile
query test suite or a third-party security audit. Rust does not remove unsafe
storage code, dependency or authorization risks.

A future behavioral security score needs a public, fixed test corpus: parameter
injection, readonly mutation attempts, authorization boundaries, resource
exhaustion, malformed frames/files, cancellation and TLS identity validation.
Today these cross-engine outcomes are **unmeasured**. CVE counts are not used as
a strength ranking because maturity, disclosure practices and deployment scope
are confounders.

## Operational coverage still requiring evidence

| Axis | Required evidence before scoring |
| --- | --- |
| Durable write latency/throughput | Identical commit semantics, batch sizes, WAL/fsync settings and verified persistence |
| Mixed concurrency | Client/thread counts, isolation, conflicts, tail latency and fairness |
| Recovery | Kill/power-loss simulations, committed-prefix checks, corruption detection and restart time |
| Memory pressure/spill | Equal enforced budgets, working sets exceeding memory, OOM behavior and temporary I/O |
| Storage efficiency | Logical payload versus table/index/WAL bytes, compression and amplification |
| Backup/restore | Consistent online/offline backups, restore verification, recovery objectives |
| Distributed operation | Replication, failover and consistency guarantees; PowDB is single-node |
| Query control | Timeouts, cancellation responsiveness and memory release after interruption |
| Deployment surfaces | TCP/TLS, driver costs, Node/TypeScript typed transport, cold startup |

These rows are deliberately unscored. The relational read score must not be
presented as evidence for them or used to advertise a universal winner.
