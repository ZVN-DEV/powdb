/**
 * Tests for the PowQL escape helpers (clients/ts/src/escape.ts).
 *
 * Pure-JS tests — no server required.
 *
 *     npx tsx test/escape.test.ts
 */

import { strict as assert } from "node:assert";
import {
  escapeIdent,
  escapeLiteral,
  ident,
  powql,
  PowqlIdent,
} from "../src/escape.js";

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

async function main() {
  // ──────────────────────────────────────────────────────────
  console.log("\nescapeLiteral — strings");
  // ──────────────────────────────────────────────────────────

  await test("simple string gets wrapped in double quotes", () => {
    assert.equal(escapeLiteral("Alice"), `"Alice"`);
  });

  await test("empty string is just a pair of quotes", () => {
    assert.equal(escapeLiteral(""), `""`);
  });

  await test("internal double quotes are backslash-escaped (C-style)", () => {
    // input:  say "hi"        (9 chars)
    // output: "say \"hi\""    (11 chars)
    assert.equal(escapeLiteral('say "hi"'), `"say \\"hi\\""`);
  });

  await test("multiple internal quotes are each escaped", () => {
    assert.equal(escapeLiteral('hi "there"'), `"hi \\"there\\""`);
  });

  await test("only leading quote", () => {
    assert.equal(escapeLiteral('"hello'), `"\\"hello"`);
  });

  await test("only trailing quote", () => {
    assert.equal(escapeLiteral('hello"'), `"hello\\""`);
  });

  await test("backslashes are escaped (C-style) so they don't swallow next char", () => {
    // input: a\b  (3 chars: a, backslash, b)
    // output: "a\\b"  (6 chars: ", a, \, \, b, ")
    assert.equal(escapeLiteral("a\\b"), `"a\\\\b"`);
  });

  await test("backslash immediately before quote — escapes are independent", () => {
    // input:  a\"b  (4 chars: a, \, ", b)
    // output: "a\\\"b"  (8 chars: ", a, \, \, \, ", b, ")
    assert.equal(escapeLiteral('a\\"b'), `"a\\\\\\"b"`);
  });

  await test("newlines pass through raw (lexer reads them literally)", () => {
    assert.equal(escapeLiteral("a\nb"), `"a\nb"`);
  });

  await test("unicode passes through", () => {
    assert.equal(escapeLiteral("café ☕"), `"café ☕"`);
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nescapeLiteral — numbers / bigints / booleans / null");
  // ──────────────────────────────────────────────────────────

  await test("positive integer", () => {
    assert.equal(escapeLiteral(42), "42");
  });

  await test("zero", () => {
    assert.equal(escapeLiteral(0), "0");
  });

  await test("negative float", () => {
    assert.equal(escapeLiteral(-1.5), "-1.5");
  });

  await test("float", () => {
    assert.equal(escapeLiteral(3.14), "3.14");
  });

  await test("bigint renders as decimal digits", () => {
    assert.equal(escapeLiteral(42n), "42");
  });

  await test("large bigint", () => {
    assert.equal(
      escapeLiteral(12345678901234567890n),
      "12345678901234567890"
    );
  });

  await test("bigint negative", () => {
    assert.equal(escapeLiteral(-7n), "-7");
  });

  await test("true / false", () => {
    assert.equal(escapeLiteral(true), "true");
    assert.equal(escapeLiteral(false), "false");
  });

  await test("null renders as the bareword null", () => {
    assert.equal(escapeLiteral(null), "null");
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nescapeLiteral — rejected types");
  // ──────────────────────────────────────────────────────────

  await test("NaN throws", () => {
    assert.throws(() => escapeLiteral(NaN), TypeError);
  });

  await test("Infinity throws", () => {
    assert.throws(() => escapeLiteral(Infinity), TypeError);
  });

  await test("-Infinity throws", () => {
    assert.throws(() => escapeLiteral(-Infinity), TypeError);
  });

  await test("undefined throws", () => {
    // @ts-expect-error — undefined is not a valid input type
    assert.throws(() => escapeLiteral(undefined), TypeError);
  });

  await test("plain object throws", () => {
    // @ts-expect-error — object is not a valid input type
    assert.throws(() => escapeLiteral({}), TypeError);
  });

  await test("array throws", () => {
    // @ts-expect-error — array is not a valid input type
    assert.throws(() => escapeLiteral([]), TypeError);
  });

  await test("symbol throws", () => {
    // @ts-expect-error — symbol is not a valid input type
    assert.throws(() => escapeLiteral(Symbol("x")), TypeError);
  });

  await test("Date throws", () => {
    // @ts-expect-error — Date is not a valid input type
    assert.throws(() => escapeLiteral(new Date()), TypeError);
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nescapeIdent");
  // ──────────────────────────────────────────────────────────

  await test("simple ASCII identifier", () => {
    assert.equal(escapeIdent("User"), "User");
  });

  await test("identifier with digits", () => {
    assert.equal(escapeIdent("user_1"), "user_1");
  });

  await test("identifier starting with underscore", () => {
    assert.equal(escapeIdent("_private"), "_private");
  });

  await test("identifier starting with digit throws", () => {
    assert.throws(() => escapeIdent("1bad"), TypeError);
  });

  await test("identifier with hyphen throws", () => {
    assert.throws(() => escapeIdent("bad-name"), TypeError);
  });

  await test("identifier with space throws", () => {
    assert.throws(() => escapeIdent("bad name"), TypeError);
  });

  await test("identifier with quote throws", () => {
    assert.throws(() => escapeIdent('bad"name'), TypeError);
  });

  await test("identifier with semicolon throws", () => {
    assert.throws(() => escapeIdent("users; drop"), TypeError);
  });

  await test("empty identifier throws", () => {
    assert.throws(() => escapeIdent(""), TypeError);
  });

  await test("non-string identifier throws", () => {
    // @ts-expect-error — only strings are accepted
    assert.throws(() => escapeIdent(123), TypeError);
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nident() / PowqlIdent");
  // ──────────────────────────────────────────────────────────

  await test("ident() returns a PowqlIdent", () => {
    const i = ident("User");
    assert.ok(i instanceof PowqlIdent);
    assert.equal(i.name, "User");
  });

  await test("ident() does NOT validate eagerly — only at render time", () => {
    // Rationale: the tagged template is the call site that matters. `ident`
    // itself is just a factory.
    const bad = ident("bad-name");
    assert.ok(bad instanceof PowqlIdent);
    assert.throws(() => powql`select ${bad}`, TypeError);
  });

  // ──────────────────────────────────────────────────────────
  console.log("\npowql tagged template");
  // ──────────────────────────────────────────────────────────

  await test("identifier-only interpolation", () => {
    assert.equal(powql`select ${ident("t")}`, "select t");
  });

  await test("string literal interpolation", () => {
    assert.equal(powql`x := ${"a"}`, `x := "a"`);
  });

  await test("number literal interpolation", () => {
    assert.equal(powql`age := ${42}`, "age := 42");
  });

  await test("null interpolation", () => {
    assert.equal(powql`age := ${null}`, "age := null");
  });

  await test("boolean interpolation", () => {
    assert.equal(powql`active := ${true}`, "active := true");
  });

  await test("bigint interpolation", () => {
    assert.equal(powql`n := ${9007199254740993n}`, "n := 9007199254740993");
  });

  await test("complete insert query with mixed types", () => {
    const q = powql`insert ${ident("User")} { name := ${'O"Brien'}, age := ${42} }`;
    assert.equal(q, `insert User { name := "O\\"Brien", age := 42 }`);
  });

  await test("multiple interpolations of same type", () => {
    const q = powql`${ident("T")} filter .city = ${"NYC"} and .age > ${30} { .name }`;
    assert.equal(q, `T filter .city = "NYC" and .age > 30 { .name }`);
  });

  await test("no interpolations works", () => {
    assert.equal(powql`select * from t`, "select * from t");
  });

  await test("empty template works", () => {
    assert.equal(powql``, "");
  });

  await test("interpolation at the very start", () => {
    assert.equal(powql`${ident("T")} filter .a = 1`, "T filter .a = 1");
  });

  await test("interpolation at the very end", () => {
    assert.equal(powql`x := ${42}`, "x := 42");
  });

  await test("adjacent interpolations", () => {
    assert.equal(powql`${ident("a")}${ident("b")}`, "ab");
  });

  // ──────────────────────────────────────────────────────────
  console.log("\ninjection resistance");
  // ──────────────────────────────────────────────────────────

  await test("injection attempt is neutralised by backslash-escaping the quote", () => {
    // Classic injection payload: try to break out of the string, inject
    // malicious statements, and open a comment to swallow the trailing quote.
    // With C-style escaping, the embedded `"` becomes `\"`, so the payload
    // stays trapped inside a single string literal.
    const payload = '"); drop table users; --';
    const q = powql`${payload}`;
    // Opening " + \" (escaped inner quote) + rest of payload + closing "
    assert.equal(q, `"\\"); drop table users; --"`);
  });

  await test("identifier interpolation rejects injection attempts", () => {
    assert.throws(
      () => powql`select ${ident('t"); drop table users; --')}`,
      TypeError
    );
  });

  await test("escapeLiteral on a value with nothing but quotes", () => {
    // 3 internal quotes → each becomes \" (2 chars) → 6 chars inside,
    // plus 2 outer quotes = 8 total.
    assert.equal(escapeLiteral('"""'), `"\\"\\"\\""`);
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
