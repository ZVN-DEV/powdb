// Every error the embedded addon throws carries a stable, machine-readable
// `code` (see the `PowDBErrorCode` union in dts-header.d.ts), so a host can
// branch without matching on message text. Before this existed, napi-rs put its
// own status name on every error, so all seven conditions below arrived in
// JavaScript as `code: "GenericFailure"`; the distinctness assertions at the
// bottom are what would have caught that.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { Database } = require("../index.js");

const HERE = dirname(fileURLToPath(import.meta.url));

/** Every code the addon is allowed to throw, mirroring `mod code` in src/lib.rs. */
const DECLARED_CODES = [
  "query_failed",
  "closed",
  "open_failed",
  "open_panicked",
  "poisoned",
  "invalid_argument",
  "sync_failed",
  "already_open",
  "internal",
];

function freshDir() {
  return mkdtempSync(join(tmpdir(), "powdb-errcode-test-"));
}

/** Run `fn`, require that it throws, and hand back the thrown error. */
function thrown(fn) {
  try {
    fn();
  } catch (err) {
    return err;
  }
  assert.fail("expected the call to throw");
}

/** Assert one call throws an Error whose `code` is exactly `code`. */
function assertCode(code, fn) {
  const err = thrown(fn);
  assert.ok(err instanceof Error, `expected an Error, got ${typeof err}`);
  // A stable string, not an opaque number and not napi's own status name.
  assert.equal(typeof err.code, "string");
  assert.match(err.code, /^[a-z][a-z_]*$/);
  assert.equal(err.code, code);
  // The message still explains the failure; the code is additive.
  assert.ok(err.message.length > 0);
  return err;
}

test("an ordinary query error carries code query_failed", () => {
  const dir = freshDir();
  const db = Database.open(dir);
  try {
    db.query("type T { required id: int }");
    assertCode("query_failed", () => db.query("count(NoSuchTable)"));
    // A parse failure is the same class of failure, so the same code.
    assertCode("query_failed", () => db.query("this is not valid powql"));
    // The typed and SQL surfaces agree with the string surface.
    assertCode("query_failed", () => db.queryNative("count(NoSuchTable)"));
    assertCode("query_failed", () => db.querySql("select * from no_such_table"));
  } finally {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("calls on a closed handle carry code closed", () => {
  const dir = freshDir();
  try {
    const db = Database.open(dir);
    db.close();
    assertCode("closed", () => db.query("count(T)"));
    assertCode("closed", () => db.queryNative("count(T)"));
    assertCode("closed", () => db.close());
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a second open of a live directory carries code already_open", () => {
  const dir = freshDir();
  const db = Database.open(dir);
  try {
    assertCode("already_open", () => Database.open(dir));
    assertCode("already_open", () => Database.openReadOnly(dir));
  } finally {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a failed open carries code open_failed", () => {
  const missing = join(tmpdir(), `powdb-errcode-missing-${process.pid}-${Date.now()}`);
  const err = assertCode("open_failed", () => Database.openReadOnly(missing));
  // open_failed must stay distinct from the terminal open_panicked code, which
  // means the data directory is corrupt rather than merely unreadable.
  assert.notEqual(err.code, "open_panicked");
});

test("rejected arguments carry code invalid_argument", () => {
  const dir = freshDir();
  const db = Database.open(dir);
  try {
    db.query("type T { required id: int }");
    assertCode("invalid_argument", () => db.setSyncMode("turbo"));
    // The embedded facade re-tags a read-only refusal as InvalidArgument ("you
    // called the wrong method"), so it lands here rather than on query_failed.
    assertCode("invalid_argument", () => db.queryReadonly("insert T { id := 1 }"));
    assertCode("invalid_argument", () => db.queryReadonlyNative("insert T { id := 1 }"));
    assertCode("invalid_argument", () =>
      db.queryWithParams("T { id } filter .id = $1", [{}]),
    );
    assertCode("invalid_argument", () =>
      db.queryWithParams("T { id } filter .id = $1", [2n ** 70n]),
    );
    assertCode("invalid_argument", () =>
      db.applyRetainedUnits({
        sinceLsn: 0n,
        databaseId: "not-hex",
        primaryGeneration: 1n,
        walFormatVersion: 1,
        catalogVersion: 5,
        segmentFormatVersion: 1,
        units: [],
      }),
    );
  } finally {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a failed retained-unit apply carries code sync_failed", () => {
  const dir = freshDir();
  const db = Database.open(dir);
  try {
    // No sync boundary was seeded in this data directory, so the apply fails
    // inside the sync substrate rather than on argument validation. That is the
    // powdb::Error::Sync variant, and it must not be flattened into the
    // invalid_argument code the malformed-request cases above produce.
    const err = assertCode("sync_failed", () =>
      db.applyRetainedUnits({
        sinceLsn: 0n,
        databaseId: "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b",
        primaryGeneration: 1n,
        walFormatVersion: 1,
        catalogVersion: 5,
        segmentFormatVersion: 1,
        units: [],
      }),
    );
    assert.notEqual(err.code, "invalid_argument");
  } finally {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("distinct failure conditions get distinct codes", () => {
  const dir = freshDir();
  const live = Database.open(dir);
  const observed = new Set();
  try {
    live.query("type T { required id: int }");
    observed.add(thrown(() => live.query("count(NoSuchTable)")).code);
    observed.add(thrown(() => live.setSyncMode("turbo")).code);
    observed.add(thrown(() => Database.open(dir)).code);
    observed.add(
      thrown(() => Database.openReadOnly(join(tmpdir(), `powdb-errcode-gone-${Date.now()}`))).code,
    );
  } finally {
    live.close();
    rmSync(dir, { recursive: true, force: true });
  }
  observed.add(
    thrown(() => {
      const db = Database.open(freshDir());
      db.close();
      db.query("count(T)");
    }).code,
  );

  // Five conditions, five different codes. Every one of them was the single
  // string "GenericFailure" before the addon carried real codes, so this size
  // check is what fails if the mapping ever collapses again.
  assert.equal(observed.size, 5);
  for (const code of observed) {
    assert.ok(DECLARED_CODES.includes(code), `undeclared code ${code}`);
    assert.notEqual(code, "GenericFailure");
  }
});

test("dts-header.d.ts declares exactly the codes the addon can throw", () => {
  const header = readFileSync(join(HERE, "..", "dts-header.d.ts"), "utf8");
  const union = header.match(/export type PowDBErrorCode =([\s\S]*?)\n\n/);
  assert.ok(union, "PowDBErrorCode union not found in dts-header.d.ts");
  const declared = [...union[1].matchAll(/\|\s*"([a-z_]+)"/g)].map((m) => m[1]);
  // Guard against a regex that quietly matches nothing and passes vacuously.
  assert.equal(declared.length, DECLARED_CODES.length);
  assert.deepEqual([...declared].sort(), [...DECLARED_CODES].sort());
});

test("the published index.d.ts keeps the hand-written error declarations", () => {
  // index.d.ts is regenerated by `napi build` from dts-header.d.ts plus the
  // generated bindings. If the header ever stops being prepended, TypeScript
  // consumers silently lose the error types while the runtime keeps the codes.
  const dts = readFileSync(join(HERE, "..", "index.d.ts"), "utf8");
  assert.match(dts, /export type PowDBErrorCode =/);
  assert.match(dts, /export interface PowDBError extends Error \{/);
  for (const code of DECLARED_CODES) {
    assert.ok(dts.includes(`"${code}"`), `index.d.ts is missing code ${code}`);
  }
});
