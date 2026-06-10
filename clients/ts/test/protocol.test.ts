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
} from "../src/protocol.js";
import { Client } from "../src/index.js";

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
