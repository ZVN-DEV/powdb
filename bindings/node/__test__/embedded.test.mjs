// End-to-end test for the embedded Node addon: open an in-process database,
// run PowQL + SQL, and assert the result shape matches the @zvndev/powdb-client
// QueryResult union. No server, no socket.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
// `napi build` emits index.js (a loader that picks the right platform .node).
const { Database } = require("../index.js");

function freshDir() {
  return mkdtempSync(join(tmpdir(), "powdb-embedded-test-"));
}

function seedApplyBoundary(dir, databaseId, generation = 1, lsn = 0) {
  const syncDir = join(dir, ".powdb-sync");
  mkdirSync(syncDir, { recursive: true });
  writeFileSync(
    join(syncDir, "identity.json"),
    JSON.stringify({
      format_version: 1,
      database_id: databaseId,
      primary_generation: generation,
      created_unix_secs: 1,
    }),
  );
  writeFileSync(
    join(syncDir, "apply-state.json"),
    JSON.stringify({
      format_version: 1,
      database_id: databaseId.match(/../g).map((byte) => Number.parseInt(byte, 16)),
      primary_generation: generation,
      wal_format_version: 1,
      catalog_version: 5,
      from_lsn: lsn,
      through_lsn: lsn,
      applied_lsn: lsn,
      status: "complete",
      started_unix_secs: 1,
      updated_unix_secs: 1,
    }),
  );
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

test("applyRetainedUnits exposes the native embedded sync adapter", () => {
  const dir = freshDir();
  try {
    const databaseId = "07070707070707070707070707070707";
    seedApplyBoundary(dir, databaseId);
    const db = Database.open(dir);
    const result = db.applyRetainedUnits({
      sinceLsn: 0n,
      databaseId,
      primaryGeneration: 1n,
      walFormatVersion: 1,
      catalogVersion: 5,
      segmentFormatVersion: 1,
      units: [],
    });
    assert.deepEqual(result, { throughLsn: 0n, unitsApplied: 0 });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("applyRetainedUnits accepts byte database identity", () => {
  const dir = freshDir();
  try {
    const databaseId = "09090909090909090909090909090909";
    seedApplyBoundary(dir, databaseId);
    const db = Database.open(dir);
    const result = db.applyRetainedUnits({
      sinceLsn: 0n,
      databaseId: Uint8Array.from(databaseId.match(/../g).map((byte) => Number.parseInt(byte, 16))),
      primaryGeneration: 1n,
      walFormatVersion: 1,
      catalogVersion: 5,
      segmentFormatVersion: 1,
      units: [],
    });
    assert.deepEqual(result, { throughLsn: 0n, unitsApplied: 0 });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("applyRetainedUnits applies a non-empty retained unit through native addon", () => {
  const dir = freshDir();
  try {
    const databaseId = "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a";
    seedApplyBoundary(dir, databaseId);
    const db = Database.open(dir);
    const result = db.applyRetainedUnits({
      sinceLsn: 0n,
      databaseId,
      primaryGeneration: 1n,
      walFormatVersion: 1,
      catalogVersion: 5,
      segmentFormatVersion: 1,
      units: [
        {
          txId: 0n,
          recordType: 4,
          lsn: 1n,
          data: new Uint8Array(),
        },
      ],
    });
    assert.deepEqual(result, { throughLsn: 1n, unitsApplied: 1 });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("applyRetainedUnits validates database identity shape", () => {
  const dir = freshDir();
  try {
    const db = Database.open(dir);
    assert.throws(() =>
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
    rmSync(dir, { recursive: true, force: true });
  }
});

test("reopen recovers committed data", () => {
  const dir = freshDir();
  try {
    const db = Database.open(dir);
    db.query("type T { required id: int }");
    db.query("insert T { id := 1 }");
    db.close(); // deterministic flush + lock release

    const db2 = Database.open(dir);
    const count = db2.query("count(T)");
    assert.equal(count.value, "1");
    db2.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("close() releases the handle and reopen works", () => {
  const dir = freshDir();
  try {
    const db = Database.open(dir);
    db.query("type T { required id: int }");
    db.query("insert T { id := 1 }");
    db.close();
    // Reopen in the same process now that the lock is released.
    const db2 = Database.open(dir);
    assert.equal(db2.query("count(T)").value, "1");
    db2.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("calls after close() throw, and close() is not silently idempotent", () => {
  const dir = freshDir();
  try {
    const db = Database.open(dir);
    db.close();
    assert.throws(() => db.query("count(T)"), /database is closed/);
    assert.throws(() => db.close(), /database is closed/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("opening the same dir twice in one process throws", () => {
  const dir = freshDir();
  try {
    const db = Database.open(dir);
    assert.throws(() => Database.open(dir), /already open in this process/);
    // After close, the second open succeeds.
    db.close();
    const db2 = Database.open(dir);
    db2.close();
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
