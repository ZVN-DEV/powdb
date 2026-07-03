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
  MAX_SYNC_UNITS,
  type Message,
  type WireSyncStatus,
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
