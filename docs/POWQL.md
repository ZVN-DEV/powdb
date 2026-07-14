# PowQL Language Reference

> Looking for SQL syntax? See [`docs/SQL.md`](SQL.md). SQL is an explicit frontend that lowers to the existing PowDB AST; native PowQL remains the default wire/query language.

PowQL is the query language for PowDB, a Rust-native embedded database with compiled query execution. PowQL is designed to be modern, concise, and pipeline-oriented while remaining immediately familiar to anyone who knows SQL.

---

## Table of Contents

1. [Quick Start](#quick-start)
2. [Schema Definition](#schema-definition)
3. [Queries](#queries)
4. [Expressions](#expressions)
5. [Aggregates](#aggregates)
6. [GROUP BY and HAVING](#group-by-and-having)
7. [Joins](#joins)
8. [Set Operations](#set-operations)
9. [Subqueries](#subqueries)
10. [Functions](#functions)
11. [JSON Documents](#json-documents)
12. [Mutations](#mutations)
13. [Transactions](#transactions)
14. [DDL](#ddl)
15. [Introspection](#introspection)
16. [Reserved Words and Quoting](#reserved-words-and-quoting)
17. [Materialized Views](#materialized-views)
18. [Window Functions](#window-functions)
19. [UPSERT](#upsert)
20. [EXPLAIN](#explain)
21. [Prepared Queries](#prepared-queries)
22. [Type System](#type-system)
23. [PowQL vs SQL Cheat Sheet](#powql-vs-sql-cheat-sheet)

---

## Quick Start

PowQL reads left to right. You name the table, apply operations, and project fields -- all in one pipeline.

```
-- Define a schema
type User {
  required name: str,
  required email: str,
  age: int
}

-- Insert a row
insert User { name := "Alice", email := "alice@example.com", age := 30 }

-- Scan all users
User

-- Filter, order, limit, project -- one pipeline
User filter .age > 25 order .age desc limit 10 { .name, .age }

-- Count rows matching a condition
count(User filter .age > 30)

-- Group and aggregate
User group .status having count(.name) > 5 { .status, n: count(.name) }
```

**PowQL vs SQL at a glance:**

| PowQL | SQL |
|---|---|
| `User filter .age > 30 { .name }` | `SELECT name FROM User WHERE age > 30` |
| `count(User filter .active = true)` | `SELECT COUNT(*) FROM User WHERE active = true` |
| `User order .age desc limit 5` | `SELECT * FROM User ORDER BY age DESC LIMIT 5` |
| `insert User { name := "Alice" }` | `INSERT INTO User (name) VALUES ('Alice')` |
| `User filter .id = 1 update { age := 31 }` | `UPDATE User SET age = 31 WHERE id = 1` |
| `User filter .id = 1 delete` | `DELETE FROM User WHERE id = 1` |

---

## Schema Definition

Tables are defined using the `type` keyword. Each field has a name and a type, optionally prefixed with the modifiers `required` (enforce non-null values) and/or `unique` (enforce that no two non-null rows share a value). The modifiers may appear in either order.

Declaring a field `unique` automatically creates a unique B+tree index on that column; duplicate inserts/updates/upserts are then rejected with a `unique constraint violation` error.

A field may declare a literal **`default`** after its type — the value applied when an insert (or upsert-insert) omits that column. The default is applied before the required-column check, so a `required` column with a default may be omitted. Defaults must be scalar literals (`int`, `float`, `str`, `bool`); expression defaults (e.g. a generated timestamp) are not yet supported. A default whose type does not match the column is rejected at `type`-creation time.

An integer field may declare the **`auto`** modifier (typically `unique auto id: int`) — when an insert omits it, the engine assigns the next value from a per-table sequence. The assigned id comes back through `insert ... returning`. The sequence resumes above the highest existing id after a restart (recovered from the data, so a process crash never reuses an id of a committed row). An explicit value is allowed and pushes the sequence past it. `auto` requires an `int` column and cannot be combined with `default`. Auto-assignment applies on `insert` (not on `upsert`).

```powql
type Account {
    unique auto id: int,
    status: str default "active",
    credits: int default 0,
    verified: bool default false
}
```

### Syntax

```
type <TableName> {
  [required] [unique] <field>: <type>,
  [required] [unique] <field>: <type>,
  ...
}
```

### Examples

```
-- A simple user table; email must be unique across all rows
type User {
  required name: str,
  required unique email: str,
  age: int
}

-- A table with all supported types
type Record {
  required id: int,
  required title: str,
  score: float,
  active: bool,
  created_at: datetime,
  ref_id: uuid,
  payload: bytes
}
```

Fields without `required` are nullable -- they can hold empty/null values. Null values are exempt from the `unique` constraint (multiple rows may be null).

### Supported Types

| Type | Description | Storage |
|---|---|---|
| `str` | UTF-8 text | Variable-length |
| `int` | 64-bit signed integer | 8 bytes fixed |
| `float` | 64-bit floating point (IEEE 754) | 8 bytes fixed |
| `bool` | Boolean (true/false) | 1 byte fixed |
| `datetime` | Timestamp as 64-bit integer (epoch) | 8 bytes fixed |
| `uuid` | 128-bit UUID | 16 bytes fixed |
| `bytes` | Raw binary data | Variable-length |
| `json` | JSON document (object, array, or scalar) | Variable-length |

`json` columns store a whole JSON document and support path extraction with
the `->` operator. See [JSON Documents](#json-documents) for the storage
semantics you need to know (keys are sorted, not insertion-ordered) and a
worked example.

---

## Queries

Queries in PowQL are pipeline-oriented: start with a table name, then chain operations left to right.

### Full Scan

Read every row from a table:

```
User
```

### Filter

Apply a predicate to keep only matching rows:

```
User filter .age > 30
User filter .name = "Alice"
User filter .age > 25 and .status = "active"
User filter .age < 20 or .age > 60
```

### Projection

Select specific fields using `{ }` braces. Reference fields with the `.field` dot syntax:

```
User { .name, .email }
User filter .age > 30 { .name, .age }
```

Projections can include aliases:

```
User { full_name: .name, years: .age }
```

Projections can include computed expressions:

```
User { .name, double_age: .age * 2 }
User { .name, info: concat(.name, " age=", .age) }
```

### Ordering

Sort results using `order` with one or more fields. Default direction is ascending. Use `asc` or `desc` explicitly:

```
User order .age
User order .age desc
User order .name asc
User order .age asc, .name desc
```

### Limit and Offset

Restrict the number of returned rows and skip rows:

```
User limit 10
User order .age desc limit 5
User order .age offset 20 limit 10
```

### Distinct

Remove duplicate rows from the result:

```
User distinct { .name }
User filter .age > 20 distinct { .status }
```

### Pipeline Composition

Operations compose naturally left to right. The full pipeline order is:

```
<Table> [distinct] [filter <expr>] [group <keys> [having <expr>]] [order <keys>] [limit <n>] [offset <n>] { <projection> }
```

A complete example:

```
User filter .age > 18 order .name asc limit 100 offset 20 { .name, .email, .age }
```

SQL equivalent:

```sql
SELECT name, email, age FROM User WHERE age > 18 ORDER BY name ASC LIMIT 100 OFFSET 20
```

---

## Expressions

PowQL supports a full expression language for filters, projections, and assignments.

### Field References

Fields are referenced with a dot prefix:

```
.name
.age
.email
```

In join queries, use qualified references with the alias:

```
u.name
o.total
```

### Literals

| Type | Examples |
|---|---|
| Integer | `42`, `-7`, `0` |
| Float | `3.14`, `-0.5` |
| String | `"hello"`, `"Alice"` |
| Boolean | `true`, `false` |

### Parameters

Positional placeholders `$1`, `$2`, … bind untrusted values without string
interpolation. They are 1-based (`?` is not a placeholder — `??` is the
COALESCE operator):

```
User filter .name = $1
User filter .age > $1 and .age <= $2
insert User { name := $1, email := $2, age := $3 }
```

Binding happens at the **token level**: each `$N` is replaced with the
literal token for the supplied value *before* parsing, so an
injection-shaped string is inert data and can never change the query's
shape. A `null` parameter binds PowQL `null`. A placeholder with no
matching argument (or a `$0`) is a clean parse error.

Over the wire this is the `client.query(powql, params)` form (see
[AGENTS.md](../AGENTS.md) for the client API and the `QueryWithParams`
message). For the in-process Rust execution API, see
[Prepared Queries](#prepared-queries).

### Comparison Operators

| Operator | Meaning |
|---|---|
| `=` | Equal |
| `!=` | Not equal |
| `<` | Less than |
| `>` | Greater than |
| `<=` | Less than or equal |
| `>=` | Greater than or equal |

```
User filter .age > 30
User filter .name = "Alice"
User filter .score != 0
```

### Arithmetic Operators

| Operator | Meaning | Precedence |
|---|---|---|
| `*` | Multiply | Higher |
| `/` | Divide | Higher |
| `+` | Add | Lower |
| `-` | Subtract | Lower |

Standard precedence applies -- `*` and `/` bind tighter than `+` and `-`:

```
User { .name, double_age: .age * 2 }
User filter .age / 10 > 2
User filter .price * .quantity > 100
User filter .a + .b * .c > 0   -- parsed as .a + (.b * .c)
```

Use parentheses to override precedence:

```
User filter (.a + .b) * .c > 0
```

### Logical Operators

| Operator | Meaning |
|---|---|
| `and` | Logical AND |
| `or` | Logical OR |
| `not` | Logical NOT |

```
User filter .age > 25 and .status = "active"
User filter .age < 20 or .age > 60
User filter not .active
```

### NULL Checks

```
User filter .age is null
User filter .age is not null
User filter .age is null and .name = "Diana"
```

### IN Lists

Check if a value is in a set of literals:

```
User filter .name in ("Alice", "Bob")
User filter .age in (25, 30, 35)
User filter .name not in ("Alice")
User filter .age not in (1, 2, 3)
```

### BETWEEN

Range check (inclusive on both ends). Desugars to `>= low AND <= high`:

```
User filter .age between 25 and 35
User filter .age not between 10 and 20
```

### LIKE

Pattern matching with `%` (any sequence) and `_` (single character):

```
User filter .name like "Ali%"        -- starts with "Ali"
User filter .name like "_ob"         -- 3 chars ending in "ob"
User filter .name like "Alice"       -- exact match
User filter .name not like "A%"      -- does NOT start with "A"
```

### Coalesce

The `??` operator returns the left operand if non-null, otherwise the right:

```
User { .name, display_age: .age ?? 0 }
```

### Operator Precedence

From highest to lowest binding:

| Precedence | Operators |
|---|---|
| 1 (tightest) | `*`, `/` |
| 2 | `+`, `-`, `??` |
| 3 | `=`, `!=`, `<`, `>`, `<=`, `>=`, `like`, `in`, `between`, `is null`, `is not null` |
| 4 | `not` |
| 5 | `and` |
| 6 (loosest) | `or` |

Use parentheses to override: `(.age > 25 or .role = "admin") and .active = true`

---

## Aggregates

PowQL supports five aggregate functions. They wrap a query in a function-call syntax.

### Standalone Aggregates

```
count(User)                              -- count all rows
count(User filter .age > 30)             -- count with filter
sum(User { .age })                       -- sum a column
sum(User filter .age > 30 { .age })      -- sum with filter
avg(User { .age })                       -- average
min(User { .age })                       -- minimum
max(User { .age })                       -- maximum
```

### count(distinct ...)

Count unique values in a column:

```
count(distinct User { .name })
count(distinct User { .age })
```

### Aggregate Functions Reference

| Function | Description | Syntax |
|---|---|---|
| `count` | Number of rows | `count(Table [filter ...])` |
| `count(distinct ...)` | Number of unique values | `count(distinct Table { .field })` |
| `sum` | Sum of numeric column | `sum(Table { .field })` |
| `avg` | Average of numeric column | `avg(Table { .field })` |
| `min` | Minimum value | `min(Table { .field })` |
| `max` | Maximum value | `max(Table { .field })` |

For `sum`, `avg`, `min`, and `max`, the target column is specified via the projection `{ .field }`. For `count`, the projection is optional.

---

## GROUP BY and HAVING

Group rows by one or more keys and compute aggregate values per group.

### Syntax

```
<Table> [filter ...] group <.key1>, <.key2> [having <expr>] { <.key>, <agg(...)> }
```

### Basic Grouping

```
-- Count users per name
User group .name { .name, n: count(.name) }

-- Group by multiple keys
User group .status, .age { .status, .age }
```

### Aggregates in GROUP BY Projections

Inside a GROUP BY projection, you can use any aggregate function:

```
User group .status {
  .status,
  total: count(.name),
  avg_age: avg(.age),
  youngest: min(.age),
  oldest: max(.age),
  total_age: sum(.age)
}
```

### count(*) in GROUP BY

Count all rows per group including nulls:

```
User group .age { .age, count(*) }
```

### count(distinct) in GROUP BY

Count distinct values within each group:

```
Sale group .dept { .dept, count(distinct .item) }
```

### HAVING

Filter groups after aggregation:

```
User group .status having count(.name) > 5 { .status, n: count(.name) }
User group .age having count(*) > 1 { .age, count(*) }
```

### Filter + Group

Filter rows before grouping:

```
User filter .age >= 30 group .name { .name, n: count(.name) }
```

SQL equivalent:

```sql
SELECT name, COUNT(name) AS n FROM User WHERE age >= 30 GROUP BY name
```

### Qualified Group Keys and Aggregate Arguments

Over a join, group keys and aggregate arguments may be qualified with a source
alias (`alias.field`), exactly like projections and filters:

```
User as u join Order as o on u.id = o.user_id
  group u.status { u.status, orders: count(o.total), revenue: sum(o.total) }
```

A qualified group key is emitted as an `alias.field` output column, so a
qualified HAVING reference and downstream projections line up with it:

```
User as u join Order as o on u.id = o.user_id
  group u.status having u.status = "active" { u.status, n: count(*) }
```

Unqualified keys and arguments still work over a join as long as the bare name
is unambiguous. `group .status` resolves to `u.status` when only one joined
column ends in `.status`. A name that matches no column is an error, and a name
that matches more than one (for example `.id`, present on both sides) is an
ambiguity error that names the candidate columns; qualify the key to resolve it.

### Grouped Aggregates over Joins: fan-out semantics

A one-to-many join repeats each row of the "one" side once per matching row on
the "many" side. In v0.11, PowQL aggregates follow SQL-standard semantics: they
aggregate the joined (fanned-out) rows directly, so `sum` and `avg` over a
column from the "one" side double-count under fan-out.

Toy example. Three accounts in one tier, joined one-to-many to their orders:

```
type Account { required id: int, required tier: str, required balance: float }
type Ord     { required id: int, required account_id: int }

-- tier "gold" holds all three accounts
-- balances: A = 10.0, B = 10.0, C = 40.0    (true average balance = 20.0)
-- orders:   A has 4, B has 1, C has 1        (6 joined rows in total)

Account as a join Ord as o on a.id = o.account_id
  group a.tier { a.tier, avg_bal: avg(a.balance) }
```

Because account A appears in four joined rows, `avg(a.balance)` over the joined
rows in the "gold" group is `(10*4 + 10 + 40) / 6 = 15.0`, not the true `20.0`.
A real document-heavy CMS workload shows the same effect at scale: a true average of
`12.92` collapses to `8.67` once the join fans out the rows.

Until symmetric aggregates ship (see below), use `count(distinct ...)` as the
fan-out-safe count. It counts distinct values within each group regardless of
how many times the join repeats them:

```
-- distinct accounts per group, unaffected by order counts
Account as a join Ord as o on a.id = o.account_id
  group a.balance { a.balance, accounts: count(distinct a.id) }
```

Note: correct-by-default (symmetric) aggregates over joins, where an aggregate
of a column from source `A` aggregates the distinct rows of `A` and fan-out
cannot inflate it, ship in v0.13 with a `raw` opt-out that restores the
SQL-standard behavior above. The SQL frontend keeps SQL semantics. See the
document-store design doc (section 5.2) for details.

---

## Joins

PowQL supports inner, left outer, right outer, and cross joins. Aliases are used to disambiguate fields from different tables.

### Example Schemas

The join examples below assume these table definitions:

```
type User { required id: int, required name: str, required email: str, age: int }
type Order { required id: int, required user_id: int, required total: float, product_id: int }
type Product { required id: int, required name: str, price: float }
```

Every column must be explicitly defined — there are no hidden/implicit columns. An id is still its own declared column, but an integer column marked `auto` (see the `auto` modifier above) is assigned from a per-table sequence when omitted, so callers don't have to generate ids themselves.

### Syntax

```
<Table1> as <alias1> [inner|left|right|cross] join <Table2> as <alias2> on <expr>
```

### Inner Join

Returns only rows that match in both tables. `join` without a modifier defaults to inner:

```
User as u join Order as o on u.id = o.user_id
User as u inner join Order as o on u.id = o.user_id
```

### Left Outer Join

Returns all rows from the left table. Unmatched right-side columns are null:

```
User as u left join Order as o on u.id = o.user_id
User as u left outer join Order as o on u.id = o.user_id
```

### Right Outer Join

Returns all rows from the right table. Unmatched left-side columns are null:

```
User as u right join Order as o on u.id = o.user_id
```

### Cross Join

Produces the Cartesian product of both tables. No `on` clause:

```
User as u cross join Product as p
```

### Qualified Field References

In join queries, reference fields with the alias prefix:

```
User as u join Order as o on u.id = o.user_id { u.name, o.total }
```

### Filter and Projection on Joins

Joins compose with the full query pipeline:

```
User as u join Order as o on u.id = o.user_id
  filter o.total > 75 { u.name, o.total }
```

### Multi-Table Joins

Chain multiple joins left to right:

```
User as u join Order as o on u.id = o.user_id
  join Product as p on o.product_id = p.id
```

```
User as u join Order as o on u.id = o.user_id
  cross join Product as p
```

### Hash Join vs Nested Loop

PowQL automatically selects the best join strategy:

- **Hash join** (O(L + R)) -- used for equi-joins (`a.col = b.col`)
- **Nested loop** (O(L x R)) -- fallback for non-equi predicates or cross joins

No hint syntax is needed; the engine detects the optimal path.

---

## Set Operations

### UNION

Combine results from two queries, removing duplicates:

```
User filter .dept = "eng" union User filter .dept = "sales"
A union B
```

### UNION ALL

Combine results keeping all duplicates:

```
X union all Y
```

### Chaining

UNION is left-associative and can be chained:

```
T1 union T2 union T3
```

### With Filters

Each side of a UNION can have its own filter/projection pipeline:

```
User filter .age > 50 union User filter .status = "vip"
```

---

## Subqueries

### IN Subquery

Filter rows where a field's value exists in the result of another query:

```
User filter .name in (VIP { .name })
User filter .name in (VIP filter .active = true { .name })
```

### NOT IN Subquery

Exclude rows where a field's value exists in another query's result:

```
User filter .name not in (VIP { .name })
User filter .id not in (Order { .user_id })
```

### Subquery with Filter

The subquery can include its own pipeline:

```
User filter .name in (Score filter .points > 70 { .name }) { .name }
```

SQL equivalent:

```sql
SELECT name FROM User WHERE name IN (SELECT name FROM Score WHERE points > 70)
```

### EXISTS / NOT EXISTS

Check whether a subquery returns any rows:

```
User filter exists (Order filter .user_id = 1)
User filter not exists (Order filter .status = "pending")
```

`exists` evaluates to true if the inner query matches at least one row. `not exists` is the negation.

---

## Functions

### Scalar Functions

Scalar functions operate on individual values and can be used in projections and filters.

#### upper / lower

Convert string to upper or lower case:

```
User filter upper(.name) = "ALICE"
User { low: lower(.email) }
```

#### length

Return the character length of a string:

```
User { .name, len: length(.name) }
```

#### trim

Remove leading and trailing whitespace:

```
User { clean: trim(.name) }
```

#### substring

Extract a substring. Arguments: `(expr, start, length)` -- 1-indexed:

```
User { sub: substring(.name, 1, 3) }
-- Alice -> "Ali", Bob -> "Bob", Charlie -> "Cha"
```

#### concat

Concatenate multiple values. Non-string types are coerced to strings:

```
User { full: concat(.name, " - ", .email) }
-- "Alice - alice@example.com"

User { info: concat(.name, " age=", .age) }
-- "Alice age=30"
```

#### json_type

Return the JSON type of a value extracted from a `json` column as one of
`'null'`, `'string'`, `'number'`, `'bool'`, `'object'`, or `'array'`. A path
that is missing (or extracts nothing) returns the empty set. This is the way
to distinguish a JSON `null` from a missing key, since `->` scalarizes both to
the empty set:

```
Post { kind: json_type(.data->author) }
-- "object" when author is present, empty when the key is absent

Post filter json_type(.data->tags) = "array"
```

See [JSON Documents](#json-documents) for the full `->` extraction rules.

### Math Functions

#### abs

Return the absolute value of a number:

```
User { .name, abs_score: abs(.score) }
```

#### round

Round a float to the nearest integer (or to N decimal places with a second argument):

```
User { .name, rounded: round(.score) }
User { .name, rounded: round(.score, 2) }
```

#### ceil / floor

Round up or down to the nearest integer:

```
User { .name, up: ceil(.score), down: floor(.score) }
```

#### sqrt

Return the square root:

```
User { .name, root: sqrt(.score) }
```

#### pow

Raise a value to a power:

```
User { .name, squared: pow(.score, 2) }
```

### Date/Time Functions

#### now

Return the current timestamp, for use in filters, projections, and update assignments:

```
Event filter .ts < now() { .name }
Event { .name, checked_at: now() }
Event filter .name = "login" update { ts := now() }
```

`now()` is a runtime function, so it can only appear where expressions are
evaluated (filters, projections, `having`, `update` assignments). **Insert**
assignments accept literal values only — `insert Event { ts := now() }` fails
with `expected literal value`. A `datetime` column is stored as an integer
timestamp, so seed inserted rows with a literal like `ts := 1752000000` and
stamp them afterwards with `update { ts := now() }` if needed.

#### extract

Extract a component from a datetime value. Supported components: `year`, `month`, `day`, `hour`, `minute`, `second`:

```
Event { .name, yr: extract("year", .ts) }
Event filter extract("month", .created_at) = 6
```

#### date_add

Add a duration to a datetime value. Units: `years`, `months`, `days`, `hours`, `minutes`, `seconds`:

```
Event { .name, next_week: date_add(.ts, 7, "days") }
```

#### date_diff

Return the difference between two datetime values in the specified unit:

```
Event { .name, age_days: date_diff(.created_at, now(), "days") }
```

### CAST

Convert a value to a different type. The target type is given as a quoted
string after a comma: `cast(<expr>, "<type>")`.

```
User { .name, age_str: cast(.age, "str") }
User filter cast(.score, "int") > 50
```

Supported target types: `"int"`, `"float"`, `"str"`, `"bool"`.

### CASE WHEN

Conditional expression with multiple branches:

```
User {
  .name,
  label: case
    when .age > 30 then "senior"
    when .age >= 30 then "exactly30"
    else "young"
  end
}
```

CASE in a filter:

```
User filter case when .age > 30 then true else false end
```

CASE without ELSE returns null (Empty) when no branch matches:

```
User { .name, label: case when .age > 100 then "old" end }
-- all labels will be null since no one is over 100
```

---

## JSON Documents

A `json` column stores a whole JSON document -- an object, an array, or a
scalar -- as a single value. You insert JSON as a string literal; PowDB
validates it, rejects malformed input, and stores a canonical binary form.

```
type Post {
  required id: int,
  data: json
}

insert Post {
  id := 1,
  data := "{\"author\": {\"name\": \"Ada\"}, \"tags\": [\"db\", \"powql\"], \"views\": 12}"
}
```

### Path extraction with `->`

The `->` operator walks into a JSON document by object key or array index. It
binds tighter than any other operator, so `.data->author->name = "Ada"`
extracts first and compares second. A key that is not a bare identifier can be
written as a string:

```
Post { author: .data->author->name }        -- object key
Post { first_tag: .data->tags->0 }           -- array index (0-based)
Post { weird: .data->'has spaces!' }         -- string-form key
Post filter .data->views > 10                -- extract, then compare
```

`->` extracts and scalarizes the value it lands on:

| JSON value at the path | PowQL value |
|---|---|
| string | `str` |
| integral number | `int` |
| non-integral number | `float` |
| `true` / `false` | `bool` |
| object or array | `json` (a sub-document) |
| JSON `null` | empty set |
| missing key or index | empty set |

Because both JSON `null` and a missing path scalarize to the empty set, use
[`json_type`](#json_type) when you need to tell them apart. There is no
implicit cross-type coercion: `.data->views > 10` compares whatever the
extraction yields under the normal PowQL value rules; use `cast` for stringly
numbers.

### Canonicalization semantics (important)

PowDB stores JSON in a canonical binary form, not as the text you typed. This
has user-visible consequences you must know:

- **Object key order is not preserved.** Keys are sorted bytewise on write, so
  `{"b":2,"a":1}` reads back as `{"a":1,"b":2}`. Do not depend on insertion
  order (this matches PostgreSQL's `jsonb`).
- **Duplicate keys are de-duplicated, last value wins.** `{"a":1,"a":2}`
  becomes `{"a":2}`.
- **Equal documents have equal bytes.** Two documents that differ only in key
  order or whitespace are equal, group together, and compare equal.
- **Numbers keep their int/float distinction** from the input text. Floating
  point values are IEEE 754 `f64`, with the usual precision limits.
- **Limits.** A single JSON value may not exceed 64MB, and nesting may not
  exceed a depth of 128 levels. Exceeding either is a typed error on insert.
- Invalid JSON or invalid UTF-8 is rejected on insert with a typed error; the
  value is never stored.

### Worked example

```
type Post { required id: int, data: json }

insert Post { id := 1, data := "{\"tags\":[\"db\"],\"author\":{\"name\":\"Ada\"},\"views\":12}" }
insert Post { id := 2, data := "{\"author\":{\"name\":\"Grace\"},\"views\":3}" }

-- Extract nested fields. Note the canonical (sorted-key) output.
Post { .id, author: .data->author->name, views: .data->views }
-- 1, "Ada",   12
-- 2, "Grace", 3

-- Filter on an extracted scalar.
Post filter .data->views > 10 { .id }
-- 1

-- Distinguish a missing key from a present one.
Post { .id, has_tags: json_type(.data->tags) }
-- 1, "array"
-- 2, (empty)   -- post 2 has no "tags" key

-- Extract a sub-document (object/array come back as json text).
Post filter .id = 1 { sub: .data->author }
-- {"name":"Ada"}
```

---

## Mutations

### INSERT

Insert a single row. Fields are assigned with `:=`:

```
insert User { name := "Alice", email := "alice@example.com", age := 30 }
insert User { name := "Bob", email := "bob@example.com" }
```

Omitted fields take their column's `default` if one is declared, otherwise null. Required fields must be provided unless they declare a default.

**Multi-row insert.** Separate row blocks with commas to insert many rows in a
single statement. Each block is independent and may set a different subset of
columns:

```
insert User
  { name := "Alice", email := "alice@example.com", age := 30 },
  { name := "Bob",   email := "bob@example.com" },
  { name := "Carol", email := "carol@example.com", age := 41 }
```

A multi-row insert is **one statement = one WAL fsync** (vs one fsync per
single-row autocommit statement), so it's the fastest durable way to bulk-load
— and over a network connection it's **one round trip** instead of N. It's also
**all-or-nothing on validation**: if any row is invalid (missing a required
field, unknown column, bad type), the whole statement fails and *no* rows are
inserted. The result reports the number of rows inserted. (A mid-write *storage*
failure — e.g. the disk filling between rows — is the one exception and can
leave earlier rows written; wrap the insert in a transaction if you need a hard
rollback boundary.) The whole batch is also charged against
`POWDB_QUERY_MEMORY_LIMIT`, so an over-large batch errors rather than exhausting
memory.

**`returning`.** End an insert with `returning` to get the inserted row(s) back
(all columns) as a result set instead of a modified-count — so you don't need a
follow-up `SELECT` to read the row you just wrote:

```
insert User { name := "Alice", email := "alice@example.com", age := 30 } returning
```

This works for single- and multi-row inserts and returns over the same rows
path as a query, so a client gets the written rows in the same round trip.

### UPDATE

Update rows matching an optional filter. Supports both literal values and expressions:

```
-- Set a literal value
User filter .name = "Alice" update { age := 31 }

-- Update with an expression referencing the current row
User filter .name = "Alice" update { age := .age + 5 }

-- Update all rows
User update { age := .age * 2 }

-- Arithmetic in update
User filter .age > 28 update { age := .age + 1 }
```

End an update with `returning` to get the **post-update** row(s) back (all
columns) as a result set instead of a modified-count:

```
User filter .name = "Alice" update { age := 31 } returning
```

### DELETE

Delete rows matching an optional filter:

```
User filter .name = "Bob" delete
User filter .age < 18 delete
User filter .age > 60 delete
```

Delete all rows (use with care):

```
User delete
```

End a delete with `returning` to get the **pre-delete** row(s) back (all
columns) as a result set instead of a modified-count — useful for archiving or
auditing what you removed in the same round trip:

```
User filter .age < 18 delete returning
```

---

## Transactions

PowDB supports explicit transactions with `begin`, `commit`, and `rollback`. Statements executed between `begin` and `commit` are applied atomically -- either all succeed or none do. Use `rollback` to discard uncommitted changes.

### Syntax

```
begin
<statement1>
<statement2>
...
commit    -- apply all changes
```

```
begin
<statement1>
<statement2>
...
rollback  -- discard all changes
```

### Examples

Insert multiple rows atomically:

```
begin
insert User { name := "Alice", email := "alice@example.com", age := 30 }
insert User { name := "Bob", email := "bob@example.com", age := 25 }
commit
```

Roll back a change before it takes effect:

```
begin
insert User { name := "Charlie", email := "charlie@example.com", age := 40 }
rollback
-- Charlie is not inserted
```

Mix reads and writes inside a transaction:

```
begin
insert Order { user_id := 1, total := 99.95 }
User filter .id = 1 update { order_count := .order_count + 1 }
commit
```

### Notes

- If a connection closes before `commit`, uncommitted changes are discarded (implicit rollback).
- Transactions are per-connection. Other connections do not see uncommitted rows.
- Nesting transactions is not supported -- calling `begin` inside an open transaction is an error.

### Transactions and write throughput

By default PowDB runs in `WalSyncMode::Full`: every autocommit statement fsyncs the write-ahead log before returning, so each write is durable on its own. That fsync is the bottleneck for single-row writes -- on real disks, autocommit inserts top out around a few hundred rows per second.

Inside a transaction, the fsync is deferred to `commit`. All statements between `begin` and `commit` share one fsync, so wrapping a bulk load in a transaction is dramatically faster while staying fully durable:

```
begin
insert User { name := "u1", email := "u1@ex.com", age := 20 }
insert User { name := "u2", email := "u2@ex.com", age := 21 }
-- ... thousands more ...
commit
```

In internal benchmarks, batching inserts this way ran roughly 50x faster than the same inserts in autocommit, with identical durability guarantees. Always wrap bulk loads in a transaction.

---

## DDL

### CREATE TABLE (type)

Create a new table. See [Schema Definition](#schema-definition):

```
type User {
  required name: str,
  required email: str,
  age: int
}
```

Re-declaring an existing type is an error (`type 'User' already exists`). Add
`if not exists` after the type name to make it a no-op instead — useful for
idempotent migrations:

```
type User if not exists {
  required name: str,
  age: int
}
```

`if not exists` never redefines an existing type; the original schema is left
untouched.

### ALTER TABLE

Add or drop columns on an existing table.

#### Add Column

```
alter User add column status: str
alter User add required active: bool       -- only on an empty table (see note)
alter User add status: str                 -- "column" keyword is optional
```

> A `required` column can only be added to an **empty** table — there is no
> default clause to backfill existing rows, so on a non-empty table it fails with
> `cannot add required column '…' to non-empty table '…': no default value to
> backfill existing rows with`. Add the column nullable, populate it, then tighten
> the constraint if needed.

#### Drop Column

```
alter User drop column email
alter User drop email                      -- "column" keyword is optional
alter User drop column if exists email     -- no-op if the column is absent
```

Dropping a column that does not exist is an error unless you add `if exists`,
in which case it is a clean no-op.

#### Add Index

Create a B+tree index on a column. Point lookups and range scans use indexes automatically — no query hints:

```
alter User add index .email
alter User add index .age
alter User add index if not exists .email  -- accepted for symmetry
```

Indexes are persistent (BIDX format in the data directory) and survive restart. Re-running `add index` on an existing index is already a no-op, so `if not exists` is accepted but does not change behavior.

#### Add Unique

Create a unique B+tree index on a column, enforcing that no two non-null rows share a value:

```
alter User add unique .email
alter User add unique if not exists .email  -- no-op if already indexed
```

The command first scans the existing data — if any duplicate (non-null) value is already present, it fails and the index is not created. Without `if not exists` it also fails if the column already has an index, since there is no in-place index upgrade (drop and recreate the table to change an existing index's uniqueness); with `if not exists` an already-indexed column is a no-op. Once created, the constraint is enforced on every subsequent insert/update/upsert and survives restart.

### DROP TABLE

Remove a table entirely:

```
drop User
drop if exists User                        -- no-op if the type is absent
```

Dropping a type that does not exist is an error unless you add `if exists`.

---

## Introspection

Discover what exists in the database without any protocol extensions — the
commands below return ordinary result rows, so any client consumes them like a
normal query.

### schema

List every type (table). One row per type: its name and its column count.

```
schema
```

| name | columns |
|------|---------|
| User | 3       |
| Post | 2       |

### describe

Describe one type: its columns with type and nullability, plus which columns are
indexed. `describe <Type>` and `schema <Type>` are equivalent.

```
describe User
schema User        -- alias for `describe User`
```

| column | type | nullable | index  |
|--------|------|----------|--------|
| id     | int  | false    | unique |
| name   | str  | false    |        |
| email  | str  | true     | index  |

- **nullable** is `false` for `required` columns, `true` otherwise.
- **index** is `unique` for a unique index, `index` for a plain index, empty
  when the column is not indexed.

Describing a type that does not exist is an error (`table 'Ghost' not found`).
Introspection always reflects the **current** schema — it is never served from a
stale cached plan.

---

## Reserved Words and Quoting

PowQL keywords cannot be used **bare** as type or column names. For example,
`type Post { type: str }` fails with:

```
syntax error: 'type' is a reserved word and cannot be used as a field name;
rename it or quote it as `type`
```

> **Breaking change (v0.10):** `schema` and `describe` are now keywords.
> A lowercase bare identifier named `schema` or `describe` that parsed in
> earlier releases must now be backtick-quoted (`` `schema` ``). Keyword
> matching is case-sensitive, so capitalized names like `type Schema { … }`
> are unaffected.

To use a reserved word as an identifier anyway, wrap it in **backticks**. A
backtick-quoted identifier is always a plain name, never a keyword, and works
everywhere an identifier is accepted — DDL field lists, `insert`/`update`/
`upsert` assignments, filters, projections, ordering, and index DDL:

```
type Post { required `type`: str, `order`: int }
insert Post { `type` := "news", `order` := 1 }
Post filter .`type` = "news" { .`type`, .`order` }
Post order .`order` asc
alter Post add index .`order`
```

Backticks may also contain characters that are not otherwise legal in an
identifier, such as spaces: `` `full name` ``.

> In filter/projection/order positions, a plain dotted reference like `.type`
> also works, because dotted field references bypass keyword lookup. Backtick
> quoting is required in bare-identifier positions (DDL field names, assignment
> targets) and is accepted everywhere for consistency.

### Complete keyword list

The following words are reserved (derived from the lexer's keyword table). This
includes the boolean literal words `true` and `false`:

```
abs, add, alter, and, as, asc, auto, avg, begin, between, case, cast, ceil,
column, commit, concat, conflict, count, cross, date_add, date_diff, default,
delete, dense_rank, desc, describe, distinct, drop, else, end, exists, explain,
extract, false, filter, floor, group, having, in, index, inner, insert, is,
join, left, length, let, like, limit, link, lower, match, materialize,
materialized, max, min, multi, not, now, null, offset, on, or, order, outer,
over, partition, pow, rank, refresh, required, returning, right, rollback,
round, row_number, schema, select, sqrt, substring, sum, then, transaction,
trim, true, type, union, unique, update, upper, upsert, view, when
```

---

## Materialized Views

Materialized views store the result of a query as a physical table. PowDB automatically refreshes views when underlying data changes.

### Create

Define a view with `materialize ... as`:

```
materialize OldUsers as User filter .age > 28
materialize UserNames as User { .name }
materialize ActiveUsers as User filter .status = "active" { .name, .email }
```

### Query

Query a materialized view exactly like a table:

```
OldUsers
OldUsers filter .name = "Alice"
count(OldUsers)
```

### Auto-Refresh

When the underlying table changes (insert, update, or delete), PowDB marks dependent views as dirty. The next time you query a dirty view, it is automatically refreshed before returning results. No stale reads.

### Manual Refresh

Force a refresh explicitly:

```
refresh OldUsers
```

### Drop

Remove a materialized view:

```
drop view OldUsers
drop view if exists OldUsers               -- no-op if the view is absent
```

Note: `drop view` removes the view. Plain `drop` (without `view`) drops a table. As with `drop table`, `if exists` turns a missing view into a no-op instead of an error.

---

## Window Functions

Window functions compute a value for each row based on a window of rows. They do not reduce the number of rows.

### Syntax

```
<Table> { .field, <func> over (partition .key order .sort_key) }
```

### ROW_NUMBER

Assign a sequential number to each row within its partition:

```
User { .name, .dept, rn: row_number() over (partition .dept order .age) }
```

### RANK / DENSE_RANK

Assign a rank based on the ORDER BY expression. `RANK` leaves gaps for ties; `DENSE_RANK` does not:

```
User { .name, r: rank() over (order .score desc) }
User { .name, dr: dense_rank() over (partition .dept order .score desc) }
```

### Aggregate Windows

SUM, AVG, MIN, MAX can be used as window functions:

```
User { .name, .salary, dept_avg: avg(.salary) over (partition .dept) }
User { .name, running_total: sum(.amount) over (order .date) }
```

---

## UPSERT

Insert a row or update it if a conflict occurs on a specified key column.

### Syntax

```
upsert <Table> on .<key_column> { <assignments> } [on conflict { <conflict_assignments> }]
```

The key column (specified after `on`) is used to detect conflicts. If a row with a matching key already exists, the row is updated with the provided assignments (or the conflict-specific assignments if `on conflict` is given). If no match exists, a new row is inserted.

> **Breaking change (since 0.4.7):** the `on` column must be **unique** — declare it with the `unique` modifier (`unique email: str`) or `alter <Table> add unique .<col>`. Upserting on a non-unique column is rejected with an error. This closes a prior bug where `upsert` on a non-unique column could silently create duplicate-key rows.

### Examples

These examples assume `email` is declared `unique` (`unique email: str`) — the
`on` column must be unique, per the note above, or the upsert is rejected.

Basic upsert (insert or replace all fields on conflict):

```
upsert User on .email { name := "Alice", email := "alice@example.com", age := 30 }
```

Upsert with explicit conflict handling (only update specific fields on conflict):

```
upsert User on .email { name := "Alice", email := "alice@example.com", age := 30 }
  on conflict { age := 30 }
```

If a row with `email = "alice@example.com"` already exists, only the `age` field is updated instead of replacing the entire row.

---

## EXPLAIN

Inspect the query plan without executing it:

```
explain User filter .age > 25 order .age desc limit 10 { .name, .age }
```

Returns the plan tree that the executor would run. Useful for understanding whether indexes are being used and how the engine structures a query.

---

## Prepared Queries

PowDB supports prepared queries for high-performance repeated operations. The query is parsed and planned once, then executed repeatedly with different literal values.

### How It Works

1. **Prepare** -- Parse and plan the query once. The engine counts the literal slots.
2. **Execute** -- Supply new literal values for each execution. The engine substitutes them into the cached plan.

Prepared queries skip the lexer, parser, planner, and plan-cache lookup on every execution after the first. This is PowDB's equivalent of SQLite's `prepare_cached`.

### Literal Slot Order

Literals are substituted in the order they appear in the source query, left to right. For example:

```
insert User { name := "seed", email := "seed@ex.com", age := 0 }
-- 3 literal slots: [0] = name, [1] = email, [2] = age
```

```
User filter .name = "seed" update { age := 0 }
-- 2 literal slots: [0] = filter value, [1] = assignment value
```

### Fast Paths

PowDB detects common prepared-query shapes and optimizes them:

- **Insert fast path** -- When all assignment values are plain literals, column indices are resolved once at prepare time. Each execution builds the row directly from the literal slice with zero plan cloning.
- **Point update fast path** -- When the query is `T filter .pk = ? update { col := ? }` with an indexed primary key and a fixed-size target column, the engine performs a single B-tree lookup and patches raw bytes in place. No plan clone, no allocations.

### API Usage (Rust)

```rust
let prep = engine.prepare(
    r#"insert User { name := "x", email := "x@e.com", age := 0 }"#
)?;

for i in 0..1000 {
    engine.execute_prepared(&prep, &[
        Literal::String(format!("user{i}")),
        Literal::String(format!("u{i}@ex.com")),
        Literal::Int(20 + i),
    ])?;
}
```

---

## Type System

PowQL has eight data types plus a null representation.

| Type | PowQL Name | Rust Mapping | Size | Description |
|---|---|---|---|---|
| Integer | `int` | `i64` | 8 bytes | 64-bit signed integer |
| Float | `float` | `f64` | 8 bytes | IEEE 754 double precision |
| Boolean | `bool` | `bool` | 1 byte | `true` or `false` |
| String | `str` | `String` | Variable | UTF-8 text |
| DateTime | `datetime` | `i64` (epoch) | 8 bytes | Unix timestamp |
| UUID | `uuid` | `[u8; 16]` | 16 bytes | 128-bit identifier |
| Bytes | `bytes` | `Vec<u8>` | Variable | Raw binary data |
| JSON | `json` | canonical binary | Variable | JSON document; see [JSON Documents](#json-documents) |
| Null | (empty) | `Value::Empty` | 0 bytes | Absence of a value |

### Nullability

- Fields marked `required` in the schema cannot be null.
- All other fields are nullable by default.
- Use `is null` / `is not null` to check for null values.
- The `??` coalesce operator provides a fallback for null values.
- Aggregate functions skip null values (except `count(*)` which counts all rows).

### Type Coercion

- `concat` coerces all arguments to strings: `concat(.name, " age=", .age)` produces `"Alice age=30"`.
- Arithmetic on mixed int/float promotes to float.
- Comparisons between incompatible types evaluate to false.

---

## PowQL vs SQL Cheat Sheet

| Operation | PowQL | SQL |
|---|---|---|
| **Select all** | `User` | `SELECT * FROM User` |
| **Select columns** | `User { .name, .age }` | `SELECT name, age FROM User` |
| **Alias** | `User { full_name: .name }` | `SELECT name AS full_name FROM User` |
| **Where** | `User filter .age > 30` | `SELECT * FROM User WHERE age > 30` |
| **AND / OR** | `User filter .a > 1 and .b < 5` | `... WHERE a > 1 AND b < 5` |
| **Order** | `User order .age desc` | `... ORDER BY age DESC` |
| **Multi-sort** | `User order .age asc, .name desc` | `... ORDER BY age ASC, name DESC` |
| **Limit** | `User limit 10` | `... LIMIT 10` |
| **Offset** | `User offset 20 limit 10` | `... LIMIT 10 OFFSET 20` |
| **Distinct** | `User distinct { .name }` | `SELECT DISTINCT name FROM User` |
| **Count** | `count(User)` | `SELECT COUNT(*) FROM User` |
| **Count where** | `count(User filter .age > 30)` | `SELECT COUNT(*) FROM User WHERE age > 30` |
| **Count distinct** | `count(distinct User { .name })` | `SELECT COUNT(DISTINCT name) FROM User` |
| **Sum** | `sum(User { .age })` | `SELECT SUM(age) FROM User` |
| **Avg** | `avg(User { .age })` | `SELECT AVG(age) FROM User` |
| **Min / Max** | `min(User { .age })` | `SELECT MIN(age) FROM User` |
| **Group By** | `User group .status { .status, count(.name) }` | `SELECT status, COUNT(name) FROM User GROUP BY status` |
| **Having** | `User group .status having count(.name) > 5 { .status }` | `... GROUP BY status HAVING COUNT(name) > 5` |
| **Inner Join** | `User as u join Order as o on u.id = o.user_id` | `SELECT * FROM User u JOIN Order o ON u.id = o.user_id` |
| **Left Join** | `User as u left join Order as o on u.id = o.user_id` | `... LEFT JOIN Order o ON u.id = o.user_id` |
| **Right Join** | `User as u right join Order as o on u.id = o.user_id` | `... RIGHT JOIN Order o ON u.id = o.user_id` |
| **Cross Join** | `User as u cross join Product as p` | `... CROSS JOIN Product p` |
| **IN list** | `User filter .age in (25, 30)` | `... WHERE age IN (25, 30)` |
| **NOT IN** | `User filter .name not in ("Alice")` | `... WHERE name NOT IN ('Alice')` |
| **IN subquery** | `User filter .name in (VIP { .name })` | `... WHERE name IN (SELECT name FROM VIP)` |
| **BETWEEN** | `User filter .age between 20 and 30` | `... WHERE age BETWEEN 20 AND 30` |
| **LIKE** | `User filter .name like "A%"` | `... WHERE name LIKE 'A%'` |
| **IS NULL** | `User filter .age is null` | `... WHERE age IS NULL` |
| **IS NOT NULL** | `User filter .age is not null` | `... WHERE age IS NOT NULL` |
| **Coalesce** | `.age ?? 0` | `COALESCE(age, 0)` |
| **CASE WHEN** | `case when .age > 30 then "old" else "young" end` | `CASE WHEN age > 30 THEN 'old' ELSE 'young' END` |
| **UNION** | `A union B` | `A UNION B` |
| **UNION ALL** | `A union all B` | `A UNION ALL B` |
| **Insert** | `insert User { name := "Alice", age := 30 }` | `INSERT INTO User (name, age) VALUES ('Alice', 30)` |
| **Update** | `User filter .id = 1 update { age := 31 }` | `UPDATE User SET age = 31 WHERE id = 1` |
| **Update expr** | `User update { age := .age + 1 }` | `UPDATE User SET age = age + 1` |
| **Delete** | `User filter .id = 1 delete` | `DELETE FROM User WHERE id = 1` |
| **Create table** | `type User { required name: str }` | `CREATE TABLE User (name TEXT NOT NULL)` |
| **Drop table** | `drop User` | `DROP TABLE User` |
| **Alter add** | `alter User add column status: str` | `ALTER TABLE User ADD COLUMN status TEXT` |
| **Alter drop** | `alter User drop column status` | `ALTER TABLE User DROP COLUMN status` |
| **Create index** | `alter User add index .email` | `CREATE INDEX ON User (email)` |
| **Unique column** | `type User { unique email: str }` | `CREATE TABLE User (email TEXT UNIQUE)` |
| **Add unique** | `alter User add unique .email` | `CREATE UNIQUE INDEX ON User (email)` |
| **Create view** | `materialize V as User filter .active = true` | `CREATE MATERIALIZED VIEW V AS SELECT * FROM User WHERE active` |
| **Refresh view** | `refresh V` | `REFRESH MATERIALIZED VIEW V` |
| **Drop view** | `drop view V` | `DROP MATERIALIZED VIEW V` |
| **Upper** | `upper(.name)` | `UPPER(name)` |
| **Lower** | `lower(.email)` | `LOWER(email)` |
| **Length** | `length(.name)` | `LENGTH(name)` |
| **Trim** | `trim(.name)` | `TRIM(name)` |
| **Substring** | `substring(.name, 1, 3)` | `SUBSTRING(name, 1, 3)` |
| **Concat** | `concat(.name, " ", .email)` | `CONCAT(name, ' ', email)` |

### Key Syntactic Differences

| Concept | PowQL | SQL |
|---|---|---|
| Field reference | `.field` (dot prefix) | `field` (bare identifier) |
| Assignment | `:=` | `=` or `SET col = val` |
| Table definition | `type Name { ... }` | `CREATE TABLE Name (...)` |
| Required/NOT NULL | `required field: type` | `field TYPE NOT NULL` |
| Unique constraint | `unique field: type` | `field TYPE UNIQUE` |
| String literals | `"double quotes"` | `'single quotes'` |
| Query shape | Pipeline: `Table verb verb { proj }` | Clausal: `SELECT proj FROM Table WHERE ... ORDER BY ...` |
| Aggregates | Wrapping: `count(Table filter ...)` | Inline: `SELECT COUNT(*) FROM Table WHERE ...` |
| Materialized views | `materialize Name as Query` | `CREATE MATERIALIZED VIEW Name AS Query` |
