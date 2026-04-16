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
