# SQL frontend

PowDB now has an explicit SQL frontend in addition to native PowQL. SQL is a frontend only: the SQL parser lowers supported statements to the existing PowDB AST and records canonical PowQL text for the plan cache. The default wire `Query` message remains PowQL for backward compatibility; SQL uses `Engine::execute_sql(...)` in embedded Rust or the wire/client SQL query path.

> **No SQL mode in `powdb-cli`.** The CLI REPL is PowQL-only. Run SQL through the
> embedded API (`Engine::execute_sql`, or `db.querySql(...)` in the
> `@zvndev/powdb-embedded` Node addon) or the `QuerySql` wire path (e.g. the
> TypeScript client's `querySql`).

## Supported production subset

- `SELECT [DISTINCT] ... FROM ... [JOIN ... ON ...] [WHERE ...] [GROUP BY ...] [HAVING ...] [ORDER BY ...] [LIMIT ...] [OFFSET ...]`
- `INSERT INTO T (a, b) VALUES (1, 'x'), (2, 'y') [RETURNING *]`
- `UPDATE T SET a = ... WHERE ... [RETURNING *]`
- `DELETE FROM T WHERE ... [RETURNING *]`
- `CREATE TABLE T (...)`, including `NOT NULL`, `UNIQUE`, `DEFAULT <literal>`, and `AUTOINCREMENT` (alias `AUTO_INCREMENT`) column modifiers
- `CREATE [UNIQUE] INDEX name ON T (col)`
- `ALTER TABLE T ADD/DROP COLUMN ...`
- `DROP TABLE`, `DROP VIEW`
- `BEGIN [TRANSACTION]`, `COMMIT`, `ROLLBACK`

Supported expressions include literals, column references, qualified join references, arithmetic, boolean `AND`/`OR`/`NOT`, comparisons, `IS [NOT] NULL`, `LIKE`, aggregate/scalar function calls that already exist in PowQL, and `count(*)`.

`INSERT`/`UPDATE`/`DELETE` accept an optional trailing `RETURNING *`, which returns the affected rows in the same statement (insert/update return the post-image, delete returns the pre-image) — so an ORM gets its rows back in one round-trip instead of a write followed by a reselect. This lowers to PowQL's `returning` clause.

`AUTOINCREMENT` (on an `INTEGER` column) lowers to PowQL's `auto` modifier: an omitted column is assigned the next value from a per-table sequence, returned via `RETURNING *`. Combine with `INSERT ... RETURNING *` for the canonical *insert-without-the-id, read-it-back* flow.

## Intentional unsupported errors

The SQL frontend returns explicit unsupported-feature parse errors for SQL features that are not yet part of the production subset, including SQL `IN` lists/subqueries, SQL scalar/EXISTS subqueries, table constraints, SQL `BETWEEN`, and column-projected `RETURNING a, b` (only `RETURNING *` is supported, because PowQL's `returning` is all-columns). Use native PowQL for those shapes until the SQL subset is expanded.

The SQL JSON path operators `->` and `->>` are not yet part of the SQL frontend; they arrive in a later release. Until then, use native PowQL's `->` operator (see [JSON Documents](POWQL.md#json-documents)) to query `json` columns.

> **Where you see the explicit message.** These detailed messages reach
> **embedded** callers — the Rust `Engine::execute_sql` / `execute_sql_readonly`
> API and the in-process `@zvndev/powdb-embedded` Node addon, which propagate the
> `QueryError` verbatim. Over the **binary wire protocol**, the server sanitizes
> any error text it doesn't recognize as safe down to a generic
> `query execution error`, so a remote client (`QuerySql` / the TypeScript
> client) sees the generic message rather than the specific unsupported-feature
> text. Prototype SQL against the embedded API to read the exact reason.

## Plan-cache parity

Equivalent SQL and PowQL spellings share cached plans because SQL lowers to canonical PowQL before hashing:

```sql
SELECT name, age FROM User WHERE age > 25 ORDER BY age DESC LIMIT 10
```

lowers to:

```powql
User filter .age > 25 order .age desc limit 10 { .name, .age }
```
