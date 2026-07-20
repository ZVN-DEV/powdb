#!/usr/bin/env bash
#
# nested-results: a runnable demo of PowQL nested projections (shaped results).
# A User/Order/Item dataset is seeded into an embedded database, then queried
# with nested projections: one row per parent, with the matching children
# assembled into a native JSON array inside that row. No join fan-out, no
# client-side regrouping, and childless parents get [] instead of vanishing.
#
# See docs/POWQL.md, "Nested Projections (Shaped Results)".
#
# The script is self-contained: it builds powdb-cli if needed, runs every
# query through the embedded CLI, prints PASS/FAIL for each checked result,
# and exits nonzero if any check fails.
#
# Usage:
#   ./run.sh                 # build (if needed) and run the demo
#   POWDB_BIN=/path ./run.sh # use a prebuilt powdb-cli from that directory
#
set -euo pipefail

# ── Paths ────────────────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
WORK="$SCRIPT_DIR/.work"          # gitignored scratch; wiped on each run

# ── Pretty output + pass/fail bookkeeping ────────────────────────────────────
FAILURES=0
pass()  { printf 'PASS: %s\n' "$1"; }
fail()  { printf 'FAIL: %s\n' "$1"; FAILURES=$((FAILURES + 1)); }
step()  { printf '\n=== %s ===\n' "$1"; }

# ── Binary ───────────────────────────────────────────────────────────────────
if [ -n "${POWDB_BIN:-}" ]; then
  BIN="$POWDB_BIN"
else
  step "Building powdb-cli (release)"
  cargo build --release -p powdb-cli --manifest-path "$REPO_ROOT/Cargo.toml"
  BIN="$REPO_ROOT/target/release"
fi
PC="$BIN/powdb-cli"
if [ ! -x "$PC" ]; then
  echo "error: missing binary: $PC" >&2
  exit 1
fi

# ── Fresh workspace ──────────────────────────────────────────────────────────
rm -rf "$WORK"
mkdir -p "$WORK"
DB="$WORK/db"

# Run a PowQL query against the embedded database and print the full table.
q() { "$PC" --data-dir "$DB" --exec "$1"; }

# Extract result row N (1-based) from a query's table output as a single
# whitespace-free string, e.g. `Alice|[{"total":9.5}]`. Row 1 of the table is
# on line 3 (after the header and separator lines).
row() { q "$2" | sed -n "$(( $1 + 2 ))p" | tr -d ' '; }

check_row() {  # check_row <label> <row#> <query> <expected>
  local label="$1" n="$2" query="$3" expected="$4" got
  got="$(row "$n" "$query")"
  if [ "$got" = "$expected" ]; then
    pass "$label"
  else
    fail "$label (expected '$expected', got '$got')"
  fi
}

# ── 1. Seed the dataset ──────────────────────────────────────────────────────
step "1. Seed User/Order/Item"
# Alice has two orders, Bob has one (with no product_id), Cara has none.
# Order 1 has items "a" and "b"; order 2 has item "c"; order 3 has none.
q '
type User { required id: int, required name: str, required email: str, age: int };
type Order { required id: int, required user_id: int, required total: float, product_id: int };
type Item { required id: int, required order_id: int, required sku: str };
insert User { id := 1, name := "Alice", email := "alice@example.com", age := 30 };
insert User { id := 2, name := "Bob", email := "bob@example.com", age := 25 };
insert User { id := 3, name := "Cara", email := "cara@example.com", age := 41 };
insert Order { id := 1, user_id := 1, total := 9.5, product_id := 101 };
insert Order { id := 2, user_id := 1, total := 20.25, product_id := 102 };
insert Order { id := 3, user_id := 2, total := 5.5 };
insert Item { id := 1, order_id := 1, sku := "a" };
insert Item { id := 2, order_id := 1, sku := "b" };
insert Item { id := 3, order_id := 2, sku := "c" }' >/dev/null
if [ "$(q 'count(User)')" = "3" ] && [ "$(q 'count(Order)')" = "3" ]; then
  pass "seeded 3 users, 3 orders, 3 items"
else
  fail "seed counts wrong"
fi

# ── 2. Basic nesting ─────────────────────────────────────────────────────────
step "2. Basic nesting: one row per user, orders as a JSON array"
Q='User as u { u.name, orders: Order as o filter o.user_id = u.id { o.total, o.product_id } }'
q "$Q"
check_row "Alice's two orders come back in one JSON array" 1 "$Q" \
  'Alice|[{"product_id":101,"total":9.5},{"product_id":102,"total":20.25}]'
check_row "Bob's null product_id maps to JSON null" 2 "$Q" \
  'Bob|[{"product_id":null,"total":5.5}]'
check_row "childless Cara gets [], not NULL, and keeps her row" 3 "$Q" \
  'Cara|[]'

# ── 3. Child conditions + per-parent order/limit ─────────────────────────────
step "3. Child condition + order + limit (top-N per parent)"
Q='User as u { u.name, orders: Order as o filter o.user_id = u.id and o.total > 10 order o.total desc limit 3 { o.total, o.product_id } }'
q "$Q"
check_row "Alice keeps only orders over 10, sorted desc" 1 "$Q" \
  'Alice|[{"product_id":102,"total":20.25}]'
check_row "Bob's only order (5.5) is filtered out: [] again" 2 "$Q" \
  'Bob|[]'

# limit 1 is per parent: every user keeps their single biggest order.
Q='User as u { u.name, orders: Order as o filter o.user_id = u.id order o.total desc limit 1 { o.total } }'
q "$Q"
check_row "per-parent limit 1 keeps Alice's biggest order" 1 "$Q" \
  'Alice|[{"total":20.25}]'
check_row "per-parent limit 1 does NOT leave Bob childless" 2 "$Q" \
  'Bob|[{"total":5.5}]'

# ── 4. Multi-level nesting ───────────────────────────────────────────────────
step "4. Two levels: users, their orders, each order's items"
Q='User as u { u.name, orders: Order as o filter o.user_id = u.id { o.total, items: Item as i filter i.order_id = o.id { i.sku } } }'
q "$Q"
check_row "orders nest their own items array" 1 "$Q" \
  'Alice|[{"items":[{"sku":"a"},{"sku":"b"}],"total":9.5},{"items":[{"sku":"c"}],"total":20.25}]'
check_row "an order with no items gets an empty inner array" 2 "$Q" \
  'Bob|[{"items":[],"total":5.5}]'

# ── Summary ──────────────────────────────────────────────────────────────────
step "Summary"
if [ "$FAILURES" -eq 0 ]; then
  echo "ALL CHECKS PASSED"
  exit 0
else
  echo "$FAILURES CHECK(S) FAILED"
  exit 1
fi
