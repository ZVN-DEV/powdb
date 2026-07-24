/**
 * Pure-decoder + cancellation tests for the PowDB TypeScript client.
 *
 * These tests do not require a running PowDB server. The cancellation test
 * uses a loopback `net.createServer` that speaks just enough of the protocol
 * to complete the handshake, then sits silent so the query can be aborted.
 *
 * Run with:
 *   npx tsx test/protocol.test.ts
 */

import * as net from "node:net";
import { strict as assert } from "node:assert";
import {
  tryDecode,
  encode,
  MAX_PAYLOAD_SIZE,
  MAX_COLUMNS,
  MAX_ROWS,
  MAX_RESULT_CELLS,
  MAX_SYNC_UNITS,
  MSG_QUERY_NATIVE,
  MSG_QUERY_PARAMS_NATIVE,
  MSG_QUERY_SQL_NATIVE,
  MSG_RESULT_ROWS_NATIVE,
  MSG_RESULT_SCALAR_NATIVE,
  type Message,
  type WireValue,
  type WireSyncStatus,
} from "../src/protocol.js";
import {
  Client,
  PowDBError,
  isPowDBError,
  assertServerCatalogVersionSupported,
  SUPPORTED_CATALOG_VERSION,
  type WireValue as PublicWireValue,
} from "../src/index.js";

let passed = 0;
let failed = 0;
const failures: string[] = [];

async function test(name: string, fn: () => Promise<void> | void) {
  try {
    await fn();
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

function buildFrame(msgType: number, payloadLen: number): Buffer {
  // Only the header needs to be well-formed; tryDecode should reject on
  // payloadLen before looking at any body.
  const header = Buffer.alloc(6);
  header.writeUInt8(msgType, 0);
  header.writeUInt8(0, 1);
  header.writeUInt32LE(payloadLen, 2);
  return header;
}

function sampleSyncStatus(overrides: Partial<WireSyncStatus> = {}): WireSyncStatus {
  return {
    replicaId: "replica-a",
    active: true,
    lastAppliedLsn: 7n,
    remoteLsn: 10n,
    servableLsn: 10n,
    unarchivedLsn: 0n,
    lagLsn: 3n,
    lagBytes: 2048n,
    lagMs: 5000n,
    stale: true,
    repairAction: "pull",
    lastSyncError: null,
    ...overrides,
  };
}

function collectFrames(
  scratch: Buffer,
  chunk: Buffer,
): { messages: Message[]; rest: Buffer } {
  let buf = Buffer.concat([scratch, chunk]);
  const messages: Message[] = [];
  while (true) {
    const decoded = tryDecode(buf);
    if (decoded === null) break;
    messages.push(decoded.msg);
    buf = buf.subarray(decoded.consumed);
  }
  return { messages, rest: buf };
}

function listen(server: net.Server): Promise<number> {
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const addr = server.address();
      if (!addr || typeof addr === "string") {
        reject(new Error("unexpected server address"));
        return;
      }
      resolve(addr.port);
    });
  });
}

/** Handshakes, then replies to each Query after `delayMs` with a scalar equal
 *  to the query text — so a caller can tell replies apart and abort in the gap. */
function echoServer(delayMs = 20): net.Server {
  return net.createServer((sock) => {
    let scratch = Buffer.alloc(0);
    sock.on("data", (chunk) => {
      const collected = collectFrames(scratch, Buffer.from(chunk));
      scratch = collected.rest;
      for (const msg of collected.messages) {
        if (msg.type === "Connect") {
          sock.write(encode({ type: "ConnectOk", version: "0.8.0" }));
        } else if (msg.type === "Query") {
          const value = msg.query;
          setTimeout(() => {
            if (!sock.destroyed) {
              sock.write(encode({ type: "ResultScalar", value }));
            }
          }, delayMs);
        } else if (msg.type === "Disconnect") {
          sock.end();
        }
      }
    });
    sock.on("error", () => {});
  });
}

/** Handshakes, then stays silent — any Query hangs until aborted. */
function silentServer(): net.Server {
  return net.createServer((sock) => {
    sock.once("data", () =>
      sock.write(encode({ type: "ConnectOk", version: "0.8.0" })),
    );
    sock.on("error", () => {});
  });
}

