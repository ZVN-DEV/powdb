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
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import {
  tryDecode,
  encode,
  MAX_PAYLOAD_SIZE,
  MAX_COLUMNS,
  MAX_ROWS,
  MAX_RESULT_CELLS,
  MAX_SYNC_UNITS,
  MAX_SYNC_PULL_UNITS,
  MAX_PARAMS,
  MSG_QUERY_NATIVE,
  MSG_QUERY_PARAMS_NATIVE,
  MSG_QUERY_SQL_NATIVE,
  MSG_RESULT_ROWS,
  MSG_RESULT_ROWS_NATIVE,
  MSG_RESULT_SCALAR_NATIVE,
  type Message,
  type WireValue,
  type WireSyncStatus,
} from "../src/protocol.js";
import {
  Client,
  Pool,
  PowDBError,
  DEFAULT_MAX_IN_FLIGHT,
  MAX_IN_FLIGHT_BYTES,
  isPowDBError,
  assertServerCatalogVersionSupported,
  serverCapabilityMismatch,
  CLIENT_CAPABILITIES,
  SUPPORTED_CATALOG_VERSION,
  legacyServerHello,
  PROTOCOL_VERSION_LEGACY,
  PROTOCOL_VERSION_NEGOTIATED,
  WIRE_FEATURE,
  type ClientHello,
  type ServerHello,
  type WireValue as PublicWireValue,
} from "../src/index.js";
import { WIRE_ERROR_CLASS } from "../src/errors.js";

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

/**
 * A mock PowDB server for handshake tests.
 *
 * `answer` decides what to reply to the Connect frame, so one helper covers
 * a pre-0.22.0 server (a bare ConnectOk), a negotiating server, and a server
 * that refuses on version grounds. Every frame the client sends is recorded,
 * which is how the tests prove a refusal happened during the handshake and
 * not after a query had already gone out.
 */
function handshakeServer(answer: (connect: Message) => Message | null): {
  server: net.Server;
  received: Message[];
} {
  const received: Message[] = [];
  const server = net.createServer((sock) => {
    let scratch = Buffer.alloc(0);
    sock.on("data", (chunk) => {
      const collected = collectFrames(scratch, Buffer.from(chunk));
      scratch = collected.rest;
      for (const msg of collected.messages) {
        received.push(msg);
        if (msg.type === "Connect") {
          const reply = answer(msg);
          if (reply === null) {
            sock.destroy();
            return;
          }
          sock.write(encode(reply));
        } else if (msg.type === "Query") {
          sock.write(encode({ type: "ResultScalar", value: "1" }));
        } else if (msg.type === "Disconnect") {
          sock.end();
        }
      }
    });
    sock.on("error", () => {});
  });
  return { server, received };
}

/** The ConnectOk a pre-0.22.0 server writes: a version string, nothing else. */
function legacyConnectOk(): Message {
  return { type: "ConnectOk", version: "0.21.0" };
}

function negotiatedConnectOk(
  overrides: Partial<ServerHello> = {},
): Message {
  return {
    type: "ConnectOk",
    version: "0.22.0",
    hello: {
      protocol: PROTOCOL_VERSION_NEGOTIATED,
      minProtocol: PROTOCOL_VERSION_LEGACY,
      maxProtocol: PROTOCOL_VERSION_NEGOTIATED,
      catalogVersion: SUPPORTED_CATALOG_VERSION,
      features: [...CLIENT_CAPABILITIES.features],
      ...overrides,
    },
  };
}

/** Connect against `port`, expecting the handshake to fail; return the error. */
async function expectHandshakeFailure(
  port: number,
  opts: Partial<Parameters<typeof Client.connect>[0]> = {},
): Promise<PowDBError> {
  try {
    const client = await Client.connect({
      host: "127.0.0.1",
      port,
      ...opts,
    });
    await client.close();
    throw new Error("expected the handshake to fail");
  } catch (err) {
    if (!isPowDBError(err)) throw err;
    return err;
  }
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

/**
 * Handshakes with a negotiating ConnectOk, then answers every request frame
 * with `ResultOk`. Records each request and the high-water mark of frames it
 * has received but not yet answered, which is how the in-flight window tests
 * prove the client stops writing past its cap.
 */
function queryServer(replyDelayMs = 0): {
  server: net.Server;
  received: Message[];
  stats: {
    inFlight: number;
    maxInFlight: number;
    inFlightBytes: number;
    maxInFlightBytes: number;
  };
  sockets: Set<net.Socket>;
} {
  const received: Message[] = [];
  // Bytes as well as frames: the server's read-ahead budget has both caps and
  // the byte one is the tighter of the two for anything but a tiny query.
  const stats = {
    inFlight: 0,
    maxInFlight: 0,
    inFlightBytes: 0,
    maxInFlightBytes: 0,
  };
  const sockets = new Set<net.Socket>();
  const server = net.createServer((sock) => {
    sockets.add(sock);
    sock.on("close", () => sockets.delete(sock));
    let scratch = Buffer.alloc(0);
    sock.on("data", (chunk) => {
      let collected;
      try {
        collected = collectFrames(scratch, Buffer.from(chunk));
      } catch {
        // A frame this server refuses to decode is a frame the real server
        // would refuse too: drop the connection rather than kill the harness.
        sock.destroy();
        return;
      }
      scratch = collected.rest;
      for (const msg of collected.messages) {
        received.push(msg);
        if (msg.type === "Connect") {
          sock.write(encode(negotiatedConnectOk()));
          continue;
        }
        if (msg.type === "Disconnect") {
          sock.end();
          continue;
        }
        stats.inFlight++;
        stats.maxInFlight = Math.max(stats.maxInFlight, stats.inFlight);
        const wireLen = encode(msg).length;
        stats.inFlightBytes += wireLen;
        stats.maxInFlightBytes = Math.max(
          stats.maxInFlightBytes,
          stats.inFlightBytes,
        );
        const answer = () => {
          stats.inFlight--;
          stats.inFlightBytes -= wireLen;
          if (!sock.destroyed) {
            sock.write(encode({ type: "ResultOk", affected: 0n }));
          }
        };
        if (replyDelayMs > 0) setTimeout(answer, replyDelayMs);
        else answer();
      }
    });
    sock.on("error", () => {});
  });
  return { server, received, stats, sockets };
}

function listenOn(server: net.Server, port: number): Promise<void> {
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", () => resolve());
  });
}

