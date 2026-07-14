/**
 * Tests for the typed-row coercion helpers (clients/ts/src/typed.ts).
 *
 * Pure-JS tests — no server required.
 *
 *     npx tsx test/typed.test.ts
 */

import { strict as assert } from "node:assert";
import {
  coerceRow,
  coerceRows,
  coerceValue,
  type TypedSchema,
} from "../src/typed.js";
import { PowDBError, isPowDBError } from "../src/errors.js";

let passed = 0;
let failed = 0;
const failures: string[] = [];

async function test(name: string, fn: () => void | Promise<void>) {
  try {
    await fn();
    passed++;
    console.log(`  ✓ ${name}`);
  } catch (err: any) {
    failed++;
    failures.push(`${name}: ${err.message}`);
    console.log(`  ✗ ${name}`);
    console.log(`    ${err.message}`);
  }
}

function assertTypeCoercionError(fn: () => unknown, column: string) {
  try {
    fn();
  } catch (err) {
    assert.ok(isPowDBError(err), `expected PowDBError, got ${err}`);
    assert.equal(err.code, "type_coercion_failed");
    assert.ok(err.message.includes(column), `message should mention column "${column}"`);
    return;
  }
  throw new Error("expected throw, got nothing");
}

async function main() {
  // ──────────────────────────────────────────────────────────
  console.log("\ncoerceValue — str (and undefined/passthrough)");
  // ──────────────────────────────────────────────────────────

  await test("undefined type returns raw string unchanged", () => {
    assert.equal(coerceValue("col", "hello", undefined), "hello");
  });

  await test("str type returns raw string unchanged", () => {
    assert.equal(coerceValue("col", "hello", "str"), "hello");
  });

  await test("str preserves empty string (NOT treated as null)", () => {
    // Empty string is a valid str value; only the literal "null" token is null.
    assert.equal(coerceValue("col", "", "str"), "");
  });

  await test("str preserves the literal \"null\" token as a string", () => {
    // For str columns, "null" stays as the 4-char string (ambiguous but explicit).
    assert.equal(coerceValue("col", "null", "str"), "null");
  });

  // ──────────────────────────────────────────────────────────
  console.log("\ncoerceValue — int");
  // ──────────────────────────────────────────────────────────

  await test("small positive int returns number", () => {
    assert.equal(coerceValue("age", "42", "int"), 42);
    assert.equal(typeof coerceValue("age", "42", "int"), "number");
  });

  await test("negative int returns number", () => {
    assert.equal(coerceValue("age", "-7", "int"), -7);
  });

  await test("zero int returns number", () => {
    assert.equal(coerceValue("age", "0", "int"), 0);
  });

  await test("int at Number.MAX_SAFE_INTEGER stays a number", () => {
    const v = coerceValue("n", "9007199254740991", "int");
    assert.equal(v, Number.MAX_SAFE_INTEGER);
    assert.equal(typeof v, "number");
  });

  await test("int above Number.MAX_SAFE_INTEGER promotes to BigInt", () => {
    const v = coerceValue("n", "9007199254740993", "int");
    assert.equal(typeof v, "bigint");
    assert.equal(v, 9007199254740993n);
  });

  await test("huge negative int promotes to BigInt", () => {
    const v = coerceValue("n", "-12345678901234567890", "int");
    assert.equal(typeof v, "bigint");
    assert.equal(v, -12345678901234567890n);
  });

  await test("int with trailing non-digits throws type_coercion_failed", () => {
    assertTypeCoercionError(() => coerceValue("age", "42abc", "int"), "age");
  });

  await test("int with decimal point throws", () => {
    assertTypeCoercionError(() => coerceValue("age", "3.14", "int"), "age");
  });

  await test("int with whitespace throws", () => {
    assertTypeCoercionError(() => coerceValue("age", " 42", "int"), "age");
  });

  await test("empty string on int throws", () => {
    assertTypeCoercionError(() => coerceValue("age", "", "int"), "age");
  });

  // ──────────────────────────────────────────────────────────
  console.log("\ncoerceValue — float");
  // ──────────────────────────────────────────────────────────

  await test("float parses 3.14", () => {
    assert.equal(coerceValue("x", "3.14", "float"), 3.14);
  });

  await test("float parses integer-looking value", () => {
    assert.equal(coerceValue("x", "42", "float"), 42);
    assert.equal(typeof coerceValue("x", "42", "float"), "number");
  });

  await test("float parses negative", () => {
    assert.equal(coerceValue("x", "-1.5", "float"), -1.5);
  });

  await test("non-numeric on float throws", () => {
    assertTypeCoercionError(() => coerceValue("x", "not-a-number", "float"), "x");
  });

  await test("NaN on float throws (the wire format never emits NaN)", () => {
    assertTypeCoercionError(() => coerceValue("x", "NaN", "float"), "x");
  });

  // ──────────────────────────────────────────────────────────
  console.log("\ncoerceValue — bool");
  // ──────────────────────────────────────────────────────────

  await test("bool true", () => {
    assert.equal(coerceValue("b", "true", "bool"), true);
  });

  await test("bool false", () => {
    assert.equal(coerceValue("b", "false", "bool"), false);
  });

  await test("bool is strict — \"True\" is not accepted", () => {
    assertTypeCoercionError(() => coerceValue("b", "True", "bool"), "b");
  });

  await test("bool is strict — \"1\" is not accepted", () => {
    assertTypeCoercionError(() => coerceValue("b", "1", "bool"), "b");
  });

  // ──────────────────────────────────────────────────────────
  console.log("\ncoerceValue — datetime");
  // ──────────────────────────────────────────────────────────

  await test("datetime 0 micros → Date at epoch", () => {
    const d = coerceValue("dt", "0", "datetime");
    assert.ok(d instanceof Date);
    assert.equal((d as Date).getTime(), 0);
  });

  await test("datetime 1718928000000000 micros → Date at 2024-06-21 UTC", () => {
    const d = coerceValue("dt", "1718928000000000", "datetime");
    assert.ok(d instanceof Date);
    // 1_718_928_000_000_000 µs / 1000 = 1_718_928_000_000 ms
    assert.equal((d as Date).getTime(), 1_718_928_000_000);
  });

  await test("datetime handles huge values without float precision loss", () => {
    // 2262-04-11 is the point where Number loses millisecond precision on
    // Date. Going via BigInt division preserves it.
    const d = coerceValue("dt", "9214646400000001", "datetime");
    assert.ok(d instanceof Date);
    // The raw 9214646400000001 / 1000 = 9214646400000 (integer division).
    assert.equal((d as Date).getTime(), 9_214_646_400_000);
  });

  await test("non-integer datetime throws", () => {
    assertTypeCoercionError(() => coerceValue("dt", "1.5", "datetime"), "dt");
  });

  await test("non-numeric datetime throws", () => {
    assertTypeCoercionError(() => coerceValue("dt", "2024-06-21", "datetime"), "dt");
  });

  // ──────────────────────────────────────────────────────────
  console.log("\ncoerceValue — uuid");
  // ──────────────────────────────────────────────────────────

  await test("canonical lowercase uuid passes through", () => {
    const u = "550e8400-e29b-41d4-a716-446655440000";
    assert.equal(coerceValue("id", u, "uuid"), u);
  });

  await test("uppercase uuid is rejected (server emits lowercase)", () => {
    assertTypeCoercionError(
      () => coerceValue("id", "550E8400-E29B-41D4-A716-446655440000", "uuid"),
      "id",
    );
  });

  await test("uuid without dashes is rejected", () => {
    assertTypeCoercionError(
      () => coerceValue("id", "550e8400e29b41d4a716446655440000", "uuid"),
      "id",
    );
  });

  await test("truncated uuid is rejected", () => {
    assertTypeCoercionError(
      () => coerceValue("id", "550e8400-e29b-41d4-a716", "uuid"),
      "id",
    );
  });

  // ──────────────────────────────────────────────────────────
  console.log("\ncoerceValue — json");
  // ──────────────────────────────────────────────────────────

  await test("json object parses to a plain object", () => {
    // Canonical PJ1 text: keys sorted, no whitespace.
    assert.deepEqual(
      coerceValue("doc", '{"a":1,"b":"two","c":true}', "json"),
      { a: 1, b: "two", c: true },
    );
  });

  await test("json array parses to an array", () => {
    assert.deepEqual(coerceValue("tags", "[1,2,3]", "json"), [1, 2, 3]);
  });

  await test("json nested document round-trips", () => {
    assert.deepEqual(
      coerceValue("doc", '{"author":{"name":"Ada"},"ids":[1,2]}', "json"),
      { author: { name: "Ada" }, ids: [1, 2] },
    );
  });

  await test("json scalar string parses to a string", () => {
    assert.equal(coerceValue("v", '"hello"', "json"), "hello");
  });

  await test("json scalar number parses to a number", () => {
    assert.equal(coerceValue("v", "42", "json"), 42);
  });

  await test("json scalar bool parses to a bool", () => {
    assert.equal(coerceValue("v", "true", "json"), true);
  });

  await test("json invalid text falls back to the raw string (no throw)", () => {
    // The server never emits non-canonical JSON, but the fallback must never
    // turn a readable result into an exception.
    assert.equal(coerceValue("v", "{not json", "json"), "{not json");
  });

  // ──────────────────────────────────────────────────────────
  console.log("\ncoerceValue — null sentinel");
  // ──────────────────────────────────────────────────────────

  await test("\"null\" on int returns null", () => {
    assert.equal(coerceValue("age", "null", "int"), null);
  });

  await test("\"null\" on float returns null", () => {
    assert.equal(coerceValue("x", "null", "float"), null);
  });

  await test("\"null\" on bool returns null", () => {
    assert.equal(coerceValue("b", "null", "bool"), null);
  });

  await test("\"null\" on datetime returns null", () => {
    assert.equal(coerceValue("dt", "null", "datetime"), null);
  });

  await test("\"null\" on uuid returns null", () => {
    assert.equal(coerceValue("id", "null", "uuid"), null);
  });

  await test("\"null\" on json returns null (shared sentinel, before parse)", () => {
    assert.equal(coerceValue("doc", "null", "json"), null);
  });

  // ──────────────────────────────────────────────────────────
  console.log("\ncoerceRow");
  // ──────────────────────────────────────────────────────────

  await test("coerces a mixed row by column name", () => {
    const cols = ["name", "age", "active"];
    const vals = ["Alice", "30", "true"];
    const schema: TypedSchema = { age: "int", active: "bool" };
    const row = coerceRow(cols, vals, schema);
    assert.deepEqual(row, { name: "Alice", age: 30, active: true });
  });

  await test("columns absent from the schema fall through as strings", () => {
    const cols = ["name", "age"];
    const vals = ["Alice", "30"];
    const row = coerceRow(cols, vals, { age: "int" });
    assert.equal(row.name, "Alice");
    assert.equal(row.age, 30);
  });

  await test("short values row treats missing slots as null sentinel", () => {
    // Defensive: if the server somehow sends a shorter row than columns
    // declared, coerce the missing slot as if it were "null".
    const row = coerceRow(["a", "b"], ["1"], { a: "int", b: "int" });
    assert.equal(row.a, 1);
    assert.equal(row.b, null);
  });

  // ──────────────────────────────────────────────────────────
  console.log("\ncoerceRows");
  // ──────────────────────────────────────────────────────────

  await test("coerces multiple rows", () => {
    const cols = ["id", "name"];
    const rows = [
      ["1", "Alice"],
      ["2", "Bob"],
    ];
    const out = coerceRows(cols, rows, { id: "int" });
    assert.deepEqual(out, [
      { id: 1, name: "Alice" },
      { id: 2, name: "Bob" },
    ]);
  });

  await test("empty rows → empty output", () => {
    assert.deepEqual(coerceRows(["a"], [], { a: "int" }), []);
  });

  await test("coercion error on any row surfaces with column name", () => {
    const cols = ["id"];
    const rows = [["1"], ["bogus"]];
    assertTypeCoercionError(() => coerceRows(cols, rows, { id: "int" }), "id");
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nerror shape");
  // ──────────────────────────────────────────────────────────

  await test("all coercion errors are PowDBError with code type_coercion_failed", () => {
    try {
      coerceValue("x", "not-int", "int");
      assert.fail("expected throw");
    } catch (err) {
      assert.ok(err instanceof PowDBError);
      assert.ok(isPowDBError(err));
      assert.equal(err.code, "type_coercion_failed");
    }
  });

  // ──────────────────────────────────────────────────────────
  console.log("\n" + "═".repeat(50));
  console.log(`Results: ${passed} passed, ${failed} failed`);
  if (failures.length > 0) {
    console.log("\nFailures:");
    for (const f of failures) {
      console.log(`  - ${f}`);
    }
  }
  console.log("═".repeat(50));

  process.exit(failed > 0 ? 1 : 0);
}

main().catch((err) => {
  console.error("Test suite crashed:", err);
  process.exit(1);
});
