# SQL frontend

PowDB now has an explicit SQL frontend in addition to native PowQL. SQL is a frontend only: the SQL parser lowers supported statements to the existing PowDB AST and records canonical PowQL text for the plan cache. The default wire `Query` message remains PowQL for backward compatibility; SQL uses `Engine::execute_sql(...)` in embedded Rust or the wire/client SQL query path.

## Supported production subset

- `SELECT [DISTINCT] ... FROM ... [JOIN ... ON ...] [WHERE ...] [GROUP BY ...] [HAVING ...] [ORDER BY ...] [LIMIT ...] [OFFSET ...]`
- `INSERT INTO T (a, b) VALUES (1, 'x'), (2, 'y') [RETURNING *]`
- `UPDATE T SET a = ... WHERE ... [RETURNING *]`
- `DELETE FROM T WHERE ... [RETURNING *]`
- `CREATE TABLE T (...)`, including `NOT NULL`, `UNIQUE`, and `DEFAULT <literal>` column modifiers
- `CREATE [UNIQUE] INDEX name ON T (col)`
- `ALTER TABLE T ADD/DROP COLUMN ...`
- `DROP TABLE`, `DROP VIEW`
- `BEGIN [TRANSACTION]`, `COMMIT`, `ROLLBACK`

Supported expressions include literals, column references, qualified join references, arithmetic, boolean `AND`/`OR`/`NOT`, comparisons, `IS [NOT] NULL`, `LIKE`, aggregate/scalar function calls that already exist in PowQL, and `count(*)`.

`INSERT`/`UPDATE`/`DELETE` accept an optional trailing `RETURNING *`, which returns the affected rows in the same statement (insert/update return the post-image, delete returns the pre-image) — so an ORM gets its rows back in one round-trip instead of a write followed by a reselect. This lowers to PowQL's `returning` clause.

## Intentional unsupported errors

The SQL frontend returns explicit unsupported-feature parse errors for SQL features that are not yet part of the production subset, including SQL `IN` lists/subqueries, SQL scalar/EXISTS subqueries, table constraints, SQL `BETWEEN`, and column-projected `RETURNING a, b` (only `RETURNING *` is supported, because PowQL's `returning` is all-columns). Use native PowQL for those shapes until the SQL subset is expanded.

## Plan-cache parity

Equivalent SQL and PowQL spellings share cached plans because SQL lowers to canonical PowQL before hashing:

```sql
SELECT name, age FROM User WHERE age > 25 ORDER BY age DESC LIMIT 10
```

lowers to:

```powql
User filter .age > 25 order .age desc limit 10 { .name, .age }
```
