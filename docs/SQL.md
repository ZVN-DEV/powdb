# SQL frontend

PowDB now has an explicit SQL frontend in addition to native PowQL. SQL is a frontend only: the SQL parser lowers supported statements to the existing PowDB AST and records canonical PowQL text for the plan cache. The default wire `Query` message remains PowQL for backward compatibility; SQL uses `Engine::execute_sql(...)` in embedded Rust or the wire/client SQL query path.

> **Running SQL from `powdb-cli`.** Start the REPL in SQL mode with `--sql`, run
> a single statement with the `.sql <STATEMENT>` meta-command, or switch an open
> session between dialects with `.sql` and `.powql`. The prompt changes to
> `sql>` so the active dialect is always visible. SQL is also available through
> the embedded API (`Engine::execute_sql`, or `db.querySql(...)` in the
> `@zvndev/powdb-embedded` Node addon) and the `QuerySql` wire path (e.g. the
> TypeScript client's `querySql`).

> **Comments.** SQL statements use `--`; PowQL uses `#` and rejects `--`. The two
> dialects do not share comment syntax, so translate comments when porting a
> snippet between them.

## Supported production subset

- `SELECT [DISTINCT] ... FROM ... [JOIN ... ON ...] [WHERE ...] [GROUP BY ...] [HAVING ...] [ORDER BY ...] [LIMIT ...] [OFFSET ...]`
- `INSERT INTO T (a, b) VALUES (1, 'x'), (2, 'y') [RETURNING *]`
- `UPDATE T SET a = ... WHERE ... [RETURNING *]`
- `DELETE FROM T WHERE ... [RETURNING *]`
- `CREATE TABLE T (...)`, including `PRIMARY KEY`, `NOT NULL`, `UNIQUE`, `DEFAULT <literal>`, and `AUTOINCREMENT` (alias `AUTO_INCREMENT`) column modifiers. A bare `NULL` modifier is accepted and ignored
- `CREATE [UNIQUE] INDEX name ON T (col)`
- `ALTER TABLE T ADD/DROP COLUMN ...`
- `DROP TABLE`, `DROP VIEW`
- `BEGIN [TRANSACTION]`, `COMMIT`, `ROLLBACK`

Supported expressions include literals, `$N` parameter placeholders, column references, qualified join references, arithmetic, boolean `AND`/`OR`/`NOT`, comparisons, `IS [NOT] NULL`, `LIKE`, aggregate/scalar function calls that already exist in PowQL, `count(*)`, and the JSON path operators `->` and `->>`.

Column types map onto PowDB's seven storage types plus `json`. A length specifier (`VARCHAR(255)`) parses and is ignored.

| SQL you write | PowDB type |
| --- | --- |
| `TEXT`, `VARCHAR`, `CHAR`, `STRING`, `STR` | `str` |
| `INT`, `INTEGER`, `BIGINT`, `SMALLINT` | `int` |
| `REAL`, `DOUBLE`, `FLOAT`, `DECIMAL`, `NUMERIC` | `float` |
| `BOOL`, `BOOLEAN` | `bool` |
| `DATETIME`, `TIMESTAMP` | `datetime` |
| `UUID` | `uuid` |
| `BLOB`, `BYTES`, `BYTEA` | `bytes` |
| `JSON`, `JSONB` | `json` |

`PRIMARY KEY` on a column lowers to PowQL's `required unique`. Written as a table constraint it is refused, naming the spelling that works: `PRIMARY KEY is supported only on the column it keys; write it after the column's type, as `id int PRIMARY KEY``.

`INSERT`/`UPDATE`/`DELETE` accept an optional trailing `RETURNING *`, which returns the affected rows in the same statement (insert/update return the post-image, delete returns the pre-image) — so an ORM gets its rows back in one round-trip instead of a write followed by a reselect. This lowers to PowQL's `returning` clause.

`AUTOINCREMENT` (on an `INTEGER` column) lowers to PowQL's `auto` modifier: an omitted column is assigned the next value from a per-table sequence, returned via `RETURNING *`. Combine with `INSERT ... RETURNING *` for the canonical *insert-without-the-id, read-it-back* flow.

## Identifiers and string literals

Double quotes delimit an **identifier**. Single quotes delimit a **string**.
That is the standard SQL rule, and it is what every ORM emits, since Prisma,
Django, SQLAlchemy and ActiveRecord all quote identifiers as a matter of
course.

```sql
SELECT "name" FROM "Author" ORDER BY "id";       -- the name column of the Author table
SELECT name FROM Author WHERE name = 'alice';    -- 'alice' is a string
SELECT name FROM Author WHERE name = "name";     -- compares the column to itself: every row
```

The last line is the trap worth internalizing: the two quote characters are not
interchangeable, so a quoted *identifier* on the right of a comparison is a
column-to-column comparison, not a string match.

A quoted identifier is never a keyword, which is the entire reason delimited
identifiers exist. The lexer re-emits a double-quoted token as a PowQL
backtick-quoted word, which bypasses every keyword check downstream, and it does
**no case folding** in either direction: identifiers are case-sensitive, so
`SELECT ID FROM T` reports `column 'ID' not found` on a column named `id`.

```sql
SELECT "limit", "order" FROM T;   -- columns named limit and order
```

Quoting is not usually *required*, though. PowQL's roughly 100 reserved words
are not reserved here: `SELECT order FROM T`, `ORDER BY order` and
`WHERE limit = 5` all work unquoted, because the frontend emits those as PowQL
dotted field references, which bypass keyword lookup. What genuinely needs
quoting is one of **SQL's own** twelve reserved words in a position that expects
a bare identifier: `select`, `from`, `where`, `insert`, `into`, `values`,
`update`, `set`, `delete`, `create`, `table`, `alter`. `CREATE TABLE S (select INT)`
is refused with ``expected column name, got reserved word `select` ``;
`CREATE TABLE S ("select" INT)` succeeds.

Two quoted identifiers are refused rather than silently mangled, because PowQL
quotes identifiers with backticks and has no escape for one: an empty identifier
(`""`), and an identifier containing a backtick.

A backslash escape inside either quote form follows the same rule as a string
literal (see below); an unknown one is a parse error, not a silently dropped
backslash.

Quoting works anywhere an identifier is legal, including table names, aliases,
and qualified references:

```sql
SELECT "a"."name" FROM Author AS "a" WHERE "a"."age" > 20;
```

### String escapes

The escape set is `\'`, `\"`, `\\`, `\n`, `\t`, `\r`, `\0` and `\uXXXX`
(four hex digits). **Any other escape is a parse error**, in a single-quoted
string and in a double-quoted identifier alike:
``unknown escape '\c' in string literal; SQL supports \' \" \\ \n \t \r \0 and \uXXXX``.

Before this release an unrecognized escape silently dropped its backslash, so
`'back\slash'` was read as `backslash` and a quoted identifier `"i\d"` resolved
to `id`. That meant the same text meant two different things in PowDB's two
frontends. It does not now: PowQL's set is identical minus `\'`, which it has
no use for because PowQL strings are double-quoted.

## Intentional unsupported errors

SQL outside the production subset returns an explicit unsupported-feature parse error that names the construct. These are terminal errors, never a silent wrong answer, and where a working spelling exists the message names it.

| SQL you wrote | The error you get | What works instead |
| --- | --- | --- |
| `CASE WHEN <cond> THEN a ELSE b END` | `SQL CASE/WHEN is not supported yet in the SQL frontend` | PowQL: `case when <cond> then <value> else <value> end` |
| `COALESCE(a, b)` | `SQL COALESCE is not supported yet in the SQL frontend` | PowQL: `.a ?? .b` |
| `COUNT(DISTINCT col)`, and `SUM`/`AVG`/`MIN`/`MAX (DISTINCT ...)` | `SQL COUNT(DISTINCT ...) is not supported yet in the SQL frontend`, and the matching message per function | PowQL: `count(distinct T { .col })`. The message names the PowQL spelling for `COUNT` only |
| `CAST(x AS INT)` | `SQL CAST(x AS TYPE) is not supported yet in the SQL frontend` | The two-argument form, which SQL mode also accepts: `cast(x, 'int')` |
| `row_number() OVER (...)` | `SQL window functions (OVER) are not supported yet in the SQL frontend` | PowQL: `row_number() over (partition .dept order .id)` |
| `x IN (1, 2)`, `x IN (SELECT ...)` | `SQL IN lists/subqueries are not supported yet in the SQL frontend` | PowQL: `.x in (1, 2)`, `.x in (T { .col })` |
| `EXISTS (SELECT ...)` | `SQL EXISTS subqueries are not supported yet; use PowQL EXISTS for now` | PowQL `exists` |
| a scalar subquery, e.g. `WHERE x = (SELECT ...)` | `SQL scalar subqueries are not supported yet; use PowQL subqueries for now` | PowQL subqueries |
| `x BETWEEN 1 AND 2` | `SQL BETWEEN is not supported yet in the SQL frontend` | SQL: `x >= 1 AND x <= 2` |
| `PRIMARY KEY (col)` as a table constraint | ``PRIMARY KEY is supported only on the column it keys; write it after the column's type, as `id int PRIMARY KEY` `` | the column modifier, which **is** supported and lowers to `required unique` |
| any other table constraint (`FOREIGN KEY`, `CONSTRAINT`, table-level `UNIQUE`, `CHECK`) | `SQL table constraints are not supported; declare UNIQUE columns or add indexes explicitly` | column modifiers, or `CREATE INDEX` |
| `RETURNING a, b` | ``RETURNING currently supports only `RETURNING *` (column projection is not yet supported)`` | `RETURNING *`, because PowQL's `returning` is all-columns |
| two aggregates, or an aggregate beside a plain column, with no `GROUP BY` | `multiple aggregates, or an aggregate mixed with plain columns, without GROUP BY are not supported; aggregate a single expression or add GROUP BY` | add `GROUP BY`, or run one aggregate per statement |
| `SELECT DISTINCT count(x)` with no `GROUP BY` | `aggregates with DISTINCT and no GROUP BY are not supported by the SQL frontend` | add `GROUP BY`, or drop the `DISTINCT` |

Four more constructs are unsupported and, unlike every row above, do **not**
name themselves. The parser reads each one as something else first, so the error
points at the wrong token. Do not read these messages as bugs in your SQL:

| SQL you wrote | The error you get | Why it reads that way |
| --- | --- | --- |
| `SELECT ... UNION SELECT ...` | `unexpected trailing SQL token: SELECT` | `UNION` is consumed as a table alias, so the parser is already past it |
| `WITH x AS (...) SELECT ...` | `expected SQL statement, got WITH` | no CTE production exists |
| `DROP TABLE IF EXISTS T` | `unexpected trailing SQL token: EXISTS` | `IF` is consumed as the table name |
| `CREATE TABLE IF NOT EXISTS T (...)` | `expected (, got NOT` | same, then the column list is expected |

`CREATE VIEW` has no production either, though `DROP VIEW` is supported. Declare
a materialized view in PowQL with `materialize`.

`CAST` is worth spelling out, because the accepted form is not the SQL one: write `cast(x, 'int')`, with the target type as a string argument. The valid type strings are `int`, `float`, `str`, `bool`, `datetime`, `uuid`, and `bytes`.

Every row above is a subset gap rather than a refusal on principle, so read the table as the current boundary and not a permanent one.

Nested projections (shaped, one-row-per-parent results with children as JSON arrays) have no spelling in **PowDB's SQL frontend**, and that is a deliberate boundary rather than a pending subset gap: SQL's `SELECT` list is flat and PowDB does not add a dialect extension for it. In SQL here, use a join and regroup client-side, or run the PowQL query directly.

They are not something SQL as a language cannot express. An engine with JSON aggregate functions produces the same shape with a correlated subquery, for example SQLite's `(SELECT json_group_array(json_object('total', o.total)) FROM "Order" o WHERE o.user_id = u.id)`, empty array for a childless parent included. What PowQL offers over that spelling is brevity, a correlation the catalog can declare once, and a result that stays a typed binary value on the wire instead of JSON text the client re-parses. Entity links are the same story: the relationship is of course expressible in SQL by writing the join condition out in every query. See [Nested Projections (Shaped Results)](POWQL.md#nested-projections-shaped-results) in the PowQL reference.

`CREATE INDEX` and `CREATE UNIQUE INDEX` accept either one stored column or a
direct JSON `->` path:

```sql
CREATE INDEX post_author ON Post ((data->'author'->'name'));
CREATE UNIQUE INDEX post_external_id ON Post ((data->'externalId'));
```

The extra expression parentheses are optional for a direct path. `->>` text
extraction, arithmetic expressions, functions, and multi-column indexes remain
outside the production subset. Native PowQL exposes the same path-index feature
as `alter T add index (.data->path)`.

## Result column names

An unaliased column is named after what it computes, in the spelling of the
language you wrote it in.

- **SQL, grouped:** `SELECT txt, count(id) FROM P GROUP BY txt` returns the
  headers `txt` and `count(id)`. `count(*)` is headed `count(*)`.
- **PowQL, grouped:** `P group .txt { .txt, count(.id) }` returns `txt` and
  `count(.id)`, with the leading dot PowQL writes.
- **An ungrouped SQL aggregate has no column name at all**, because it lowers to
  a PowQL scalar aggregate: `SELECT count(txt) FROM P` returns a single scalar
  value, not a one-column row.

Before this release an unaliased aggregate was headed by the planner's internal
`__agg_0`, and an unaliased expression was headed `?`. Alias explicitly
(`count(id) AS n`) if you depend on the header text.

## NULL comparison semantics

PowDB follows SQL NULL semantics for comparisons: a `NULL` (missing) value
**never matches a comparison**. A row whose compared column is `NULL` is
excluded from `<`, `<=`, `>`, `>=`, `=`, and `!=` against any non-null value,
including `col != <value>`. Test for presence with `IS NULL` / `IS NOT NULL`,
not with a comparison. This holds on every execution path (indexed, compiled
fast path, generic, JSON `->` path comparisons).

```sql
-- rows where age IS NULL are excluded from all of these:
SELECT * FROM User WHERE age < 30;
SELECT * FROM User WHERE age = 30;
SELECT * FROM User WHERE age != 30;   -- NULL is excluded, not treated as "!= 30"
SELECT * FROM User WHERE age IS NOT NULL;   -- use this to select present values
```

### `= NULL` selects different rows in SQL and PowQL

Comparing *against the NULL literal* is the one source text that selects a
different set of rows in PowDB's two frontends, and the difference is
deliberate. Every other filter in SQL lowers to exactly the predicate the
equivalent PowQL filter lowers to.

PowQL desugars `x = null` to `x is null` as a documented convenience. SQL does
not: a comparison against `NULL` is UNKNOWN, so `WHERE x = NULL` and
`WHERE x <> NULL` select no rows, exactly as they do in every other engine. The
SQL frontend lowers a NULL comparison to a constant-false predicate rather than
inheriting PowQL's spelling.

```sql
SELECT name FROM Author WHERE age = NULL;    -- no rows
SELECT name FROM Author WHERE age <> NULL;   -- no rows
SELECT name FROM Author WHERE age IS NULL;   -- the rows with no age
```

```powql
Author filter .age = null { .name }          # the rows with no age
```

Both are right in their own language, and collapsing them to one meaning would
break the other. Lowering SQL to the PowQL meaning hands ported SQL the
`IS NULL` rows, which is the opposite row set from what the author wrote, on
the most commonly written incorrect SQL idiom. Lowering PowQL to the SQL
meaning removes a documented convenience from the native language. If you are
porting a query and want the presence test, write `IS NULL` in SQL.

Aggregate mode is a further deliberate difference between the two frontends,
and is described under
[Aggregate semantics over joins](#aggregate-semantics-over-joins). It changes
which value an aggregate returns, not which rows a filter selects.

### `NOT` is two-valued

This is a divergence from the SQL *standard* rather than from PowQL, and the
two frontends share it. PowDB filter logic is two-valued, so `NOT (expr)` is
the plain complement of `expr`. Because a comparison against `NULL` evaluates
to false (not the SQL "unknown"), `WHERE NOT (age > 30)` **includes** rows
where `age IS NULL` (the inner `age > 30` is false, so `NOT` makes it true),
whereas standard three-valued SQL would exclude them. Guard presence
explicitly when that matters:

```sql
SELECT * FROM User WHERE age IS NOT NULL AND NOT (age > 30);
```

The same rule applied to the constant-false lowering above means
`WHERE NOT (age = NULL)` returns **every** row, where three-valued SQL returns
none. Both halves of that are already-documented behavior meeting each other,
not a third divergence, but it is worth knowing before you write it.

`JOIN ... ON` key equality is separate: PowDB deliberately matches two missing
keys (`Empty = Empty`) so nullable-key rows join, rather than applying the
filter comparison rule. That behavior is unchanged.

## JSON path operators

Both SQL arrow operators accept a string object key or a non-negative integer
array index. They bind as postfix operators, so a chained path is evaluated
before arithmetic or comparison:

```sql
SELECT data->'author'->'name' AS author,
       data->'tags'->0 AS first_tag,
       data->>'views' AS views_text
FROM Post
WHERE data->'views' > 10
ORDER BY data->'views' DESC;
```

- `->` returns the extracted PowDB value: strings, numbers, and booleans keep
  their scalar types, while objects and arrays remain JSON subdocuments.
- `->>` returns text. Strings are unquoted, numbers and booleans use their
  canonical text, and objects or arrays use canonical JSON text.
- A missing path or explicit JSON null returns SQL/PowDB Empty for either
  operator.
- Negative and fractional array indexes are rejected.

PowDB SQL intentionally inherits PowQL's scalarizing `->` behavior. This is
not PostgreSQL's JSON-value-preserving behavior, and direct `->` does not
distinguish a missing path from explicit JSON null. Use `json_type()` when that
distinction matters.

## COUNT and NULL

`COUNT(*)` counts rows. `COUNT(col)` counts rows where `col` is not NULL, per
the SQL standard, and lowers to PowQL's `count(T { .col })`, so both frontends
return the same number for the same question. Every other aggregate
(`SUM`/`AVG`/`MIN`/`MAX`) also skips NULL values, and each of them returns
NULL when no non-null value contributes at all (no rows, or all NULL), per
the SQL standard. `COUNT` is the aggregate that answers `0` for no rows.

```sql
-- 3 rows, one of which has a NULL nickname:
SELECT COUNT(*) FROM User;          -- 3
SELECT COUNT(nickname) FROM User;   -- 2
```

## Aggregate semantics over joins

SQL aggregates evaluate the joined rows directly, including join fan-out. For
example, if one account joins to four entries, its balance contributes four
times to `SUM(account.balance)`. This is standard SQL behavior.

Native PowQL uses symmetric source-row semantics by default and exposes `raw`
as an explicit opt-out. The SQL frontend always lowers aggregates with raw
semantics, so a cached plan cannot inherit PowQL's symmetric behavior based on
which dialect ran first.

> **Where you see the explicit message.** Everywhere. Embedded callers — the
> Rust `Engine::execute_sql` / `execute_sql_readonly` API and the in-process
> `@zvndev/powdb-embedded` Node addon — get the `QueryError` verbatim, and the
> server's wire sanitizer recognizes these unsupported-feature diagnostics as
> safe to forward, so a remote client (`QuerySql` / the TypeScript client)
> receives the same message text with error class 1 (parse).

## Plan-cache parity

Equivalent SQL and PowQL spellings share cached plans because SQL lowers to canonical PowQL before hashing:

```sql
SELECT name, age FROM User WHERE age > 25 ORDER BY age DESC LIMIT 10
```

lowers to:

```powql
User filter .age > 25 order .age desc limit 10 { .name, .age }
```