async function main() {
  console.log("\nPure decoder — size caps");

  await test("tryDecode throws on payloadLen > MAX_PAYLOAD_SIZE", () => {
    const frame = buildFrame(0x07, MAX_PAYLOAD_SIZE + 1);
    assert.throws(() => tryDecode(frame), /payload too large/);
  });

  await test("tryDecode accepts payloadLen exactly at MAX_PAYLOAD_SIZE header", () => {
    // Buffer only contains the header — tryDecode should return null
    // (not enough bytes) rather than throwing.
    const frame = buildFrame(0x07, MAX_PAYLOAD_SIZE);
    const result = tryDecode(frame);
    assert.equal(result, null);
  });

  await test("tryDecode throws on MSG_RESULT_ROWS with colCount > MAX_COLUMNS", () => {
    // Hand-craft a ResultRows frame with colCount = MAX_COLUMNS + 1 but
    // otherwise minimal. payloadLen must cover at least the colCount field.
    const payload = Buffer.alloc(2);
    payload.writeUInt16LE(MAX_COLUMNS + 1, 0);
    const frame = Buffer.alloc(6 + payload.length);
    frame.writeUInt8(0x07, 0); // MSG_RESULT_ROWS
    frame.writeUInt8(0, 1);
    frame.writeUInt32LE(payload.length, 2);
    payload.copy(frame, 6);
    assert.throws(() => tryDecode(frame), /too many columns/);
  });

  await test("tryDecode throws on MSG_RESULT_ROWS with rowCount > MAX_ROWS", () => {
    // colCount=0 (valid), then rowCount = MAX_ROWS + 1.
    const payload = Buffer.alloc(2 + 4);
    payload.writeUInt16LE(0, 0);
    payload.writeUInt32LE(MAX_ROWS + 1, 2);
    const frame = Buffer.alloc(6 + payload.length);
    frame.writeUInt8(0x07, 0);
    frame.writeUInt8(0, 1);
    frame.writeUInt32LE(payload.length, 2);
    payload.copy(frame, 6);
    assert.throws(() => tryDecode(frame), /too many rows/);
  });

  await test("tryDecode rejects nonzero rows with zero columns before allocation", () => {
    const payload = Buffer.alloc(2 + 4);
    payload.writeUInt16LE(0, 0);
    payload.writeUInt32LE(MAX_ROWS, 2);
    const frame = Buffer.alloc(6 + payload.length);
    frame.writeUInt8(0x07, 0);
    frame.writeUInt8(0, 1);
    frame.writeUInt32LE(payload.length, 2);
    payload.copy(frame, 6);
    assert.throws(() => tryDecode(frame), /zero columns/);
  });

  await test("tryDecode rejects impossible row shape before allocation", () => {
    const payload = Buffer.alloc(2 + 4 + 4);
    payload.writeUInt16LE(1, 0);
    payload.writeUInt32LE(0, 2); // empty column name
    payload.writeUInt32LE(MAX_ROWS, 6);
    const frame = Buffer.alloc(6 + payload.length);
    frame.writeUInt8(0x07, 0);
    frame.writeUInt8(0, 1);
    frame.writeUInt32LE(payload.length, 2);
    payload.copy(frame, 6);
    assert.throws(() => tryDecode(frame), /row data too short/);
  });

  // A hostile (or MITM'd) server can declare a huge narrow result whose wire
  // cost is tiny: every empty cell is 4 bytes on the wire but costs an order of
  // magnitude more JS heap (a row array plus a slot). Before MAX_RESULT_CELLS a
  // ~40 MB frame decoded into ~1.9 GB of heap. The cell cap bounds it.
  await test("tryDecode rejects a result whose declared cells exceed MAX_RESULT_CELLS", () => {
    const rowCount = MAX_RESULT_CELLS + 1;
    // colCount = 1 with an empty column name, then rowCount empty-string cells:
    // the byte-shape check passes, so only the cell cap can reject this.
    const payloadLen = 2 + 4 + 4 + rowCount * 4;
    const frame = Buffer.alloc(6 + payloadLen);
    frame.writeUInt8(0x07, 0); // MSG_RESULT_ROWS
    frame.writeUInt8(0, 1);
    frame.writeUInt32LE(payloadLen, 2);
    frame.writeUInt16LE(1, 6);
    frame.writeUInt32LE(0, 8);
    frame.writeUInt32LE(rowCount, 12);
    assert.throws(() => tryDecode(frame), /result too large/);
  });

  await test("tryDecode rejects a wide native result that exceeds MAX_RESULT_CELLS", () => {
    // Wide rather than tall, and backed by enough bytes to clear the native
    // byte-shape check, so only the cell cap can reject it.
    const colCount = 8;
    const rowCount = Math.ceil((MAX_RESULT_CELLS + 1) / colCount);
    const header = 2 + colCount * 4 + 4;
    const payload = Buffer.alloc(header + rowCount * colCount * 5);
    payload.writeUInt16LE(colCount, 0);
    for (let i = 0; i < colCount; i++) payload.writeUInt32LE(0, 2 + i * 4);
    payload.writeUInt32LE(rowCount, 2 + colCount * 4);
    const frame = Buffer.alloc(6 + payload.length);
    frame.writeUInt8(MSG_RESULT_ROWS_NATIVE, 0);
    frame.writeUInt8(0, 1);
    frame.writeUInt32LE(payload.length, 2);
    payload.copy(frame, 6);
    assert.throws(() => tryDecode(frame), /result too large/);
  });

  await test("tryDecode still accepts an ordinary result under the cell cap", () => {
    const columns = ["name", "age"];
    const rows = [
      ["ada", "36"],
      ["bob", "24"],
    ];
    const frame = encode({ type: "ResultRows", columns, rows });
    const decoded = tryDecode(frame);
    assert.ok(decoded);
    assert.deepEqual(decoded!.msg, { type: "ResultRows", columns, rows });
  });

  await test("tryDecode throws intentional error on truncated ResultRows column count", () => {
    const frame = Buffer.alloc(6);
    frame.writeUInt8(0x07, 0);
    frame.writeUInt8(0, 1);
    frame.writeUInt32LE(0, 2);
    assert.throws(() => tryDecode(frame), /truncated column count/);
  });

  await test("tryDecode throws intentional error on truncated ResultRows row count", () => {
    const payload = Buffer.alloc(2);
    payload.writeUInt16LE(0, 0);
    const frame = Buffer.alloc(6 + payload.length);
    frame.writeUInt8(0x07, 0);
    frame.writeUInt8(0, 1);
    frame.writeUInt32LE(payload.length, 2);
    payload.copy(frame, 6);
    assert.throws(() => tryDecode(frame), /truncated row count/);
  });

  await test("tryDecode throws intentional error on truncated ResultOk payload", () => {
    const payload = Buffer.alloc(7);
    const frame = Buffer.alloc(6 + payload.length);
    frame.writeUInt8(0x09, 0);
    frame.writeUInt8(0, 1);
    frame.writeUInt32LE(payload.length, 2);
    payload.copy(frame, 6);
    assert.throws(() => tryDecode(frame), /truncated affected count/);
  });

  console.log("\nNative typed wire surface");

  await test("legacy 0x07 row frame remains byte-identical", () => {
    assert.equal(
      encode({ type: "ResultRows", columns: ["x"], rows: [["y"]] }).toString("hex"),
      "07001000000001000100000078010000000100000079",
    );
  });

  const nativeGoldenHex =
    "16009c000000090001000000650100000069010000006601000000620100000073010000006401000000750100000078010000006a0100000000000000000108000000ffffffffffffdfff02080000000000000000000440030100000001040600000068c3a96c6c6f050800000015615391a61f0600061000000000112233445566778899aabbccddeeff0704000000007f80ff0809000000030100000000002000";
  const nativeValues: WireValue[] = [
    { type: "empty" },
    { type: "int", value: -9007199254740993n },
    { type: "float", value: 2.5 },
    { type: "bool", value: true },
    { type: "str", value: "héllo" },
    { type: "datetime", value: 1723650123456789n },
    {
      type: "uuid",
      value: Uint8Array.from([
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99,
        0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
      ]),
    },
    { type: "bytes", value: Uint8Array.from([0x00, 0x7f, 0x80, 0xff]) },
    {
      type: "json",
      value: 9007199254740993n,
      pj1: Uint8Array.from([3, 1, 0, 0, 0, 0, 0, 32, 0]),
    },
  ];

  await test("native mixed row matches the Rust golden byte-for-byte", () => {
    const encoded = encode({
      type: "ResultRowsNative",
      columns: ["e", "i", "f", "b", "s", "d", "u", "x", "j"],
      rows: [nativeValues],
    });
    assert.equal(encoded.toString("hex"), nativeGoldenHex);
    const decoded = tryDecode(Buffer.from(nativeGoldenHex, "hex"));
    assert.ok(decoded);
    assert.equal(decoded.msg.type, "ResultRowsNative");
    if (decoded.msg.type === "ResultRowsNative") {
      assert.deepStrictEqual(decoded.msg.rows, [nativeValues]);
    }
  });

  await test("public lossless cells preserve empty, string null, and raw PJ1 null", () => {
    const values: PublicWireValue[] = [
      { type: "empty" },
      { type: "str", value: "null" },
      {
        type: "json",
        value: null,
        pj1: Uint8Array.from([0]),
      },
    ];
    const decoded = tryDecode(
      encode({
        type: "ResultRowsNative",
        columns: ["missing", "text", "json"],
        rows: [values],
      }),
    );
    assert.ok(decoded);
    assert.equal(decoded.msg.type, "ResultRowsNative");
    if (decoded.msg.type === "ResultRowsNative") {
      assert.deepStrictEqual(decoded.msg.rows[0], values);
      const json = decoded.msg.rows[0]?.[2];
      assert.equal(json?.type, "json");
      if (json?.type === "json") {
        assert.deepStrictEqual(json.pj1, Uint8Array.from([0]));
      }
    }
  });

  await test("native request tags round-trip without legacy fallback", () => {
    const requests: Message[] = [
      { type: "QueryNative", query: "T" },
      {
        type: "QueryWithParamsNative",
        query: "T filter .x = $1",
        params: [{ tag: "int", value: 7n }],
      },
      { type: "QuerySqlNative", query: "SELECT * FROM T" },
    ];
    assert.deepStrictEqual(requests.map((request) => encode(request)[0]), [
      MSG_QUERY_NATIVE,
      MSG_QUERY_PARAMS_NATIVE,
      MSG_QUERY_SQL_NATIVE,
    ]);
    for (const request of requests) {
      assert.deepStrictEqual(tryDecode(encode(request))?.msg, request);
    }
  });

  await test("native scalar rejects malformed typed cells", () => {
    const typedFrame = (cell: Buffer): Buffer => {
      const out = Buffer.alloc(6 + cell.length);
      out[0] = MSG_RESULT_SCALAR_NATIVE;
      out.writeUInt32LE(cell.length, 2);
      cell.copy(out, 6);
      return out;
    };
    const cell = (tag: number, body: number[]): Buffer => {
      const out = Buffer.alloc(5 + body.length);
      out[0] = tag;
      out.writeUInt32LE(body.length, 1);
      Buffer.from(body).copy(out, 5);
      return out;
    };
    for (const malformed of [
      cell(0xff, []),
      cell(1, [0, 0, 0, 0, 0, 0, 0]),
      cell(3, [2]),
      cell(4, [0xff]),
      cell(8, [0xff]),
      cell(8, [0, 0]),
    ]) {
      assert.throws(() => tryDecode(typedFrame(malformed)));
    }
    assert.throws(() => tryDecode(typedFrame(Buffer.concat([cell(0, []), Buffer.from([0])]))), /trailing bytes/);
  });

  await test("native JSON recursively decodes unsafe integers as bigint", () => {
    const pj1 = Buffer.from(
      "070100000011000000160000002c000000010000006106010000000d00000016000000030100000000002000",
      "hex",
    );
    const cell = Buffer.concat([
      Buffer.from([8]),
      Buffer.from([pj1.length, 0, 0, 0]),
      pj1,
    ]);
    const frame = Buffer.alloc(6 + cell.length);
    frame[0] = MSG_RESULT_SCALAR_NATIVE;
    frame.writeUInt32LE(cell.length, 2);
    cell.copy(frame, 6);
    const decoded = tryDecode(frame)?.msg;
    assert.equal(decoded?.type, "ResultScalarNative");
    if (decoded?.type === "ResultScalarNative") {
      assert.deepStrictEqual(decoded.value, {
        type: "json",
        value: { a: [9007199254740993n] },
        pj1: new Uint8Array(pj1),
      });
    }
  });

  await test("native rows reject impossible counts before allocation", () => {
    const payload = Buffer.alloc(2 + 4 + 1 + 4);
    payload.writeUInt16LE(1, 0);
    payload.writeUInt32LE(1, 2);
    payload[6] = 0x78;
    payload.writeUInt32LE(MAX_ROWS, 7);
    const frame = Buffer.alloc(6 + payload.length);
    frame[0] = MSG_RESULT_ROWS_NATIVE;
    frame.writeUInt32LE(payload.length, 2);
    payload.copy(frame, 6);
    assert.throws(() => tryDecode(frame), /too short/);
  });

  console.log("\nConnect frame — optional username (multi-user auth)");

  // Helper: length-prefixed string, identical to the wire encoding.
  const lpString = (s: string): Buffer => {
    const bytes = Buffer.from(s, "utf8");
    const out = Buffer.alloc(4 + bytes.length);
    out.writeUInt32LE(bytes.length, 0);
    bytes.copy(out, 4);
    return out;
  };

  await test("encodes Connect with username after password (round-trip)", () => {
    const buf = encode({
      type: "Connect",
      dbName: "main",
      password: "pw",
      username: "alice",
    });
    const decoded = tryDecode(buf);
    assert.ok(decoded, "frame should decode");
    assert.equal(decoded.msg.type, "Connect");
    if (decoded.msg.type === "Connect") {
      assert.equal(decoded.msg.dbName, "main");
      assert.equal(decoded.msg.password, "pw");
      assert.equal(decoded.msg.username, "alice");
    }
  });

  await test("encodes Connect with null username as byte-identical legacy frame", () => {
    const buf = encode({
      type: "Connect",
      dbName: "main",
      password: "pw",
      username: null,
    });
    // Hand-build the pre-username (0.3.x) frame: header + dbName + password,
    // with NO trailing username field. Old servers must see exactly this.
    const payload = Buffer.concat([lpString("main"), lpString("pw")]);
    const expected = Buffer.alloc(6 + payload.length);
    expected.writeUInt8(0x01, 0); // MSG_CONNECT
    expected.writeUInt8(0, 1); // flags
    expected.writeUInt32LE(payload.length, 2);
    payload.copy(expected, 6);
    assert.deepStrictEqual(buf, expected);
  });

  await test("decodes legacy Connect frame (no username bytes) with username=null", () => {
    // Frame as produced by a 0.3.x client: dbName + password only.
    const payload = Buffer.concat([lpString("main"), lpString("pw")]);
    const frame = Buffer.alloc(6 + payload.length);
    frame.writeUInt8(0x01, 0);
    frame.writeUInt8(0, 1);
    frame.writeUInt32LE(payload.length, 2);
    payload.copy(frame, 6);
    const decoded = tryDecode(frame);
    assert.ok(decoded, "frame should decode");
    assert.equal(decoded.msg.type, "Connect");
    if (decoded.msg.type === "Connect") {
      assert.equal(decoded.msg.username, null);
    }
  });

  await test("decodes empty (len=0) username as null, mirroring the server", () => {
    // Server treats a zero-length username string as None.
    const payload = Buffer.concat([
      lpString("main"),
      lpString("pw"),
      lpString(""),
    ]);
    const frame = Buffer.alloc(6 + payload.length);
    frame.writeUInt8(0x01, 0);
    frame.writeUInt8(0, 1);
    frame.writeUInt32LE(payload.length, 2);
    payload.copy(frame, 6);
    const decoded = tryDecode(frame);
    assert.ok(decoded, "frame should decode");
    assert.equal(decoded.msg.type, "Connect");
    if (decoded.msg.type === "Connect") {
      assert.equal(decoded.msg.username, null);
    }
  });

  console.log("\nQueryWithParams — positional $N binding round-trip");

  await test("encode/decode QueryWithParams preserves query and all param types", () => {
    const buf = encode({
      type: "QueryWithParams",
      query: "insert User { name := $1, age := $2, ok := $3, note := $4, f := $5 }",
      params: [
        { tag: "str", value: `a"b\\c; drop User` },
        { tag: "int", value: -7n },
        { tag: "bool", value: true },
        { tag: "null" },
        { tag: "float", value: 2.5 },
      ],
    });
    // New frame must use the dedicated 0x04 tag.
    assert.equal(buf.readUInt8(0), 0x04);
    const decoded = tryDecode(buf);
    assert.ok(decoded, "frame should decode");
    assert.equal(decoded.msg.type, "QueryWithParams");
    if (decoded.msg.type === "QueryWithParams") {
      assert.ok(decoded.msg.query.includes("$1"));
      assert.equal(decoded.msg.params.length, 5);
      assert.deepStrictEqual(decoded.msg.params[0], {
        tag: "str",
        value: `a"b\\c; drop User`,
      });
      assert.deepStrictEqual(decoded.msg.params[1], { tag: "int", value: -7n });
      assert.deepStrictEqual(decoded.msg.params[2], { tag: "bool", value: true });
      assert.deepStrictEqual(decoded.msg.params[3], { tag: "null" });
      assert.deepStrictEqual(decoded.msg.params[4], {
        tag: "float",
        value: 2.5,
      });
    }
  });

  await test("decode rejects an unknown param tag", () => {
    // header + empty query + count=1 + bogus tag 0x63
    const payload = Buffer.concat([
      lpString(""),
      Buffer.from([0x01, 0x00]), // count = 1 (u16 LE)
      Buffer.from([0x63]), // bogus tag
    ]);
    const frame = Buffer.alloc(6 + payload.length);
    frame.writeUInt8(0x04, 0);
    frame.writeUInt8(0, 1);
    frame.writeUInt32LE(payload.length, 2);
    payload.copy(frame, 6);
    assert.throws(() => tryDecode(frame), /unknown param tag/);
  });

  console.log("\nEmbedded sync frames — request/result round-trip");

  await test("encode/decode SyncStatus request", () => {
    const buf = encode({ type: "SyncStatus", replicaId: "replica-a" });
    assert.equal(buf.readUInt8(0), 0x20);
    const decoded = tryDecode(buf);
    assert.ok(decoded, "frame should decode");
    assert.deepStrictEqual(decoded.msg, {
      type: "SyncStatus",
      replicaId: "replica-a",
    });
  });

  await test("assertServerCatalogVersionSupported accepts <= max, rejects newer", () => {
    // The ceiling is the entity-links catalog format (v7, since 0.19): a
    // client stating an older ceiling is refused by any v7-activated server.
    assert.equal(SUPPORTED_CATALOG_VERSION, 7);
    // A server on an older or equal catalog format is readable.
    assertServerCatalogVersionSupported(SUPPORTED_CATALOG_VERSION - 1);
    assertServerCatalogVersionSupported(SUPPORTED_CATALOG_VERSION);
    // A server on a newer catalog format the client cannot read is rejected.
    assert.throws(
      () => assertServerCatalogVersionSupported(SUPPORTED_CATALOG_VERSION + 1),
      /newer than this client supports/,
    );
    // An explicit client max is honored.
    assertServerCatalogVersionSupported(5, 5);
    assert.throws(() => assertServerCatalogVersionSupported(6, 5), /upgrade the client/);
    // A nonsense version is rejected.
    assert.throws(() => assertServerCatalogVersionSupported(0), /invalid server catalog version/);
  });

  await test("encode/decode SyncPull request", () => {
    const databaseId = Buffer.from("sync-protocol!!!");
    const buf = encode({
      type: "SyncPull",
      replicaId: "replica-a",
      sinceLsn: 7n,
      maxUnits: 128,
      maxBytes: 4096n,
      databaseId,
      primaryGeneration: 9n,
      walFormatVersion: 1,
      catalogVersion: 2,
      segmentFormatVersion: 1,
    });
    assert.equal(buf.readUInt8(0), 0x21);
    const decoded = tryDecode(buf);
    assert.ok(decoded, "frame should decode");
    assert.equal(decoded.msg.type, "SyncPull");
    if (decoded.msg.type === "SyncPull") {
      assert.equal(decoded.msg.replicaId, "replica-a");
      assert.equal(decoded.msg.sinceLsn, 7n);
      assert.equal(decoded.msg.maxUnits, 128);
      assert.equal(decoded.msg.maxBytes, 4096n);
      assert.deepStrictEqual(decoded.msg.databaseId, databaseId);
      assert.equal(decoded.msg.primaryGeneration, 9n);
      assert.equal(decoded.msg.walFormatVersion, 1);
      assert.equal(decoded.msg.catalogVersion, 2);
      assert.equal(decoded.msg.segmentFormatVersion, 1);
    }
  });

  await test("encode/decode SyncAck request", () => {
    const buf = encode({
      type: "SyncAck",
      replicaId: "replica-a",
      appliedLsn: 10n,
      remoteLsn: 11n,
    });
    assert.equal(buf.readUInt8(0), 0x22);
    const decoded = tryDecode(buf);
    assert.ok(decoded, "frame should decode");
    assert.deepStrictEqual(decoded.msg, {
      type: "SyncAck",
      replicaId: "replica-a",
      appliedLsn: 10n,
      remoteLsn: 11n,
    });
  });

  await test("encode/decode SyncStatusResult preserves lag and repair action", () => {
    const status = sampleSyncStatus({
      repairAction: "awaitArchive",
      lastSyncError: "primary WAL is not yet archived",
    });
    const decoded = tryDecode(encode({ type: "SyncStatusResult", status }));
    assert.ok(decoded, "frame should decode");
    assert.equal(decoded.msg.type, "SyncStatusResult");
    if (decoded.msg.type === "SyncStatusResult") {
      assert.deepStrictEqual(decoded.msg.status, status);
    }
  });

  await test("encode/decode SyncPullResult preserves retained units and hasMore", () => {
    const units = [
      { txId: 1n, recordType: 4, lsn: 8n, data: Buffer.from([1, 2, 3]) },
      { txId: 1n, recordType: 4, lsn: 9n, data: Buffer.from([4, 5]) },
    ];
    const decoded = tryDecode(
      encode({
        type: "SyncPullResult",
        status: sampleSyncStatus(),
        units,
        hasMore: true,
      }),
    );
    assert.ok(decoded, "frame should decode");
    assert.equal(decoded.msg.type, "SyncPullResult");
    if (decoded.msg.type === "SyncPullResult") {
      assert.deepStrictEqual(decoded.msg.units, units);
      assert.equal(decoded.msg.hasMore, true);
      assert.equal(decoded.msg.status.repairAction, "pull");
    }
  });

  await test("encode rejects retained units with recordType outside u8", () => {
    assert.throws(
      () =>
        encode({
          type: "SyncPullResult",
          status: sampleSyncStatus(),
          units: [
            {
              txId: 1n,
              recordType: 256,
              lsn: 8n,
              data: Buffer.from([1]),
            },
          ],
          hasMore: false,
        }),
      /record type must fit in u8/,
    );
  });

  await test("encode/decode SyncAckResult preserves acknowledgement summary", () => {
    const decoded = tryDecode(
      encode({
        type: "SyncAckResult",
        previousAppliedLsn: 7n,
        appliedLsn: 10n,
        remoteLsn: 10n,
        advanced: true,
        status: sampleSyncStatus({
          stale: false,
          repairAction: "none",
          lagLsn: 0n,
          lagBytes: 0n,
          lagMs: 0n,
        }),
      }),
    );
    assert.ok(decoded, "frame should decode");
    assert.equal(decoded.msg.type, "SyncAckResult");
    if (decoded.msg.type === "SyncAckResult") {
      assert.equal(decoded.msg.previousAppliedLsn, 7n);
      assert.equal(decoded.msg.appliedLsn, 10n);
      assert.equal(decoded.msg.remoteLsn, 10n);
      assert.equal(decoded.msg.advanced, true);
      assert.equal(decoded.msg.status.stale, false);
    }
  });

  await test("decode rejects an unknown sync repair action", () => {
    const frame = encode({
      type: "SyncStatusResult",
      status: sampleSyncStatus({ repairAction: "pull" }),
    });
    const decoded = tryDecode(frame);
    assert.ok(decoded, "sanity: frame should decode before mutation");
    const mutated = Buffer.from(frame);
    // Payload layout mirrors crates/server/src/protocol.rs:
    // replica string, active, lastApplied option, remoteLsn, five more u64
    // options, stale, repairAction, lastSyncError option.
    const repairActionOffset =
      6 + 4 + Buffer.byteLength("replica-a") + 1 + 9 + 8 + 9 * 5 + 1;
    mutated[repairActionOffset] = 0x63;
    assert.throws(() => tryDecode(mutated), /unknown sync repair action/);
  });

  await test("decode rejects too many retained units", () => {
    const statusFrame = encode({
      type: "SyncStatusResult",
      status: sampleSyncStatus(),
    });
    const statusPayload = statusFrame.subarray(6);
    const count = Buffer.alloc(4);
    count.writeUInt32LE(MAX_SYNC_UNITS + 1, 0);
    const payload = Buffer.concat([statusPayload, count, Buffer.from([0])]);
    const frame = Buffer.alloc(6 + payload.length);
    frame.writeUInt8(0x24, 0);
    frame.writeUInt8(0, 1);
    frame.writeUInt32LE(payload.length, 2);
    payload.copy(frame, 6);
    assert.throws(() => tryDecode(frame), /too many retained units/);
  });

  console.log("\nEmbedded sync client helpers — mock server");

  await test("Client syncStatus/syncPull/syncAck send and decode sync frames", async () => {
    const seen: Message[] = [];
    const syncEvents: unknown[] = [];
    const connected = new Promise<{ port: number; server: net.Server }>(
      (resolveConn) => {
        const server = net.createServer((sock) => {
          let scratch = Buffer.alloc(0);
          sock.on("data", (chunk) => {
            const collected = collectFrames(scratch, Buffer.from(chunk));
            scratch = collected.rest;
            for (const msg of collected.messages) {
              if (msg.type === "Connect") {
                sock.write(encode({ type: "ConnectOk", version: "0.7.2" }));
                continue;
              }
              seen.push(msg);
              if (msg.type === "SyncStatus") {
                sock.write(
                  encode({
                    type: "SyncStatusResult",
                    status: sampleSyncStatus(),
                  }),
                );
              } else if (msg.type === "SyncPull") {
                sock.write(
                  encode({
                    type: "SyncPullResult",
                    status: sampleSyncStatus(),
                    units: [
                      {
                        txId: 1n,
                        recordType: 4,
                        lsn: 8n,
                        data: Buffer.from([8]),
                      },
                    ],
                    hasMore: false,
                  }),
                );
              } else if (msg.type === "SyncAck") {
                sock.write(
                  encode({
                    type: "SyncAckResult",
                    previousAppliedLsn: 7n,
                    appliedLsn: 8n,
                    remoteLsn: 10n,
                    advanced: true,
                    status: sampleSyncStatus({ lastAppliedLsn: 8n }),
                  }),
                );
              } else if (msg.type === "Disconnect") {
                sock.end();
              }
            }
          });
        });
        server.listen(0, "127.0.0.1", () => {
          const addr = server.address();
          if (!addr || typeof addr === "string") {
            throw new Error("unexpected server address");
          }
          resolveConn({ port: addr.port, server });
        });
      },
    );
    const { port, server } = await connected;
    const client = await Client.connect({
      host: "127.0.0.1",
      port,
      connectTimeoutMs: 1000,
    });
    client.on("sync", (event) => syncEvents.push(event));

    const status = await client.syncStatus("replica-a");
    assert.equal(status.repairAction, "pull");

    const pull = await client.syncPull({
      replicaId: "replica-a",
      sinceLsn: 7n,
      maxUnits: 128,
      maxBytes: 4096n,
      databaseId: "73796e632d70726f746f636f6c212121",
      primaryGeneration: 9n,
      walFormatVersion: 1,
      catalogVersion: 2,
      segmentFormatVersion: 1,
    });
    assert.equal(pull.units.length, 1);
    assert.equal(pull.hasMore, false);

    const ack = await client.syncAck({
      replicaId: "replica-a",
      appliedLsn: 8n,
      remoteLsn: 10n,
    });
    assert.equal(ack.advanced, true);

    assert.deepStrictEqual(
      seen.map((msg) => msg.type),
      ["SyncStatus", "SyncPull", "SyncAck"],
    );
    assert.equal(syncEvents.length, 3);
    assert.equal((syncEvents[1] as { units?: number }).units, 1);
    assert.equal(
      (syncEvents[1] as { status?: { remoteLsn?: bigint } }).status?.remoteLsn,
      10n,
    );

    await assert.rejects(
      () =>
        client.syncPull({
          replicaId: "replica-a",
          sinceLsn: 8n,
          maxUnits: 0,
          maxBytes: 4096n,
          databaseId: "73796e632d70726f746f636f6c212121",
          primaryGeneration: 9n,
          walFormatVersion: 1,
          catalogVersion: 2,
          segmentFormatVersion: 1,
        }),
      /maxUnits must be between 1 and 4096/,
    );
    assert.deepStrictEqual(
      seen.map((msg) => msg.type),
      ["SyncStatus", "SyncPull", "SyncAck"],
      "local maxUnits validation must not write an invalid SyncPull frame",
    );

    await client.close();
    await new Promise<void>((r) => server.close(() => r()));
  });

  await test("Client native APIs preserve types and never replay as legacy queries", async () => {
    let legacyRequests = 0;
    const server = net.createServer((sock) => {
      let scratch = Buffer.alloc(0);
      sock.on("data", (chunk) => {
        const collected = collectFrames(scratch, Buffer.from(chunk));
        scratch = collected.rest;
        for (const msg of collected.messages) {
          switch (msg.type) {
            case "Connect":
              sock.write(encode({ type: "ConnectOk", version: "0.13.0" }));
              break;
            case "QueryNative":
              sock.write(
                encode({
                  type: "ResultRowsNative",
                  columns: ["e", "i", "f", "b", "s", "d", "u", "x", "j"],
                  rows: [nativeValues],
                }),
              );
              break;
            case "QueryWithParamsNative":
              sock.write(
                encode({
                  type: "ResultScalarNative",
                  value: { type: "int", value: 9007199254740993n },
                }),
              );
              break;
            case "QuerySqlNative":
              sock.write(
                encode({
                  type: "ResultScalarNative",
                  value: { type: "datetime", value: 1723650123456789n },
                }),
              );
              break;
            case "Query":
            case "QueryWithParams":
            case "QuerySql":
              legacyRequests++;
              break;
            case "Disconnect":
              sock.end();
              break;
          }
        }
      });
      sock.on("error", () => {});
    });
    const port = await listen(server);
    const client = await Client.connect({ host: "127.0.0.1", port });

    const rows = await client.queryNative("T");
    assert.equal(rows.kind, "rows");
    if (rows.kind === "rows") {
      assert.deepStrictEqual(rows.rows[0], [
        null,
        -9007199254740993n,
        2.5,
        true,
        "héllo",
        1723650123456789n,
        "00112233-4455-6677-8899-aabbccddeeff",
        Uint8Array.from([0x00, 0x7f, 0x80, 0xff]),
        9007199254740993n,
      ]);
    }
    const parameterized = await client.queryNative("T filter .x = $1", [7]);
    assert.deepStrictEqual(parameterized, {
      kind: "scalar",
      value: 9007199254740993n,
    });
    const sql = await client.querySqlNative("SELECT x FROM T");
    assert.deepStrictEqual(sql, {
      kind: "scalar",
      value: 1723650123456789n,
    });
    assert.equal(legacyRequests, 0);

    await client.close();
    await new Promise<void>((resolve) => server.close(() => resolve()));
  });

  console.log("\nCancellation — abort during in-flight query");

  await test("AbortSignal rejects the pending query without destroying the socket", async () => {
    // Tiny server: speaks the handshake (ConnectOk) then stays silent so
    // any Query sent will hang until we abort it.
    const connected = new Promise<void>((resolveConn) => {
      const server = net.createServer((sock) => {
        // As soon as the client sends Connect, reply with ConnectOk.
        sock.once("data", () => {
          sock.write(encode({ type: "ConnectOk", version: "0.2.0" }));
        });
        // Hold the connection open; never reply to queries.
        sock.on("error", () => {});
      });
      server.listen(0, "127.0.0.1", () => {
        const addr = server.address();
        if (!addr || typeof addr === "string") {
          throw new Error("unexpected server address");
        }
        // Stash port on the closure for the outer scope.
        resolveConn();
        (globalThis as unknown as { __port: number }).__port = addr.port;
        (globalThis as unknown as { __server: net.Server }).__server = server;
      });
    });
    await connected;
    const port = (globalThis as unknown as { __port: number }).__port;
    const server = (globalThis as unknown as { __server: net.Server }).__server;

    const client = await Client.connect({
      host: "127.0.0.1",
      port,
      connectTimeoutMs: 1000,
    });

    const controller = new AbortController();
    // Fire the abort on the next microtask — after the query is enqueued
    // but before the (non-existent) server reply arrives.
    queueMicrotask(() => controller.abort());

    let rejected = false;
    try {
      await client.query("anything", { signal: controller.signal });
    } catch (err) {
      rejected = true;
      const e = err as { name?: string; message?: string };
      assert.ok(
        e.name === "AbortError" ||
          /abort/i.test(e.message ?? "") ||
          /aborted/i.test(e.message ?? ""),
        `expected AbortError-like rejection, got ${e.name}: ${e.message}`,
      );
    }
    assert.ok(rejected, "query() should have rejected on abort");

    // Socket should NOT have been destroyed by the abort — we can still
    // close cleanly.
    await client.close();
    await new Promise<void>((r) => server.close(() => r()));
  });

  await test("AbortSignal that is already aborted rejects immediately", async () => {
    const connected = new Promise<void>((resolveConn) => {
      const server = net.createServer((sock) => {
        sock.once("data", () => {
          sock.write(encode({ type: "ConnectOk", version: "0.2.0" }));
        });
        sock.on("error", () => {});
      });
      server.listen(0, "127.0.0.1", () => {
        const addr = server.address();
        if (!addr || typeof addr === "string") {
          throw new Error("unexpected server address");
        }
        resolveConn();
        (globalThis as unknown as { __port2: number }).__port2 = addr.port;
        (globalThis as unknown as { __server2: net.Server }).__server2 =
          server;
      });
    });
    await connected;
    const port = (globalThis as unknown as { __port2: number }).__port2;
    const server = (globalThis as unknown as { __server2: net.Server })
      .__server2;

    const client = await Client.connect({
      host: "127.0.0.1",
      port,
      connectTimeoutMs: 1000,
    });

    const controller = new AbortController();
    controller.abort(new Error("nope"));

    let rejected = false;
    try {
      await client.query("anything", { signal: controller.signal });
    } catch (err) {
      rejected = true;
      assert.equal((err as Error).message, "nope");
    }
    assert.ok(rejected, "pre-aborted signal should reject immediately");

    await client.close();
    await new Promise<void>((r) => server.close(() => r()));
  });

  await test("abort of in-flight query does not desync — next query gets its own reply", async () => {
    const server = echoServer(20);
    const port = await listen(server);
    const client = await Client.connect({
      host: "127.0.0.1",
      port,
      connectTimeoutMs: 1000,
    });

    const controller = new AbortController();
    const aborted = client.query("count(User)", { signal: controller.signal });
    queueMicrotask(() => controller.abort());
    await assert.rejects(aborted, /abort/i);

    // The aborted query's reply is still in flight. The next query must get
    // ITS OWN result, not the aborted one's.
    const result = await client.query("User { .name }");
    assert.equal(result.kind, "scalar");
    if (result.kind === "scalar") {
      assert.equal(result.value, "User { .name }");
    }

    // Connection is still usable for a third query.
    const again = await client.query("count(User)");
    assert.equal(again.kind === "scalar" && again.value, "count(User)");

    await client.close();
    await new Promise<void>((r) => server.close(() => r()));
  });

  await test("abort with nothing else pending keeps the connection open (no protocol_error)", async () => {
    const server = echoServer(20);
    const port = await listen(server);
    const client = await Client.connect({
      host: "127.0.0.1",
      port,
      connectTimeoutMs: 1000,
    });

    const controller = new AbortController();
    const aborted = client.query("count(User)", { signal: controller.signal });
    queueMicrotask(() => controller.abort());
    await assert.rejects(aborted, /abort/i);

    // Let the orphaned reply arrive and be dropped. If it were treated as an
    // unsolicited frame, the client would tear the connection down.
    await new Promise((r) => setTimeout(r, 40));

    const result = await client.query("User { .name }");
    assert.equal(result.kind, "scalar");

    // close() must resolve, not hang.
    await Promise.race([
      client.close(),
      new Promise<void>((_, rej) =>
        setTimeout(() => rej(new Error("close() hung")), 2000),
      ),
    ]);
    await new Promise<void>((r) => server.close(() => r()));
  });

  await test("plain abort() rejects with PowDBError code 'aborted'", async () => {
    const server = silentServer();
    const port = await listen(server);
    const client = await Client.connect({
      host: "127.0.0.1",
      port,
      connectTimeoutMs: 1000,
    });

    const controller = new AbortController();
    queueMicrotask(() => controller.abort());
    let caught: unknown;
    try {
      await client.query("anything", { signal: controller.signal });
    } catch (err) {
      caught = err;
    }
    assert.ok(isPowDBError(caught), "expected a PowDBError, got " + String(caught));
    assert.equal((caught as PowDBError).code, "aborted");

    await client.close();
    await new Promise<void>((r) => server.close(() => r()));
  });

  await test("custom abort reason passes through unchanged", async () => {
    const server = silentServer();
    const port = await listen(server);
    const client = await Client.connect({
      host: "127.0.0.1",
      port,
      connectTimeoutMs: 1000,
    });

    const controller = new AbortController();
    const custom = new Error("my custom reason");
    queueMicrotask(() => controller.abort(custom));
    let caught: unknown;
    try {
      await client.query("anything", { signal: controller.signal });
    } catch (err) {
      caught = err;
    }
    assert.equal(caught, custom);

    await client.close();
    await new Promise<void>((r) => server.close(() => r()));
  });

  await test("close() after an errored teardown releases the socket", async () => {
    let serverSock: net.Socket | undefined;
    const server = net.createServer((sock) => {
      serverSock = sock;
      sock.once("data", () =>
        sock.write(encode({ type: "ConnectOk", version: "0.8.0" })),
      );
      sock.on("error", () => {});
    });
    const port = await listen(server);
    const client = await Client.connect({
      host: "127.0.0.1",
      port,
      connectTimeoutMs: 1000,
    });

    const tornDown = new Promise<Error | null>((r) =>
      client.once("close", (e) => r(e.error)),
    );
    // An unsolicited frame with nothing pending tears the client down
    // (protocol_error) without destroying the socket.
    serverSock!.write(encode({ type: "ResultScalar", value: "unsolicited" }));
    const err = await tornDown;
    assert.ok(isPowDBError(err) && err.code === "protocol_error");

    // close() must release the socket — observed as the server seeing the
    // connection close — rather than leaving it holding the event loop open.
    await client.close();
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error("server never saw the socket close")),
        2000,
      );
      serverSock!.once("close", () => {
        clearTimeout(timer);
        resolve();
      });
    });
    await new Promise<void>((r) => server.close(() => r()));
  });

  console.log("\nError frames: trailing class byte");

  // Build a raw MSG_ERROR (0x0a) frame: length-prefixed message string,
  // optionally followed by one class byte (0.17+ servers).
  function buildErrorFrame(message: string, errorClass?: number): Buffer {
    const msgBytes = Buffer.from(message, "utf8");
    const parts = [Buffer.alloc(4), msgBytes];
    parts[0]!.writeUInt32LE(msgBytes.length, 0);
    if (errorClass !== undefined) {
      parts.push(Buffer.from([errorClass]));
    }
    const payload = Buffer.concat(parts);
    const header = Buffer.alloc(6);
    header.writeUInt8(0x0a, 0);
    header.writeUInt32LE(payload.length, 2);
    return Buffer.concat([header, payload]);
  }

  await test("decodes the class byte from a new-server Error frame", () => {
    const frame = buildErrorFrame("query timeout after 75ms", 3);
    const result = tryDecode(frame);
    assert.ok(result !== null);
    assert.equal(result.msg.type, "Error");
    if (result.msg.type !== "Error") throw new Error("unreachable");
    assert.equal(result.msg.message, "query timeout after 75ms");
    assert.equal(result.msg.errorClass, 3);
  });

  await test("tolerates a legacy Error frame with no class byte", () => {
    const frame = buildErrorFrame("table 'users' not found");
    const result = tryDecode(frame);
    assert.ok(result !== null);
    assert.equal(result.msg.type, "Error");
    if (result.msg.type !== "Error") throw new Error("unreachable");
    assert.equal(result.msg.message, "table 'users' not found");
    assert.equal(result.msg.errorClass, undefined);
  });

  await test("carries an unknown future class byte through unchanged", () => {
    const frame = buildErrorFrame("some future error", 200);
    const result = tryDecode(frame);
    assert.ok(result !== null && result.msg.type === "Error");
    if (result.msg.type !== "Error") throw new Error("unreachable");
    assert.equal(result.msg.errorClass, 200);
  });

  await test("class byte does not disturb frame length accounting", () => {
    const classed = buildErrorFrame("boom", 2);
    const legacy = buildErrorFrame("boom");
    const decodedClassed = tryDecode(classed);
    const decodedLegacy = tryDecode(legacy);
    assert.ok(decodedClassed !== null && decodedLegacy !== null);
    assert.equal(decodedClassed.consumed, classed.length);
    assert.equal(decodedLegacy.consumed, legacy.length);
    assert.equal(decodedClassed.consumed, decodedLegacy.consumed + 1);
  });

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
