# Nested results

A runnable, self-contained demo of **PowQL nested projections (shaped
results)**: ask for one row per parent with the matching children assembled
into a native JSON array inside that row, instead of a fanned-out join you
have to regroup client-side. For the full language reference see
[`docs/POWQL.md`, "Nested Projections (Shaped Results)"](../../docs/POWQL.md#nested-projections-shaped-results).

## Run it

```bash
./run.sh
```

The script builds `powdb-cli` (release), seeds a tiny `User`/`Order`/`Item`
dataset into an embedded database, runs each query, and prints `PASS`/`FAIL`
for every checked result, exiting nonzero if any check fails. All scratch data
lives in a gitignored `.work/` directory beside the script. To skip the build
and use a prebuilt binary:

```bash
POWDB_BIN=/path/to/target/release ./run.sh
```

## What it demonstrates

The dataset: Alice has two orders, Bob has one (with no `product_id`), Cara
has none.

**Basic nesting.** One output row per user; the `orders` field is a JSON array
of objects keyed by the child projection names:

```
User as u { u.name, orders: Order as o filter o.user_id = u.id { o.total, o.product_id } }
-- Alice, [{"product_id":101,"total":9.5},{"product_id":102,"total":20.25}]
-- Bob,   [{"product_id":null,"total":5.5}]
-- Cara,  []
```

Childless Cara gets `[]`, never NULL, and her row is never dropped. There is
no row explosion to undo.

**Child conditions + per-parent order/limit.** Extra `and` conditions filter
the children; `order`/`limit` apply to each parent's array independently
(top-N per parent, not N rows overall):

```
User as u {
  u.name,
  orders: Order as o filter o.user_id = u.id and o.total > 10 order o.total desc limit 3 { o.total, o.product_id }
}
```

**Multi-level nesting.** A nested block can contain its own nested blocks:

```
User as u {
  u.name,
  orders: Order as o filter o.user_id = u.id {
    o.total,
    items: Item as i filter i.order_id = o.id { i.sku }
  }
}
```

Nested projections are PowQL-only: the SQL frontend deliberately has no
equivalent (see [`docs/SQL.md`](../../docs/SQL.md)).
