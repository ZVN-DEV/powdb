// Lane B: typed (native) results + parameter binding for the embedded addon.
// queryNative/querySqlNative/queryReadonlyNative return lossless WireValue cells
// (mirroring @zvndev/powdb-client's WireValue union); queryWithParams binds
// positional $N parameters. The legacy string query() path must stay unchanged.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { Database } = require("../index.js");

function freshDir() {
  return mkdtempSync(join(tmpdir(), "powdb-native-test-"));
}

function withDb(fn) {
  const dir = freshDir();
  const db = Database.open(dir);
  try {
    fn(db);
  } finally {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  }
}

test("all nine TypeIds round-trip through queryNative as WireValue cells", () => {
  withDb((db) => {
    db.query(
      "type A { required id: int, dt: datetime, u: uuid, b: bytes, f: float, bo: bool, s: str, j: json }",
    );
    // `dt` coerces the int literal to datetime via the column type; `u`/`b` use
    // the uuid()/bytes() cast sugar. `j` is unsorted on input; the engine
    // canonicalizes the PJ1.
    db.query(
      `insert A { id := 1, dt := 1700000000000000, u := uuid("550e8400-e29b-41d4-a716-446655440000"), b := bytes("\\\\xdeadbeef"), f := 2.5, bo := true, s := "hi", j := "{\\"b\\":2,\\"a\\":1}" }`,
    );

    const r = db.queryNative("A { id, dt, u, b, f, bo, s, j }");
    assert.equal(r.kind, "rows");
    assert.equal(r.rows.length, 1);
    const [id, dt, u, b, f, bo, s, j] = r.rows[0];

    // Int / DateTime => bigint (TypeId 1, 5).
    assert.deepEqual(id, { type: "int", value: 1n });
    assert.deepEqual(dt, { type: "datetime", value: 1700000000000000n });
    // Uuid => canonical hyphenated string (TypeId 6).
    assert.deepEqual(u, {
      type: "uuid",
      value: "550e8400-e29b-41d4-a716-446655440000",
    });
    // Bytes => Buffer, raw bytes preserved (TypeId 7).
    assert.equal(b.type, "bytes");
    assert.ok(Buffer.isBuffer(b.value));
    assert.deepEqual([...b.value], [0xde, 0xad, 0xbe, 0xef]);
    // Float / Bool / Str (TypeId 2, 3, 4).
    assert.deepEqual(f, { type: "float", value: 2.5 });
    assert.deepEqual(bo, { type: "bool", value: true });
    assert.deepEqual(s, { type: "str", value: "hi" });
    // Json => parsed value plus raw PJ1 bytes (TypeId 8). Keys canonicalized.
    assert.equal(j.type, "json");
    assert.deepEqual(j.value, { a: 1, b: 2 });
    assert.ok(j.pj1 instanceof Uint8Array);
    assert.ok(j.pj1.length > 0);
  });
});

test("Empty (TypeId 0) is a distinct tagged cell, not a null string", () => {
  withDb((db) => {
    // `note` is optional; row 2 omits it, leaving it Empty.
    db.query("type N { required id: int, note: str }");
    db.query(`insert N { id := 1, note := "here" }`);
    db.query("insert N { id := 2 }");

    const present = db.queryNative("N filter .id = 1 { .note }").rows[0][0];
    const missing = db.queryNative("N filter .id = 2 { .note }").rows[0][0];
    assert.deepEqual(present, { type: "str", value: "here" });
    assert.deepEqual(missing, { type: "empty" });
  });
});

test("null-vs-missing distinction at the whole-JSON-column level", () => {
  withDb((db) => {
    // `body` optional. Row 1 stores JSON null; row 2 omits body entirely.
    db.query("type Doc { required id: int, body: json }");
    db.query(`insert Doc { id := 1, body := "null" }`);
    db.query("insert Doc { id := 2 }");

    const jsonNull = db.queryNative("Doc filter .id = 1 { .body }").rows[0][0];
    const absent = db.queryNative("Doc filter .id = 2 { .body }").rows[0][0];

    // A JSON null is a typed json cell whose value is null...
    assert.equal(jsonNull.type, "json");
    assert.equal(jsonNull.value, null);
    // ...distinct from a missing cell, which is Empty.
    assert.deepEqual(absent, { type: "empty" });
    assert.notEqual(jsonNull.type, absent.type);
  });
});

test("i64 beyond Number.MAX_SAFE_INTEGER survives as bigint", () => {
  withDb((db) => {
    db.query("type Big { required id: int, n: int }");
    // 2^53 + 1 and i64::MAX, both unrepresentable as a JS number.
    db.query("insert Big { id := 1, n := 9007199254740993 }");
    db.query("insert Big { id := 2, n := 9223372036854775807 }");

    const a = db.queryNative("Big filter .id = 1 { .n }").rows[0][0];
    const b = db.queryNative("Big filter .id = 2 { .n }").rows[0][0];
    assert.equal(a.type, "int");
    assert.equal(typeof a.value, "bigint");
    assert.equal(a.value, 9007199254740993n);
    assert.equal(b.value, 9223372036854775807n);
    // The exact value survived: a JS number would have rounded 2^53+1 down to
    // 2^53 (9007199254740992), losing the distinction the bigint preserves.
    assert.notEqual(a.value, 9007199254740992n);
    assert.equal(Number(a.value), 9007199254740992); // demonstrates the loss avoided
  });
});