function closeServer(
  server: net.Server,
  sockets?: Set<net.Socket>,
): Promise<void> {
  return new Promise((resolve) => {
    if (sockets !== undefined) {
      for (const sock of [...sockets]) sock.destroy();
    }
    server.close(() => resolve());
  });
}

function withTimeout<T>(promise: Promise<T>, ms: number): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`timed out after ${ms}ms`)),
      ms,
    );
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (err) => {
        clearTimeout(timer);
        reject(err);
      },
    );
  });
}

function nextCloseEvent(
  client: Client,
  ms: number,
): Promise<{ error: Error | null }> {
  return withTimeout(
    new Promise<{ error: Error | null }>((resolve) => {
      client.once("close", resolve);
    }),
    ms,
  );
}

/** Let queued socket close events run before asserting on pool bookkeeping. */
function settleCloseEvents(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 50));
}

/**
 * A well-framed `ResultRows` frame declaring more cells than the client is
 * willing to materialize. The row bytes are all empty-string length prefixes,
 * so the frame is only as large as the shape check requires.
 */
function oversizedResultRowsFrame(rows: number, cols: number): Buffer {
  const names: Buffer[] = [];
  for (let c = 0; c < cols; c++) {
    const name = Buffer.from(`c${c}`, "utf8");
    const len = Buffer.alloc(4);
    len.writeUInt32LE(name.length, 0);
    names.push(len, name);
  }
  const header = Buffer.alloc(2);
  header.writeUInt16LE(cols, 0);
  const rowCount = Buffer.alloc(4);
  rowCount.writeUInt32LE(rows, 0);
  const cells = Buffer.alloc(rows * cols * 4);
  const payload = Buffer.concat([header, ...names, rowCount, cells]);
  const frame = Buffer.alloc(6 + payload.length);
  frame.writeUInt8(MSG_RESULT_ROWS, 0);
  frame.writeUInt8(0, 1);
  frame.writeUInt32LE(payload.length, 2);
  payload.copy(frame, 6);
  return frame;
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

  await test("the sync unit ceilings, and why they differ, are the server's own", () => {
    const rustConstant = (file: string, decl: string, name: string): number => {
      const path = fileURLToPath(new URL(`../../../crates/server/src/${file}`, import.meta.url));
      const text = readFileSync(path, "utf8");
      const matches = [
        ...text.matchAll(new RegExp(`^${decl} ${name}: u\\w+ = ([^;]+);$`, "gm")),
      ];
      assert.equal(matches.length, 1, `expected exactly one \`${name}\` in ${path}`);
      const expr = matches[0]![1]!.replace(/_/g, "").trim();
      assert.match(expr, /^\d+( \* \d+)*$/, `cannot evaluate ${name} = ${expr}`);
      return expr.split("*").reduce((acc, part) => acc * Number(part.trim()), 1);
    };

    // What this client DECODES is the server's decoder ceiling, and what it may
    // ASK FOR is the server's serving cap. They are deliberately different
    // numbers, so drift in either direction has to be caught here.
    assert.equal(
      MAX_SYNC_UNITS,
      rustConstant("protocol.rs", "const", "MAX_SYNC_UNITS"),
      "MAX_SYNC_UNITS has drifted from the server's decoder ceiling",
    );
    assert.equal(
      MAX_SYNC_PULL_UNITS,
      rustConstant("handler/sync.rs", "pub\\(super\\) const", "MAX_SYNC_PULL_UNITS"),
      "MAX_SYNC_PULL_UNITS has drifted from the server's serving cap",
    );

    // The doc comment on the decoder ceiling is the only place a reader is told
    // why the two differ, so it has to state the reason that holds. It used to
    // say the server extends a chunk past the serving cap to the commit that
    // closes a transaction, so a transaction of any size arrived whole. It does
    // not: a served chunk is capped at MAX_SYNC_PULL_UNITS because every
    // released decoder through v0.27.0 refuses more, and a transaction that
    // does not fit is answered with a typed rebootstrap.
    const ownSource = readFileSync(
      fileURLToPath(new URL("../src/protocol.ts", import.meta.url)),
      "utf8",
    );
    const docAbove = (decl: string): string => {
      const at = ownSource.indexOf(decl);
      assert.notEqual(at, -1, `${decl} is no longer in src/protocol.ts`);
      return ownSource
        .slice(0, at)
        .split("/**")
        .pop()!
        .replace(/^\s*\*/gm, " ")
        .replace(/\s+/g, " ");
    };
    for (const decl of [
      "export const MAX_SYNC_UNITS",
      "export const MAX_SYNC_PULL_UNITS",
    ]) {
      for (const retired of [
        /extends? a chunk past/i,
        /transaction of any size/i,
        /arrives whole/i,
      ]) {
        assert.doesNotMatch(
          docAbove(decl),
          retired,
          `the ${decl} comment still describes the pull window 97cf06b capped`,
        );
      }
    }
    assert.match(
      docAbove("export const MAX_SYNC_PULL_UNITS"),
      /rebootstrap/i,
      "the MAX_SYNC_PULL_UNITS comment does not say what happens to a " +
        "transaction that does not fit the serving cap",
    );
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
      assert.ok(isPowDBError(err), `rejected with ${err}, not a PowDBError`);
      assert.equal((err as PowDBError).code, "aborted");
      assert.match((err as PowDBError).message, /nope/);
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

  await test("a custom abort reason is wrapped, keeping the reason as cause", async () => {
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
    assert.ok(isPowDBError(caught), `rejected with ${caught}, not a PowDBError`);
    assert.equal((caught as PowDBError).code, "aborted");
    assert.match((caught as PowDBError).message, /my custom reason/);
    assert.equal((caught as PowDBError).cause, custom);

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

  // ─── Wire protocol version negotiation ──────────────────────────────────

  await test("Connect hello round-trips through encode/decode", () => {
    const hello: ClientHello = {
      minProtocol: 1,
      maxProtocol: 2,
      catalogVersion: 7,
      features: [WIRE_FEATURE.sql, WIRE_FEATURE.nativeTyped],
    };
    const frame = encode({
      type: "Connect",
      dbName: "mydb",
      password: "pw",
      username: "alice",
      hello,
    });
    assert.equal(frame[0], 0x01, "hello must reuse the Connect tag");
    const decoded = tryDecode(frame);
    assert.ok(decoded !== null && decoded.msg.type === "Connect");
    assert.equal(decoded.msg.dbName, "mydb");
    assert.equal(decoded.msg.username, "alice");
    assert.deepEqual(decoded.msg.hello, hello);
  });

  await test("Connect hello survives a null username", () => {
    // The username field cannot be elided once a hello follows it.
    const frame = encode({
      type: "Connect",
      dbName: "d",
      password: null,
      username: null,
      hello: {
        minProtocol: 1,
        maxProtocol: 2,
        catalogVersion: 7,
        features: [],
      },
    });
    const decoded = tryDecode(frame);
    assert.ok(decoded !== null && decoded.msg.type === "Connect");
    assert.equal(decoded.msg.username, null);
    assert.equal(decoded.msg.hello?.maxProtocol, 2);
  });

  await test("a hello-less Connect frame is byte-identical to the 0.21 shape", () => {
    const withHello = encode({
      type: "Connect",
      dbName: "d",
      password: null,
      username: null,
      hello: {
        minProtocol: 1,
        maxProtocol: 2,
        catalogVersion: 7,
        features: [],
      },
    });
    const legacy = encode({
      type: "Connect",
      dbName: "d",
      password: null,
      username: null,
    });
    // 6-byte header + dbName("d") + zero-length password = 6 + 5 + 4.
    assert.equal(legacy.length, 15);
    assert.ok(withHello.length > legacy.length);
    const decoded = tryDecode(legacy);
    assert.ok(decoded !== null && decoded.msg.type === "Connect");
    assert.equal(decoded.msg.hello, undefined);
  });

  await test("ConnectOk hello round-trips and legacy decode ignores it", () => {
    const frame = encode(negotiatedConnectOk());
    assert.equal(frame[0], 0x02);
    const decoded = tryDecode(frame);
    assert.ok(decoded !== null && decoded.msg.type === "ConnectOk");
    assert.equal(decoded.msg.version, "0.22.0");
    assert.equal(decoded.msg.hello?.protocol, PROTOCOL_VERSION_NEGOTIATED);
    assert.deepEqual(
      decoded.msg.hello?.features,
      [...CLIENT_CAPABILITIES.features],
    );

    const bare = tryDecode(encode(legacyConnectOk()));
    assert.ok(bare !== null && bare.msg.type === "ConnectOk");
    assert.equal(bare.msg.hello, undefined);
  });

  await test("hello blocks skip trailing bytes so a later release can extend them", () => {
    // The whole point of the design: adding a field later must not require
    // another breaking handshake change.
    const base = encode(negotiatedConnectOk());
    const extended = Buffer.concat([base, Buffer.from([1, 2, 3, 4, 5])]);
    extended.writeUInt32LE(extended.length - 6, 2);
    const decoded = tryDecode(extended);
    assert.ok(decoded !== null && decoded.msg.type === "ConnectOk");
    assert.equal(decoded.msg.hello?.protocol, PROTOCOL_VERSION_NEGOTIATED);
    assert.equal(decoded.consumed, extended.length);
  });

  await test("a hello with the wrong magic is rejected, not misread", () => {
    // The block is otherwise perfectly well-formed, so the magic check is the
    // only thing that can reject it. Building a short payload instead would
    // fail on truncation and never exercise the magic at all.
    const wellFormed = encode(negotiatedConnectOk());
    const magicAt = 6 + 4 + Buffer.byteLength("0.22.0", "utf8");
    assert.equal(wellFormed.readUInt32LE(magicAt), 0x50574831);
    const wrongMagic = Buffer.from(wellFormed);
    wrongMagic.writeUInt32LE(0xdeadbeef, magicAt);
    assert.throws(() => tryDecode(wrongMagic), /malformed ConnectOk hello block/);
    // The control: identical bytes with the right magic decode fine.
    assert.ok(tryDecode(wellFormed) !== null);
  });

  await test("serverCapabilityMismatch covers version, features and catalog", () => {
    const modern: ServerHello = {
      protocol: 2,
      minProtocol: 1,
      maxProtocol: 2,
      catalogVersion: 7,
      features: [WIRE_FEATURE.sql],
    };
    assert.equal(serverCapabilityMismatch(modern, 2, [WIRE_FEATURE.sql], 7), null);
    assert.match(
      serverCapabilityMismatch(legacyServerHello(), 2, [], 7) ?? "",
      /upgrade the server/,
    );
    assert.match(
      serverCapabilityMismatch(modern, 1, [WIRE_FEATURE.nativeTyped], 7) ?? "",
      /native-typed/,
    );
    assert.match(
      serverCapabilityMismatch(modern, 1, [], 6) ?? "",
      /upgrade the client/,
    );
    // A server that stated no catalog version is not judged on one.
    assert.equal(serverCapabilityMismatch(legacyServerHello(), 1, [], 7), null);
  });

  await test("SUPPORTED_CATALOG_VERSION is derived from CLIENT_CAPABILITIES", () => {
    assert.equal(SUPPORTED_CATALOG_VERSION, CLIENT_CAPABILITIES.catalogVersion);
    // And the handshake applies the same ceiling the standalone helper does.
    assert.throws(
      () =>
        assertServerCatalogVersionSupported(
          CLIENT_CAPABILITIES.catalogVersion + 1,
        ),
      /upgrade the client/,
    );
    assert.match(
      serverCapabilityMismatch(
        {
          protocol: 2,
          minProtocol: 1,
          maxProtocol: 2,
          catalogVersion: CLIENT_CAPABILITIES.catalogVersion + 1,
          features: [],
        },
        1,
        [],
        CLIENT_CAPABILITIES.catalogVersion,
      ) ?? "",
      /upgrade the client/,
    );
  });

  await test("the client states its capabilities in the Connect frame", async () => {
    const { server, received } = handshakeServer(() => negotiatedConnectOk());
    const port = await listen(server);
    try {
      const client = await Client.connect({ host: "127.0.0.1", port });
      const connect = received[0];
      assert.ok(connect !== undefined && connect.type === "Connect");
      assert.deepEqual(connect.hello, {
        minProtocol: CLIENT_CAPABILITIES.minProtocolVersion,
        maxProtocol: CLIENT_CAPABILITIES.maxProtocolVersion,
        catalogVersion: CLIENT_CAPABILITIES.catalogVersion,
        features: [...CLIENT_CAPABILITIES.features],
      });
      assert.equal(client.protocolVersion, PROTOCOL_VERSION_NEGOTIATED);
      assert.equal(client.hasFeature(WIRE_FEATURE.sql), true);
      assert.equal(client.hasFeature("not-a-feature"), false);
      await client.close();
    } finally {
      server.close();
    }
  });

  await test("legacyHandshake sends the pre-0.22.0 frame with no hello", async () => {
    const { server, received } = handshakeServer(() => legacyConnectOk());
    const port = await listen(server);
    try {
      const client = await Client.connect({
        host: "127.0.0.1",
        port,
        legacyHandshake: true,
      });
      const connect = received[0];
      assert.ok(connect !== undefined && connect.type === "Connect");
      assert.equal(connect.hello, undefined);
      await client.close();
    } finally {
      server.close();
    }
  });

  await test("a new client still connects to a pre-0.22.0 server", async () => {
    // Backward compatibility: no hello in the reply means protocol v1 and no
    // named features, which the default requirements accept.
    const { server } = handshakeServer(() => legacyConnectOk());
    const port = await listen(server);
    try {
      const client = await Client.connect({ host: "127.0.0.1", port });
      assert.equal(client.protocolVersion, PROTOCOL_VERSION_LEGACY);
      assert.equal(client.serverVersion, "0.21.0");
      assert.equal(client.hasFeature(WIRE_FEATURE.sql), false);
      await client.close();
    } finally {
      server.close();
    }
  });

  await test("a new client requiring v2 fails against an old server AT the handshake", async () => {
    const { server, received } = handshakeServer(() => legacyConnectOk());
    const port = await listen(server);
    try {
      const err = await expectHandshakeFailure(port, {
        requireProtocolVersion: PROTOCOL_VERSION_NEGOTIATED,
      });
      assert.equal(err.code, "protocol_version");
      assert.match(err.message, /upgrade the server/);
      // The refusal happened during the handshake: no query frame was ever
      // written, so no mismatch could surface mid-session.
      assert.deepEqual(
        received.map((m) => m.type),
        ["Connect"],
      );
    } finally {
      server.close();
    }
  });

  await test("a required feature an old server cannot name fails at the handshake", async () => {
    const { server } = handshakeServer(() => legacyConnectOk());
    const port = await listen(server);
    try {
      const err = await expectHandshakeFailure(port, {
        requireFeatures: [WIRE_FEATURE.nativeTyped],
      });
      assert.equal(err.code, "protocol_version");
      assert.match(err.message, /native-typed/);
    } finally {
      server.close();
    }
  });

  await test("a server catalog newer than the client fails at the handshake", async () => {
    const { server } = handshakeServer(() =>
      negotiatedConnectOk({
        catalogVersion: CLIENT_CAPABILITIES.catalogVersion + 1,
      }),
    );
    const port = await listen(server);
    try {
      const err = await expectHandshakeFailure(port);
      assert.equal(err.code, "protocol_version");
      assert.match(err.message, /upgrade the client/);
    } finally {
      server.close();
    }
  });

  await test("an old client refused by a new server gets a typed version error", async () => {
    // The mirror direction: the server decides the ranges do not overlap and
    // answers with the ProtocolVersion class instead of ConnectOk.
    const { server, received } = handshakeServer(() => ({
      type: "Error",
      message:
        "unsupported wire protocol: client speaks up to v1, this server requires at least v3; upgrade the client",
      errorClass: WIRE_ERROR_CLASS.protocol_version,
    }));
    const port = await listen(server);
    try {
      const err = await expectHandshakeFailure(port);
      assert.equal(
        err.code,
        "protocol_version",
        "a version refusal must not be reported as auth_failed",
      );
      assert.equal(err.wireErrorClass, WIRE_ERROR_CLASS.protocol_version);
      assert.match(err.message, /upgrade the client/);
      assert.deepEqual(
        received.map((m) => m.type),
        ["Connect"],
      );
    } finally {
      server.close();
    }
  });

  await test("Error frames round-trip their class byte, and omit it when absent", () => {
    const classed = tryDecode(
      encode({
        type: "Error",
        message: "boom",
        errorClass: WIRE_ERROR_CLASS.protocol_version,
      }),
    );
    assert.ok(classed !== null && classed.msg.type === "Error");
    assert.equal(classed.msg.message, "boom");
    assert.equal(classed.msg.errorClass, WIRE_ERROR_CLASS.protocol_version);

    const bare = encode({ type: "Error", message: "boom" });
    const decodedBare = tryDecode(bare);
    assert.ok(decodedBare !== null && decodedBare.msg.type === "Error");
    assert.equal(decodedBare.msg.errorClass, undefined);
    // Byte-identical to the pre-class frame: one byte shorter, same prefix.
    const classedFrame = encode({
      type: "Error",
      message: "boom",
      errorClass: WIRE_ERROR_CLASS.protocol_version,
    });
    assert.equal(classedFrame.length, bare.length + 1);
    assert.deepEqual(classedFrame.subarray(6, bare.length), bare.subarray(6));
  });

  await test("the pool does not retry a version refusal", async () => {
    // Retrying a version mismatch can only fail the same way. One attempt.
    const { server, received } = handshakeServer(() => legacyConnectOk());
    const port = await listen(server);
    const pool = new Pool({
      host: "127.0.0.1",
      port,
      requireProtocolVersion: PROTOCOL_VERSION_NEGOTIATED,
      connectRetries: 3,
      connectBackoffMs: 1,
    });
    try {
      await assert.rejects(
        () => pool.acquire(),
        (err: unknown) =>
          isPowDBError(err) && err.code === "protocol_version",
      );
      assert.equal(
        received.filter((m) => m.type === "Connect").length,
        1,
        "a version refusal must not be retried",
      );
    } finally {
      await pool.close();
      server.close();
    }
  });

  await test("a non-version Connect refusal is still auth_failed", async () => {
    const { server } = handshakeServer(() => ({
      type: "Error",
      message: "authentication failed",
      errorClass: WIRE_ERROR_CLASS.auth_failed,
    }));
    const port = await listen(server);
    try {
      const err = await expectHandshakeFailure(port);
      assert.equal(err.code, "auth_failed");
    } finally {
      server.close();
    }
  });

  console.log("\nCross-language wire conformance");

  // These read crates/server/tests/wire_vectors/handshake.txt: bytes generated
  // by the Rust encoder, checked here against the TypeScript one. This module
  // is a hand-written mirror of crates/server/src/protocol.rs and nothing else
  // forces the two to agree — each side round-trips its own bytes happily, so
  // a mirror can drift into a layout that is self-consistent, passes every
  // test in this file, and cannot talk to a real server. Swapping two u16
  // fields in both the encoder and the decoder below is exactly that failure.
  //
  // Regenerate the vectors (after an intentional wire change) with:
  //   POWDB_UPDATE_WIRE_VECTORS=1 cargo test -p powdb-server --test wire_conformance

  /** Records of one kind from the shared vector file, in file order. */
  function vectorRecords(kind: string): Array<[string, string]> {
    const path = fileURLToPath(
      new URL(
        "../../../crates/server/tests/wire_vectors/handshake.txt",
        import.meta.url,
      ),
    );
    const text = readFileSync(path, "utf8");
    const out: Array<[string, string]> = [];
    for (const line of text.split("\n")) {
      if (line.startsWith("#") || line.trim() === "") continue;
      const parts = line.trim().split(/\s+/);
      if (parts[0] !== kind) continue;
      out.push([parts[1]!, parts.slice(2).join(" ")]);
    }
    return out;
  }

  /**
   * What each named frame must decode to. A vector with no entry here fails
   * loudly rather than being skipped: a frame the Rust side added and this
   * client has not accounted for is precisely the drift being guarded against.
   */
  const expectedFrames: Record<string, Message> = {
    connect_legacy_password_only: {
      type: "Connect",
      dbName: "mydb",
      password: "pw",
      username: null,
    },
    connect_legacy_password_and_username: {
      type: "Connect",
      dbName: "mydb",
      password: "pw",
      username: "alice",
    },
    connect_hello_no_credentials: {
      type: "Connect",
      dbName: "mydb",
      password: null,
      username: null,
      hello: {
        minProtocol: 1,
        maxProtocol: 2,
        catalogVersion: 7,
        features: [],
      },
    },
    connect_hello_full: {
      type: "Connect",
      dbName: "mydb",
      password: "pw",
      username: "alice",
      hello: {
        minProtocol: 1,
        maxProtocol: 2,
        catalogVersion: 7,
        features: ["params", "sql"],
      },
    },
    connect_ok_legacy: { type: "ConnectOk", version: "0.21.0" },
    connect_ok_hello: {
      type: "ConnectOk",
      version: "0.22.0",
      hello: {
        protocol: 2,
        minProtocol: 1,
        maxProtocol: 2,
        catalogVersion: 7,
        features: ["params", "sql"],
      },
    },
    connect_ok_hello_no_features: {
      type: "ConnectOk",
      version: "0.22.0",
      hello: {
        protocol: 1,
        minProtocol: 1,
        maxProtocol: 2,
        catalogVersion: 7,
        features: [],
      },
    },
    error_protocol_version: {
      type: "Error",
      message: "unsupported wire protocol",
      errorClass: WIRE_ERROR_CLASS.protocol_version,
    },
  };

  await test("every Rust-generated frame decodes to the expected fields", () => {
    const vectors = vectorRecords("frame");
    assert.ok(vectors.length > 0, "vector file must contain frames");
    for (const [name, hex] of vectors) {
      const expected = expectedFrames[name];
      assert.ok(
        expected !== undefined,
        `wire vector '${name}' has no expected decode in this client; the Rust ` +
          `side added a frame this client does not account for`,
      );
      const decoded = tryDecode(Buffer.from(hex, "hex"));
      assert.ok(decoded !== null, `vector '${name}' must decode`);
      assert.deepEqual(
        decoded.msg,
        expected,
        `vector '${name}' decoded to the wrong fields`,
      );
    }
    assert.deepEqual(
      vectors.map(([name]) => name).sort(),
      Object.keys(expectedFrames).sort(),
      "the expected-frame table and the vector file must cover the same names",
    );
  });

  await test("this client re-encodes every frame to the exact Rust bytes", () => {
    for (const [name, hex] of vectorRecords("frame")) {
      const expected = expectedFrames[name]!;
      assert.equal(
        encode(expected).toString("hex"),
        hex,
        `vector '${name}': this client's encoder disagrees with the server's`,
      );
    }
  });

  await test("the byte shapes an older client writes still decode here", () => {
    // The reverse-compatibility half: bytes a 0.21.0 peer puts on the wire,
    // which neither implementation emits today.
    const expectedAlt: Record<string, Message> = {
      connect_legacy_db_only: {
        type: "Connect",
        dbName: "mydb",
        password: null,
        username: null,
      },
      connect_legacy_username_explicit_empty: {
        type: "Connect",
        dbName: "mydb",
        password: "pw",
        username: null,
      },
    };
    const vectors = vectorRecords("framealt");
    assert.ok(vectors.length > 0, "vector file must contain alternate frames");
    for (const [name, hex] of vectors) {
      const expected = expectedAlt[name];
      assert.ok(expected !== undefined, `no expected decode for '${name}'`);
      const decoded = tryDecode(Buffer.from(hex, "hex"));
      assert.ok(decoded !== null, `vector '${name}' must decode`);
      assert.deepEqual(decoded.msg, expected, `vector '${name}'`);
    }
  });

  await test("the feature names match the server's, in order", () => {
    const shipped = vectorRecords("feature").map(([name]) => name);
    assert.deepEqual(
      Object.values(WIRE_FEATURE),
      shipped,
      "WIRE_FEATURE does not match the server's SERVER_FEATURES",
    );
    assert.deepEqual(
      [...CLIENT_CAPABILITIES.features],
      shipped,
      "CLIENT_CAPABILITIES.features does not match the server's SERVER_FEATURES",
    );
  });

  await test("the error class numbering matches the server's", () => {
    const shipped = vectorRecords("class");
    assert.deepEqual(
      Object.entries(WIRE_ERROR_CLASS).map(([name, byte]) => [
        name,
        String(byte),
      ]),
      shipped,
      "WIRE_ERROR_CLASS does not match the server's ErrorClass numbering",
    );
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nParameter binding — out-of-range and non-finite values");
  // ──────────────────────────────────────────────────────────

  await test("a bigint outside the signed 64-bit range is rejected before anything is queued", async () => {
    const { server } = queryServer();
    const port = await listen(server);
    const client = await Client.connect({ host: "127.0.0.1", port });
    try {
      for (const bad of [2n ** 63n, -(2n ** 63n) - 1n]) {
        const err = await client
          .query("Val filter .id = $1 { .id }", [bad])
          .then(
            () => null,
            (e: unknown) => e,
          );
        assert.ok(isPowDBError(err), `${bad} threw ${err}, not a PowDBError`);
        assert.equal((err as PowDBError).code, "invalid_argument");
      }
      // The connection must still work: a rejected parameter may never
      // leave a pending slot waiting for a reply that never comes.
      const after = await withTimeout(client.query("still alive"), 2000);
      assert.equal(after.kind, "ok");
    } finally {
      await client.close();
      await closeServer(server);
    }
  });

  await test("integral doubles outside the int64 range bind as float", async () => {
    const { server, received } = queryServer();
    const port = await listen(server);
    const client = await Client.connect({ host: "127.0.0.1", port });
    try {
      // 2^63 is one past i64::MAX, so the int tag has no room for it.
      for (const big of [2 ** 63, -(2 ** 63) - 2048, 1e19, 1e300, Number.MAX_VALUE]) {
        await withTimeout(client.query("Val filter .f = $1 { .id }", [big]), 2000);
      }
      const params = received
        .filter((m) => m.type === "QueryWithParams")
        .map((m) => (m as { params: { tag: string }[] }).params[0]!.tag);
      assert.deepEqual(params, ["float", "float", "float", "float", "float"]);
    } finally {
      await client.close();
      await closeServer(server);
    }
  });

  await test("an integral double above 2^53 still binds as int", async () => {
    const { server, received } = queryServer();
    const port = await listen(server);
    const client = await Client.connect({ host: "127.0.0.1", port });
    try {
      // Every double in this range is an exact integer, so the int tag holds
      // it losslessly -- and the tag is what decides the plan: an int literal
      // probes the B+tree on an `int` column, a float literal falls back to a
      // filtered sequential scan. Snowflake-shaped ids sit right here.
      const values = [2 ** 53, 2 ** 60, 2 ** 63 - 1024, -(2 ** 63)];
      for (const value of values) {
        await withTimeout(client.query("Val filter .id = $1 { .id }", [value]), 2000);
      }
      const params = received
        .filter((m) => m.type === "QueryWithParams")
        .map((m) => (m as { params: { tag: string; value: bigint }[] }).params[0]!);
      assert.deepEqual(
        params,
        values.map((value) => ({ tag: "int", value: BigInt(value) })),
      );
    } finally {
      await client.close();
      await closeServer(server);
    }
  });

  await test("the number-to-param rule matches the embedded addon's", () => {
    // The two bindings are hand-kept twins: the same JS number has to reach
    // the engine with the same tag whether it goes over the wire or through
    // the in-process addon, or one of them silently loses the index.
    const lib = readFileSync(
      fileURLToPath(new URL("../../../bindings/node/src/lib.rs", import.meta.url)),
      "utf8",
    );
    const rule = lib.match(/if (n\.is_finite\(\)[^{]*)\{\s*\n\s*Ok\(Value::Int/);
    assert.ok(rule, "the addon's number-to-param rule is no longer where this test looks");
    assert.equal(
      rule[1]!.replace(/\s+/g, " ").trim(),
      "n.is_finite() && n.fract() == 0.0 && n >= i64::MIN as f64 && n < i64::MAX as f64",
      "bindings/node/src/lib.rs changed its number rule; toWireParam has to move with it",
    );
  });

  await test("safe integers still bind as int", async () => {
    const { server, received } = queryServer();
    const port = await listen(server);
    const client = await Client.connect({ host: "127.0.0.1", port });
    try {
      await withTimeout(client.query("Val filter .id = $1", [42]), 2000);
      await withTimeout(client.query("Val filter .id = $1", [-0]), 2000);
      await withTimeout(client.query("Val filter .id = $1", [7n]), 2000);
      const params = received
        .filter((m) => m.type === "QueryWithParams")
        .map((m) => (m as { params: { tag: string }[] }).params[0]!);
      assert.deepEqual(
        params,
        [
          { tag: "int", value: 42n },
          { tag: "int", value: 0n },
          { tag: "int", value: 7n },
        ],
      );
    } finally {
      await client.close();
      await closeServer(server);
    }
  });

  await test("NaN and the infinities are rejected as invalid_argument", async () => {
    const { server } = queryServer();
    const port = await listen(server);
    const client = await Client.connect({ host: "127.0.0.1", port });
    try {
      for (const bad of [Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY]) {
        const err = await client.query("Val filter .f = $1", [bad]).then(
          () => null,
          (e: unknown) => e,
        );
        assert.ok(isPowDBError(err), `${bad} threw ${err}, not a PowDBError`);
        assert.equal((err as PowDBError).code, "invalid_argument");
      }
      const after = await withTimeout(client.query("still alive"), 2000);
      assert.equal(after.kind, "ok");
    } finally {
      await client.close();
      await closeServer(server);
    }
  });

  await test("queryNative rejects the same values without desyncing", async () => {
    const { server } = queryServer();
    const port = await listen(server);
    const client = await Client.connect({ host: "127.0.0.1", port });
    try {
      const err = await client.queryNative("Val filter .id = $1", [2n ** 64n]).then(
        () => null,
        (e: unknown) => e,
      );
      assert.ok(isPowDBError(err));
      assert.equal((err as PowDBError).code, "invalid_argument");
      const after = await withTimeout(client.query("still alive"), 2000);
      assert.equal(after.kind, "ok");
    } finally {
      await client.close();
      await closeServer(server);
    }
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nClient-side frame pre-checks");
  // ──────────────────────────────────────────────────────────

  await test("more than MAX_PARAMS parameters fails locally with size_exceeded", async () => {
    const { server, received } = queryServer();
    const port = await listen(server);
    const client = await Client.connect({ host: "127.0.0.1", port });
    try {
      const params = new Array(MAX_PARAMS + 1).fill(1);
      const err = await client.query("Val filter .id = $1", params).then(
        () => null,
        (e: unknown) => e,
      );
      assert.ok(isPowDBError(err), `threw ${err}, not a PowDBError`);
      assert.equal((err as PowDBError).code, "size_exceeded");
      assert.ok(
        !received.some((m) => m.type === "QueryWithParams"),
        "an over-cap frame must never reach the server",
      );
      const after = await withTimeout(client.query("still alive"), 2000);
      assert.equal(after.kind, "ok");
    } finally {
      await client.close();
      await closeServer(server);
    }
  });

  await test("a frame over MAX_PAYLOAD_SIZE fails locally with size_exceeded", async () => {
    const { server, received } = queryServer();
    const port = await listen(server);
    const client = await Client.connect({ host: "127.0.0.1", port });
    try {
      const err = await client.query("x".repeat(MAX_PAYLOAD_SIZE + 1)).then(
        () => null,
        (e: unknown) => e,
      );
      assert.ok(isPowDBError(err), `threw ${err}, not a PowDBError`);
      assert.equal((err as PowDBError).code, "size_exceeded");
      assert.ok(
        !received.some((m) => m.type === "Query"),
        "an oversized frame must never reach the server",
      );
      const after = await withTimeout(client.query("still alive"), 2000);
      assert.equal(after.kind, "ok");
    } finally {
      await client.close();
      await closeServer(server);
    }
  });

  await test("encode refuses to write a parameter count it cannot frame", () => {
    assert.throws(
      () =>
        encode({
          type: "QueryWithParams",
          query: "q",
          params: new Array(MAX_PARAMS + 1).fill({ tag: "null" }),
        }),
      /too many parameters/,
    );
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nServer-initiated frames and socket teardown");
  // ──────────────────────────────────────────────────────────

  await test("an unsolicited Error frame closes the client with the server's text", async () => {
    const server = net.createServer((sock) => {
      sock.once("data", () => {
        sock.write(encode(negotiatedConnectOk()));
        setTimeout(() => {
          if (!sock.destroyed) {
            sock.write(
              encode({
                type: "Error",
                message: "connection idle for 300s; closing",
                errorClass: WIRE_ERROR_CLASS.timeout,
              }),
            );
          }
        }, 10);
      });
      sock.on("error", () => {});
    });
    const port = await listen(server);
    const client = await Client.connect({ host: "127.0.0.1", port });
    try {
      const closed = await nextCloseEvent(client, 2000);
      assert.ok(
        isPowDBError(closed.error),
        `close carried ${closed.error}, not a PowDBError`,
      );
      const err = closed.error as PowDBError;
      assert.match(err.message, /connection idle for 300s/);
      assert.equal(err.code, "timeout");
      assert.equal(err.wireErrorClass, WIRE_ERROR_CLASS.timeout);
    } finally {
      await client.close();
      await closeServer(server);
    }
  });

  await test("an unsolicited non-Error frame is still a protocol error", async () => {
    const server = net.createServer((sock) => {
      sock.once("data", () => {
        sock.write(encode(negotiatedConnectOk()));
        setTimeout(() => {
          if (!sock.destroyed) {
            sock.write(encode({ type: "ResultOk", affected: 0n }));
          }
        }, 10);
      });
      sock.on("error", () => {});
    });
    const port = await listen(server);
    const client = await Client.connect({ host: "127.0.0.1", port });
    try {
      const closed = await nextCloseEvent(client, 2000);
      assert.ok(isPowDBError(closed.error));
      assert.equal((closed.error as PowDBError).code, "protocol_error");
    } finally {
      await client.close();
      await closeServer(server);
    }
  });

  await test("a reset connection rejects in-flight queries with a PowDBError", async () => {
    const server = net.createServer((sock) => {
      let scratch = Buffer.alloc(0);
      sock.on("data", (chunk) => {
        const collected = collectFrames(scratch, Buffer.from(chunk));
        scratch = collected.rest;
        for (const msg of collected.messages) {
          if (msg.type === "Connect") {
            sock.write(encode(negotiatedConnectOk()));
          } else if (msg.type === "Query") {
            sock.resetAndDestroy();
          }
        }
      });
      sock.on("error", () => {});
    });
    const port = await listen(server);
    const client = await Client.connect({ host: "127.0.0.1", port });
    try {
      const err = await client.query("boom").then(
        () => null,
        (e: unknown) => e,
      );
      assert.ok(isPowDBError(err), `rejected with ${err}, not a PowDBError`);
      assert.equal((err as PowDBError).code, "closed");
      assert.ok((err as PowDBError).cause instanceof Error, "the raw socket error must be the cause");
    } finally {
      await client.close();
      await closeServer(server);
    }
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nResult cell cap does not kill the connection");
  // ──────────────────────────────────────────────────────────

  await test("an over-cap result rejects one query and leaves the client usable", async () => {
    const rows = 300_000;
    const cols = 7;
    const server = net.createServer((sock) => {
      let scratch = Buffer.alloc(0);
      sock.on("data", (chunk) => {
        const collected = collectFrames(scratch, Buffer.from(chunk));
        scratch = collected.rest;
        for (const msg of collected.messages) {
          if (msg.type === "Connect") {
            sock.write(encode(negotiatedConnectOk()));
          } else if (msg.type === "Query" && msg.query === "huge") {
            sock.write(oversizedResultRowsFrame(rows, cols));
          } else if (msg.type === "Query") {
            sock.write(encode({ type: "ResultOk", affected: 0n }));
          } else if (msg.type === "Disconnect") {
            sock.end();
          }
        }
      });
      sock.on("error", () => {});
    });
    const port = await listen(server);
    const client = await Client.connect({ host: "127.0.0.1", port });
    try {
      const err = await client.query("huge").then(
        () => null,
        (e: unknown) => e,
      );
      assert.ok(isPowDBError(err), `rejected with ${err}, not a PowDBError`);
      assert.equal((err as PowDBError).code, "size_exceeded");
      assert.match((err as PowDBError).message, /result too large/);
      const after = await withTimeout(client.query("still alive"), 2000);
      assert.equal(after.kind, "ok");
    } finally {
      await client.close();
      await closeServer(server);
    }
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nIn-flight window");
  // ──────────────────────────────────────────────────────────

  await test("500 concurrent queries all resolve within the in-flight window", async () => {
    const { server, stats } = queryServer(2);
    const port = await listen(server);
    const client = await Client.connect({ host: "127.0.0.1", port });
    try {
      const results = await withTimeout(
        Promise.all(
          Array.from({ length: 500 }, (_, i) => client.query(`q${i}`)),
        ),
        30_000,
      );
      assert.equal(results.length, 500);
      assert.ok(
        stats.maxInFlight <= 64,
        `client wrote ${stats.maxInFlight} unanswered frames, cap is 64`,
      );
    } finally {
      await client.close();
      await closeServer(server);
    }
  });

  await test("the in-flight window is configurable and preserves reply order", async () => {
    const { server, stats } = queryServer(2);
    const port = await listen(server);
    const client = await Client.connect({
      host: "127.0.0.1",
      port,
      maxInFlight: 4,
    });
    try {
      const results = await withTimeout(
        Promise.all(Array.from({ length: 40 }, (_, i) => client.query(`q${i}`))),
        10_000,
      );
      assert.equal(results.length, 40);
      assert.ok(
        stats.maxInFlight <= 4,
        `client wrote ${stats.maxInFlight} unanswered frames, cap is 4`,
      );
    } finally {
      await client.close();
      await closeServer(server);
    }
  });

  await test("the in-flight window bounds unanswered bytes, not only frames", async () => {
    // 64 frames of 20 KiB is 1.28 MiB of read-ahead, and a pre-0.28.0 server
    // cancels the running query and closes the connection at 1 MiB with no
    // Error frame -- the exact ECONNRESET this window exists to prevent. The
    // frame count never gets near its own cap on traffic like this.
    const { server, stats } = queryServer(2);
    const port = await listen(server);
    const client = await Client.connect({ host: "127.0.0.1", port });
    try {
      const text = "q".repeat(20 * 1024);
      const results = await withTimeout(
        Promise.all(Array.from({ length: 200 }, (_, i) => client.query(`${text}${i}`))),
        30_000,
      );
      assert.equal(results.length, 200);
      assert.ok(
        stats.maxInFlightBytes < 1024 * 1024,
        `client left ${stats.maxInFlightBytes} unanswered bytes on the wire, cap is 1 MiB`,
      );
      // The byte budget, not the frame count, is what bound this burst.
      assert.ok(
        stats.maxInFlight < 64,
        `frame count ${stats.maxInFlight} reached the window, so bytes were never the binding cap`,
      );
    } finally {
      await client.close();
      await closeServer(server);
    }
  });

  await test("a single frame larger than the byte budget still goes out", async () => {
    // The budget may never deadlock a client whose one and only query is
    // bigger than it: with nothing in flight, the head frame always writes.
    const { server, stats } = queryServer(2);
    const port = await listen(server);
    const client = await Client.connect({ host: "127.0.0.1", port });
    try {
      const result = await withTimeout(client.query("q".repeat(2 * 1024 * 1024)), 30_000);
      assert.equal(result.kind, "ok");
      assert.ok(stats.maxInFlightBytes > 1024 * 1024);
    } finally {
      await client.close();
      await closeServer(server);
    }
  });

  await test("the window's caps are the ones the server actually enforces", () => {
    const path = fileURLToPath(
      new URL("../../../crates/server/src/handler/wire.rs", import.meta.url),
    );
    const text = readFileSync(path, "utf8");
    const constant = (name: string): number => {
      const matches = [
        ...text.matchAll(
          new RegExp(`^pub\\(super\\) const ${name}: usize = ([^;]+);$`, "gm"),
        ),
      ];
      assert.equal(matches.length, 1, `expected exactly one \`${name}\` in ${path}`);
      const expr = matches[0]![1]!.replace(/_/g, "").trim();
      assert.match(expr, /^\d+( \* \d+)*$/, `cannot evaluate ${name} = ${expr}`);
      return expr.split("*").reduce((acc, part) => acc * Number(part.trim()), 1);
    };
    assert.ok(
      DEFAULT_MAX_IN_FLIGHT < constant("MAX_IN_FLIGHT_READ_AHEAD_FRAMES"),
      "the default window no longer sits under the server's frame cap",
    );
    assert.ok(
      MAX_IN_FLIGHT_BYTES <= constant("MAX_IN_FLIGHT_READ_AHEAD_BYTES"),
      "the client's byte budget no longer sits under the server's byte cap",
    );
  });

  await test("queries queued behind the window reject when the connection dies", async () => {
    const server = net.createServer((sock) => {
      let scratch = Buffer.alloc(0);
      sock.on("data", (chunk) => {
        const collected = collectFrames(scratch, Buffer.from(chunk));
        scratch = collected.rest;
        for (const msg of collected.messages) {
          if (msg.type === "Connect") {
            sock.write(encode(negotiatedConnectOk()));
          } else if (msg.type === "Query") {
            sock.resetAndDestroy();
          }
        }
      });
      sock.on("error", () => {});
    });
    const port = await listen(server);
    const client = await Client.connect({
      host: "127.0.0.1",
      port,
      maxInFlight: 2,
    });
    try {
      const settled = await withTimeout(
        Promise.allSettled(
          Array.from({ length: 20 }, (_, i) => client.query(`q${i}`)),
        ),
        5000,
      );
      assert.equal(settled.length, 20);
      for (const outcome of settled) {
        assert.equal(outcome.status, "rejected");
        const reason = (outcome as PromiseRejectedResult).reason;
        assert.ok(isPowDBError(reason), `queued query rejected with ${reason}`);
      }
    } finally {
      await client.close();
      await closeServer(server);
    }
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nPool health");
  // ──────────────────────────────────────────────────────────

  await test("the pool evicts idle clients whose connection died", async () => {
    const first = queryServer();
    const port = await listen(first.server);
    const pool = new Pool({ host: "127.0.0.1", port, max: 2 });
    try {
      const a = await pool.acquire();
      const b = await pool.acquire();
      pool.release(a);
      pool.release(b);
      assert.equal(pool.idle, 2);

      await closeServer(first.server, first.sockets);
      await settleCloseEvents();

      assert.equal(pool.idle, 0, "dead clients must not stay in the idle set");
      assert.equal(pool.size, 0, "an evicted client must give its slot back");

      const second = queryServer();
      await listenOn(second.server, port);
      try {
        const result = await withTimeout(
          pool.withClient((c) => c.query("after restart")),
          5000,
        );
        assert.equal(result.kind, "ok");
      } finally {
        await closeServer(second.server, second.sockets);
      }
    } finally {
      await pool.close();
    }
  });

  await test("an acquire timeout is a PowDBError", async () => {
    const { server } = queryServer();
    const port = await listen(server);
    const pool = new Pool({
      host: "127.0.0.1",
      port,
      max: 1,
      acquireTimeoutMs: 25,
    });
    try {
      const held = await pool.acquire();
      const err = await pool.acquire().then(
        () => null,
        (e: unknown) => e,
      );
      assert.ok(isPowDBError(err), `acquire rejected with ${err}`);
      assert.equal((err as PowDBError).code, "timeout");
      assert.match((err as PowDBError).message, /pool acquire timeout/);
      pool.release(held);
    } finally {
      await pool.close();
      await closeServer(server);
    }
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
