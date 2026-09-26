// Public embedded-addon statement atomicity regressions.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { Database } = require("../loader.js");

function freshDir() {
  return mkdtempSync(join(tmpdir(), "powdb-node-atomicity-"));
}

function withDb(fn) {
  const dir = freshDir();
  try {
    const db = Database.open(dir);
    fn(db);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

function textRows(db, query) {
  const result = db.query(query);
  assert.equal(result.kind, "rows");
  return result.rows;
}

test("failed autocommit statement preserves existing row data", () => {
  withDb((db) => {
    db.query("type Item { required unique id: int, name: str }");
    db.query(`insert Item { id := 1, name := "old" }`);

    assert.throws(
      () =>
        db.query(
          `insert Item { id := 2, name := "would-leak" }, { id := 1, name := "dupe" }`,
        ),
      /unique|duplicate|already exists/i,
    );

    assert.deepEqual(textRows(db, "Item order .id { .id, .name }"), [["1", "old"]]);
  });
});

test("aborted explicit transaction refuses reads and rolls back before reuse", () => {
  withDb((db) => {
    db.query("type Item { required unique id: int, name: str }");
    db.query(`insert Item { id := 1, name := "committed" }`);

    db.query("begin");
    db.query(`insert Item { id := 2, name := "tx" }`);
    assert.throws(
      () => db.query(`insert Item { id := 1, name := "dupe" }`),
      /unique|duplicate|already exists/i,
    );

    assert.throws(
      () => db.queryNative("Item { .id }"),
      /explicit transaction is aborted/i,
    );
    assert.throws(() => db.query("commit"), /explicit transaction is aborted/i);

    db.query("rollback");
    assert.deepEqual(textRows(db, "Item order .id { .id, .name }"), [
      ["1", "committed"],
    ]);

    db.query(`insert Item { id := 3, name := "after" }`);
    assert.deepEqual(textRows(db, "Item order .id { .id, .name }"), [
      ["1", "committed"],
      ["3", "after"],
    ]);
  });
});