test("bytes survive a round trip losslessly, including a NUL byte", () => {
  withDb((db) => {
    db.query("type B { required id: int, data: bytes }");
    db.query(`insert B { id := 1, data := bytes("\\\\x00ff10") }`);
    const cell = db.queryNative("B { data }").rows[0][0];
    assert.equal(cell.type, "bytes");
    assert.deepEqual([...cell.value], [0x00, 0xff, 0x10]);
  });
});

test("querySqlNative returns typed cells via the SQL frontend", () => {
  withDb((db) => {
    db.query("type User { required name: str, age: int }");
    db.query(`insert User { name := "Ada", age := 36 }`);
    const r = db.querySqlNative("SELECT age FROM User");
    assert.equal(r.kind, "rows");
    assert.deepEqual(r.rows[0][0], { type: "int", value: 36n });
  });
});

test("queryReadonlyNative reads typed cells and rejects mutations", () => {
  withDb((db) => {
    db.query("type T { required id: int }");
    db.query("insert T { id := 7 }");
    const r = db.queryReadonlyNative("T { id }");
    assert.deepEqual(r.rows[0][0], { type: "int", value: 7n });
    assert.throws(() => db.queryReadonlyNative("insert T { id := 8 }"));
  });
});

test("queryWithParams binds each supported parameter type", () => {
  withDb((db) => {
    db.query(
      "type U { required name: str, age: int, score: float, active: bool }",
    );
    db.query(`insert U { name := "Ada", age := 36, score := 1.5, active := true }`);
    db.query(`insert U { name := "Bo", age := 20, score := 9.9, active := false }`);

    // string param
    let r = db.queryWithParams("U filter .name = $1 { .name }", ["Ada"]);
    assert.deepEqual(r.rows, [[{ type: "str", value: "Ada" }]]);
    // number param that is integral binds as int
    r = db.queryWithParams("U filter .age = $1 { .age }", [36]);
    assert.deepEqual(r.rows, [[{ type: "int", value: 36n }]]);
    // bigint param binds as int
    r = db.queryWithParams("U filter .age = $1 { .age }", [20n]);
    assert.deepEqual(r.rows, [[{ type: "int", value: 20n }]]);
    // float param
    r = db.queryWithParams("U filter .score = $1 { .name }", [1.5]);
    assert.deepEqual(r.rows, [[{ type: "str", value: "Ada" }]]);
    // bool param
    r = db.queryWithParams("U filter .active = $1 { .name }", [false]);
    assert.deepEqual(r.rows, [[{ type: "str", value: "Bo" }]]);
    // null param binds PowQL null without error (matches no non-null name)
    r = db.queryWithParams("U filter .name = $1 { .name }", [null]);
    assert.equal(r.kind, "rows");
    assert.equal(r.rows.length, 0);
  });
});

test("queryWithParams rejects unsupported parameter types with a clear error", () => {
  withDb((db) => {
    db.query("type T { required id: int }");
    // A Buffer / bytes has no positional-parameter encoding.
    assert.throws(
      () => db.queryWithParams("T filter .id = $1 { .id }", [Buffer.from([1, 2, 3])]),
      /unsupported type|supported parameter types/,
    );
    // A plain object is likewise rejected.
    assert.throws(
      () => db.queryWithParams("T filter .id = $1 { .id }", [{ nope: true }]),
      /unsupported type|supported parameter types/,
    );
  });
});

test("regression: legacy string query() output is unchanged", () => {
  withDb((db) => {
    db.query(
      "type A { required id: int, u: uuid, b: bytes, j: json, note: str }",
    );
    db.query(
      `insert A { id := 42, u := uuid("550e8400-e29b-41d4-a716-446655440000"), b := bytes("\\\\xdeadbeef"), j := "{\\"b\\":2,\\"a\\":1}" }`,
    );

    // Every cell is a string, rendered by the shared Value::to_wire_string.
    const r = db.query("A { id, u, b, j, note }");
    assert.equal(r.kind, "rows");
    assert.deepEqual(r.rows, [
      [
        "42",
        "550e8400-e29b-41d4-a716-446655440000",
        "<4 bytes>", // bytes render as a placeholder on the string path
        `{"a":1,"b":2}`, // canonical JSON text
        "null", // missing optional cell renders as the "null" bareword
      ],
    ]);

    // Scalar shape also unchanged (unset optional fields are simply absent).
    assert.deepEqual(db.query("count(A)"), { kind: "scalar", value: "1" });
  });
});
