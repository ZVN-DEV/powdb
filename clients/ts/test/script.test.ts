/**
 * Pure unit tests for the statement-aware script splitter.
 *
 * Each case mirrors a `split_statements` test in the Rust lexer
 * (`crates/query/src/lexer.rs`), so the TS client's `execScript` splits a
 * script byte-for-byte the same way the CLI's `--exec`/`.powql` path does.
 * If you change one splitter, change both and keep these suites in lockstep.
 *
 * Run with:
 *   npx tsx test/script.test.ts
 */

import { strict as assert } from "node:assert";
import { splitStatements } from "../src/script.js";

let passed = 0;
let failed = 0;
const failures: string[] = [];

function test(name: string, fn: () => void) {
  try {
    fn();
    passed++;
    console.log(`  ✓ ${name}`);
  } catch (err) {
    failed++;
    const msg = err instanceof Error ? err.message : String(err);
    failures.push(`${name}: ${msg}`);
    console.log(`  ✗ ${name}`);
    console.log(`    ${msg}`);
  }
}

console.log("\nsplitStatements — Rust `split_statements` parity");

// mirrors test_split_top_level_semicolons
test("top-level semicolons split", () => {
  assert.deepStrictEqual(
    splitStatements("insert A { a := 1 }; insert B { b := 2 }"),
    ["insert A { a := 1 }", "insert B { b := 2 }"],
  );
});

// mirrors test_split_semicolon_in_string_not_split
test("semicolon inside a string literal does not split", () => {
  assert.deepStrictEqual(
    splitStatements(`insert Note { body := "hello; world" }`),
    [`insert Note { body := "hello; world" }`],
  );
});

// mirrors test_split_escaped_quote_then_semicolon
test("escaped quote keeps the string open across a semicolon", () => {
  // `\"` keeps us inside the string, so the following `;` does not split;
  // the string closes at the final unescaped `"`, then the top-level `;`
  // splits.
  const input = `insert A { v := "a\\"; b" }; insert B { c := 1 }`;
  assert.deepStrictEqual(splitStatements(input), [
    `insert A { v := "a\\"; b" }`,
    "insert B { c := 1 }",
  ]);
});

// mirrors test_split_backslash_consumes_any_char
test("backslash consumes any next char", () => {
  // `"\\"` is a single-backslash string (the `\` escapes the `\`); the
  // string closes at the second `"`, so the trailing `;` splits.
  const input = `insert A { v := "\\\\" }; x`;
  assert.deepStrictEqual(splitStatements(input), [
    `insert A { v := "\\\\" }`,
    "x",
  ]);
});

// mirrors test_split_semicolon_in_comment_not_split
test("semicolon inside a # comment does not split", () => {
  const input = "insert A { a := 1 } # trailing; comment\n; insert B { b := 2 }";
  assert.deepStrictEqual(splitStatements(input), [
    "insert A { a := 1 } # trailing; comment",
    "insert B { b := 2 }",
  ]);
});

// mirrors test_split_drops_empty_segments
test("leading, doubled, and trailing semicolons drop empty segments", () => {
  assert.deepStrictEqual(splitStatements("; A ;; B ;\n\n"), ["A", "B"]);
});

// mirrors test_split_no_semicolon_backcompat
test("no semicolon — single statement back-compat", () => {
  assert.deepStrictEqual(splitStatements("count(User)"), ["count(User)"]);
  assert.deepStrictEqual(splitStatements("   "), []);
});

// mirrors test_split_unterminated_string_tail
test("unterminated string becomes the final segment (never errors)", () => {
  const input = `insert A { a := 1 }; insert B { b := "oops`;
  assert.deepStrictEqual(splitStatements(input), [
    "insert A { a := 1 }",
    `insert B { b := "oops`,
  ]);
});

// mirrors test_split_multiline_string_with_semicolon
test("multiline string with semicolon does not split", () => {
  const input = 'insert A { body := "line1;\nline2" }; insert B { b := 2 }';
  assert.deepStrictEqual(splitStatements(input), [
    'insert A { body := "line1;\nline2" }',
    "insert B { b := 2 }",
  ]);
});

// TS-side extras (no Rust counterpart, same semantics)
test("empty input yields no statements", () => {
  assert.deepStrictEqual(splitStatements(""), []);
});

test("comment text is retained, not stripped (parity with Rust)", () => {
  // The splitter only guards `;` inside comments — it does not remove the
  // comment text itself. The PowQL lexer downstream skips comments.
  assert.deepStrictEqual(splitStatements("# just a comment\n# another; one\n"), [
    "# just a comment\n# another; one",
  ]);
});

console.log("\n" + "═".repeat(50));
console.log(`Results: ${passed} passed, ${failed} failed`);
if (failures.length > 0) {
  console.log("\nFailures:");
  for (const f of failures) console.log(`  - ${f}`);
}
console.log("═".repeat(50));
process.exit(failed > 0 ? 1 : 0);
