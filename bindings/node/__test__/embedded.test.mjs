// End-to-end test for the embedded Node addon: open an in-process database,
// run PowQL + SQL, and assert the result shape matches the @zvndev/powdb-client
// QueryResult union. No server, no socket.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
// `napi build` emits index.js (a loader that picks the right platform .node).
const { Database } = require("../index.js");

function freshDir() {
  return mkdtempSync(join(tmpdir(), "powdb-embedded-test-"));
}

test("open, write, read in-process", () => {
  const dir = freshDir();
  try {
    const db = Database.open(dir);
    const created = db.query("type User { required name: str, age: int }");
    assert.equal(created.kind, "message");

    const inserted = db.query(`insert User { name := "Ada", age := 36 }`);
    assert.equal(inserted.kind, "ok");
    assert.equal(inserted.affected, 1n);

    const count = db.query("count(User)");
    assert.equal(count.kind, "scalar");
    assert.equal(count.value, "1");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("rows come back as string[][] matching the wire shape", () => {
  const dir = freshDir();
  try {
    const db = Database.open(dir);
    db.query("type T { required id: int, name: str }");
    db.query(`insert T { id := 1, name := "x" }`);
    const r = db.query("T { id, name }");
    assert.equal(r.kind, "rows");
    assert.deepEqual(r.columns, ["id", "name"]);
    assert.deepEqual(r.rows, [["1", "x"]]);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("SQL frontend works in-process", () => {
  const dir = freshDir();
  try {
    const db = Database.open(dir);
    db.query("type User { required name: str, age: int }");
    db.query(`insert User { name := "Ada", age := 36 }`);
    const r = db.querySql("SELECT name FROM User");
    assert.equal(r.kind, "rows");
    assert.equal(r.rows.length, 1);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("reopen recovers committed data", () => {
  const dir = freshDir();
  try {
    let db = Database.open(dir);
    db.query("type T { required id: int }");
    db.query("insert T { id := 1 }");
    db = null; // drop handle (GC closes it)
    global.gc?.();

    const db2 = Database.open(dir);
    const count = db2.query("count(T)");
    assert.equal(count.value, "1");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a bad query returns an error, not a crash", () => {
  const dir = freshDir();
  try {
    const db = Database.open(dir);
    assert.throws(() => db.query("this is not valid powql"));
    // The handle is still usable after a normal query error.
    const created = db.query("type Ok { required id: int }");
    assert.equal(created.kind, "message");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
