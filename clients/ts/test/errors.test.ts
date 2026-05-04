/**
 * Tests for the PowDBError class (clients/ts/src/errors.ts).
 *
 * Pure-JS tests — no server required.
 *
 *     npx tsx test/errors.test.ts
 */

import { strict as assert } from "node:assert";
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

async function main() {
  // ──────────────────────────────────────────────────────────
  console.log("\nconstruction");
  // ──────────────────────────────────────────────────────────

  await test("sets name to PowDBError", () => {
    const err = new PowDBError("boom", "connect_failed");
    assert.equal(err.name, "PowDBError");
  });

  await test("message round-trips", () => {
    const err = new PowDBError("something failed", "query_failed");
    assert.equal(err.message, "something failed");
  });

  await test("code is exposed on the instance", () => {
    const err = new PowDBError("boom", "auth_failed");
    assert.equal(err.code, "auth_failed");
  });

  await test("extends Error", () => {
    const err = new PowDBError("boom", "connect_failed");
    assert.ok(err instanceof Error);
  });

  await test("extends PowDBError (prototype chain survives transpilation)", () => {
    const err = new PowDBError("boom", "connect_failed");
    assert.ok(err instanceof PowDBError);
  });

  await test("toString includes name and message", () => {
    const err = new PowDBError("boom", "connect_failed");
    // Node's default Error.toString: "PowDBError: boom"
    assert.equal(String(err), "PowDBError: boom");
  });

  await test("stack trace is populated", () => {
    const err = new PowDBError("boom", "connect_failed");
    assert.ok(typeof err.stack === "string");
    assert.ok(err.stack!.length > 0);
  });

  // ──────────────────────────────────────────────────────────
  console.log("\ncause");
  // ──────────────────────────────────────────────────────────

  await test("cause is preserved when provided", () => {
    const inner = new Error("socket eof");
    const err = new PowDBError("connect failed", "connect_failed", {
      cause: inner,
    });
    assert.equal((err as { cause?: unknown }).cause, inner);
  });

  await test("cause is undefined when not provided", () => {
    const err = new PowDBError("boom", "connect_failed");
    assert.equal((err as { cause?: unknown }).cause, undefined);
  });

  await test("cause can be a non-Error value", () => {
    const err = new PowDBError("boom", "protocol_error", { cause: "raw" });
    assert.equal((err as { cause?: unknown }).cause, "raw");
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nisPowDBError");
  // ──────────────────────────────────────────────────────────

  await test("narrows a PowDBError instance", () => {
    const err: unknown = new PowDBError("boom", "timeout");
    assert.ok(isPowDBError(err));
    if (isPowDBError(err)) {
      // Type-narrowing check — this should compile without casts.
      assert.equal(err.code, "timeout");
    }
  });

  await test("rejects a plain Error", () => {
    assert.equal(isPowDBError(new Error("plain")), false);
  });

  await test("rejects a string", () => {
    assert.equal(isPowDBError("oops"), false);
  });

  await test("rejects null", () => {
    assert.equal(isPowDBError(null), false);
  });

  await test("rejects undefined", () => {
    assert.equal(isPowDBError(undefined), false);
  });

  await test("rejects a plain object that happens to have a .code", () => {
    assert.equal(isPowDBError({ code: "connect_failed", message: "x" }), false);
  });

  // ──────────────────────────────────────────────────────────
  console.log("\ncatch-block branching");
  // ──────────────────────────────────────────────────────────

  await test("a thrown PowDBError survives throw/catch (instanceof still true)", () => {
    let caught: unknown;
    try {
      throw new PowDBError("boom", "connect_failed");
    } catch (err) {
      caught = err;
    }
    assert.ok(caught instanceof PowDBError);
    assert.ok(caught instanceof Error);
  });

  await test("branching on .code works as documented", () => {
    const errs = [
      new PowDBError("a", "connect_failed"),
      new PowDBError("b", "auth_failed"),
      new PowDBError("c", "timeout"),
    ];
    const retryable = errs.filter(
      (e) => e.code === "connect_failed" || e.code === "timeout",
    );
    assert.equal(retryable.length, 2);
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
