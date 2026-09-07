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
const { Database } = require("../loader.js");

const HERE = dirname(fileURLToPath(import.meta.url));

/** Every code the addon is allowed to throw, mirroring `mod code` in src/lib.rs. */
const DECLARED_CODES = [
  "query_failed",
  "parse_error",
  "timeout",
  "size_exceeded",
  "readonly_refused",
  "constraint_violation",
  "cancelled",
  "closed",
  "open_failed",
  "open_panicked",
  "poisoned",
  "invalid_argument",
  "sync_failed",
  "already_open",
  "internal",
  "unsupported_platform",
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
    assertCode("query_failed", () => db.query("NoSuchTable { id }"));
    // The typed and SQL surfaces agree with the string surface.
    assertCode("query_failed", () => db.queryNative("NoSuchTable { id }"));
    assertCode("query_failed", () => db.querySql("select * from no_such_table"));
  } finally {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("engine failures carry the class the wire protocol gives them", () => {
  // Every one of these arrived as a flat `query_failed` with no class, while
  // the networked client exposed `wireErrorClass` for the identical engine
  // error. An embedded caller could not tell a unique-constraint violation
  // from a parse error without matching on message text.
  const dir = freshDir();
  const db = Database.open(dir);
  try {
    db.query("type T { required unique id: int }");
    db.query("insert T { id := 1 }");

    const parse = assertCode("parse_error", () => db.query("this is not valid powql"));
    assert.equal(parse.errorClass, 1);

    const execution = assertCode("query_failed", () => db.query("NoSuchTable { id }"));
    assert.equal(execution.errorClass, 2);

    const constraint = assertCode("constraint_violation", () =>
      db.query("insert T { id := 1 }"),
    );
    assert.equal(constraint.errorClass, 8);
  } finally {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a read-only handle refuses a write with readonly_refused", () => {
  const dir = freshDir();
  const writer = Database.open(dir);
  writer.query("type T { required id: int }");
  writer.close();
  const reader = Database.openReadOnly(dir);
  try {
    const err = assertCode("readonly_refused", () => reader.query("insert T { id := 1 }"));
    assert.equal(err.errorClass, 5);
  } finally {
    reader.close();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("every engine code has an error class", () => {
  const { ERROR_CLASS_BY_CODE } = require("../loader.js");
  // unsupported_platform is raised by the entry point before any engine call,
  // so it has no wire class to carry and is the one declared code absent here.
  const engineCodes = DECLARED_CODES.filter((code) => code !== "unsupported_platform");
  for (const code of engineCodes) {
    assert.equal(
      typeof ERROR_CLASS_BY_CODE[code],
      "number",
      `code ${code} has no error class in the loader`,
    );
  }
  // Guard against the table growing codes the addon cannot actually throw.
  assert.deepEqual(Object.keys(ERROR_CLASS_BY_CODE).sort(), [...engineCodes].sort());
});

test("a rejected path or memory budget is invalid_argument, not a raw OS error", () => {
  assertCode("invalid_argument", () => Database.open(""));
  assertCode("invalid_argument", () => Database.open(join(HERE, "error-codes.test.mjs")));
  const dir = freshDir();
  try {
    assertCode("invalid_argument", () => Database.openWithMemoryLimit(dir, Number.NaN));
    assertCode("invalid_argument", () => Database.openWithMemoryLimit(dir, 1.5));
    assertCode("invalid_argument", () => Database.openWithMemoryLimit(dir, -1));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a napi coercion failure is reported as invalid_argument", () => {
  const dir = freshDir();
  const db = Database.open(dir);
  try {
    // Generated argument coercion runs before any addon logic and raises
    // napi's own status names; the entry point rewrites them.
    const err = assertCode("invalid_argument", () => db.query(42));
    assert.equal(err.errorClass, 2);
  } finally {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("an undefined parameter is refused rather than bound as null", () => {
  // `[undefined]` used to bind a SQL NULL, so a caller who read a missing
  // property off an object silently ran a different query instead of hearing
  // about the mistake.
  const dir = freshDir();
  const db = Database.open(dir);
  try {
    db.query("type T { required id: int }");
    db.query("insert T { id := 1 }");
    const err = assertCode("invalid_argument", () =>
      db.queryWithParams("T { id } filter .id = $1", [undefined]),
    );
    assert.match(err.message, /undefined/);
    // An explicit null still binds a null.
    db.queryWithParams("T { id } filter .id = $1", [null]);
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
    observed.add(thrown(() => live.query("NoSuchTable { id }")).code);
    observed.add(thrown(() => live.query("this is not valid powql")).code);
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

  // Six conditions, six different codes. Every one of them was the single
  // string "GenericFailure" before the addon carried real codes, so this size
  // check is what fails if the mapping ever collapses again.
  assert.equal(observed.size, 6);
  for (const code of observed) {
    assert.ok(DECLARED_CODES.includes(code), `undeclared code ${code}`);
    assert.notEqual(code, "GenericFailure");
  }
});

test("every static factory classifies what it throws, not just the methods", () => {
  // napi defines `#[napi(factory)]` statics as non-writable, and the loader's
  // wrapper used to skip exactly those -- so `open`, `openReadOnly` and their
  // memory-limited siblings, the only four ways to obtain a handle, were the
  // one surface with no errorClass and no napi-status rewriting, while
  // dts-header.d.ts promised both on every entry point.
  const statics = Object.getOwnPropertyNames(Database).filter(
    (name) => typeof Database[name] === "function" && name !== "constructor",
  );
  // A floor, so this cannot pass by finding nothing to check.
  assert.ok(statics.length >= 4, `expected the addon's factories, found ${statics}`);
  for (const name of statics) {
    // Every factory takes the data directory first, so a number is a napi
    // coercion failure on all of them.
    const err = thrown(() => Database[name](123));
    assert.equal(err.code, "invalid_argument", `${name} did not rewrite the napi status`);
    assert.equal(err.errorClass, 2, `${name} threw without an errorClass`);
  }
});

test("an errorClass rides along on every factory failure a caller can hit", () => {
  const dir = freshDir();
  try {
    // invalid_argument is class 2; open_failed is class 0. Both are in the
    // loader's table, so the .d.ts promise that errorClass is absent "only on
    // an error whose code the loader does not recognize" has to hold here.
    assert.equal(thrown(() => Database.open("")).errorClass, 2);
    assert.equal(thrown(() => Database.openWithMemoryLimit(dir, Number.NaN)).errorClass, 2);
    assert.equal(thrown(() => Database.openReadOnlyWithMemoryLimit(dir, 1.5)).errorClass, 2);
    const missing = join(tmpdir(), `powdb-errclass-missing-${process.pid}-${Date.now()}`);
    const failed = thrown(() => Database.openReadOnly(missing));
    assert.equal(failed.code, "open_failed");
    assert.equal(failed.errorClass, 0);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("the exported class stays usable as a class after wrapping", () => {
  // Classification must not cost `instanceof`, the prototype methods, or the
  // class name a host sees in a stack trace.
  const dir = freshDir();
  const db = Database.open(dir);
  try {
    assert.ok(db instanceof Database);
    assert.equal(Database.name, "Database");
    assert.equal(typeof db.query, "function");
    // Static identity is stable: two reads are the same function object.
    assert.equal(Database.open, Database.open);
  } finally {
    db.close();
    rmSync(dir, { recursive: true, force: true });
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
