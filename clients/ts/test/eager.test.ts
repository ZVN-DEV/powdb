/**
 * Tests for pipelined ("eager") connect.
 *
 * Mock-server cases run against a loopback `net.createServer` speaking just
 * enough protocol to control handshake ordering deterministically. When
 * POWDB_HOST/POWDB_PORT are set (run through run-with-server.ts), the
 * end-to-end cases also exercise a real powdb-server.
 *
 * Run with:
 *   npx tsx test/run-with-server.ts test/eager.test.ts
 */

import * as net from "node:net";
import { strict as assert } from "node:assert";
import { encode, tryDecode, type Message } from "../src/protocol.js";
import { Client, isPowDBError } from "../src/index.js";

const HOST = process.env.POWDB_HOST ?? "127.0.0.1";
const PORT = Number(process.env.POWDB_PORT ?? "0");

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

function collectFrames(buf: Buffer, chunk: Buffer) {
  buf = Buffer.concat([buf, chunk]);
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

function closeServer(server: net.Server): Promise<void> {
  return new Promise((resolve) => server.close(() => resolve()));
}

async function main() {
  console.log("\nEager connect — mock server (deterministic ordering)");

  await test(
    "eager connect resolves before ConnectOk; query is pipelined behind Connect",
    async () => {
      // The mock withholds ALL replies until it has seen BOTH the Connect
      // frame and a Query frame. A non-eager client would deadlock here
      // (connect() never resolves, so the query is never sent); the eager
      // client must sail through.
      let sawConnectBeforeReplying = false;
      const server = net.createServer((sock) => {
        let scratch = Buffer.alloc(0);
        let gotConnect = false;
        let gotQuery = false;
        sock.on("data", (chunk) => {
          const collected = collectFrames(scratch, Buffer.from(chunk));
          scratch = collected.rest;
          for (const msg of collected.messages) {
            if (msg.type === "Connect") gotConnect = true;
            if (msg.type === "Query") gotQuery = true;
            if (msg.type === "Disconnect") sock.end();
          }
          if (gotConnect && gotQuery) {
            sawConnectBeforeReplying = true;
            // Replies strictly in request order: ConnectOk, then the result.
            sock.write(encode({ type: "ConnectOk", version: "0.8.1" }));
            sock.write(encode({ type: "ResultScalar", value: "42" }));
          }
        });
        sock.on("error", () => {});
      });
      const port = await listen(server);

      try {
        const client = await Client.connect({
          host: "127.0.0.1",
          port,
          eager: true,
        });
        // connect() resolved even though no ConnectOk has been sent yet.
        assert.equal(client.serverVersion, "", "version unknown pre-handshake");

        const result = await client.query("count(X)");
        assert.equal(result.kind, "scalar");
        if (result.kind === "scalar") assert.equal(result.value, "42");
        assert.ok(sawConnectBeforeReplying);

        await client.ready(); // settled by now; must not throw
        assert.equal(client.serverVersion, "0.8.1");
        await client.close();
      } finally {
        await closeServer(server);
      }
    },
  );

  await test(
    "handshake Error frame rejects every queued query with auth_failed and closes",
    async () => {
      const server = net.createServer((sock) => {
        let scratch = Buffer.alloc(0);
        sock.on("data", (chunk) => {
          const collected = collectFrames(scratch, Buffer.from(chunk));
          scratch = collected.rest;
          for (const msg of collected.messages) {
            if (msg.type === "Connect") {
              sock.write(encode({ type: "Error", message: "authentication failed" }));
              sock.end();
            }
          }
        });
        sock.on("error", () => {});
      });
      const port = await listen(server);

      try {
        const client = await Client.connect({
          host: "127.0.0.1",
          port,
          eager: true,
          password: "wrong",
        });
        const q1 = client.query("count(A)");
        const q2 = client.query("count(B)");

        for (const q of [q1, q2]) {
          try {
            await q;
            assert.fail("queued query should reject with the handshake error");
          } catch (err) {
            assert.ok(isPowDBError(err), `expected PowDBError, got ${err}`);
            assert.equal(err.code, "auth_failed");
            assert.ok(err.message.includes("connect failed"));
          }
        }

        try {
          await client.ready();
          assert.fail("ready() should reject after a failed handshake");
        } catch (err) {
          assert.ok(isPowDBError(err) && err.code === "auth_failed");
        }

        // The client is closed: later queries reject immediately.
        try {
          await client.query("count(C)");
          assert.fail("post-failure query should reject");
        } catch (err) {
          assert.ok(isPowDBError(err), `expected PowDBError, got ${err}`);
        }
        await client.close(); // must not hang or throw
      } finally {
        await closeServer(server);
      }
    },
  );

  await test(
    "peer closing before ConnectOk rejects queued queries with connect_failed",
    async () => {
      const server = net.createServer((sock) => {
        // Read the Connect frame, then FIN without replying.
        sock.once("data", () => sock.end());
        sock.on("error", () => {});
      });
      const port = await listen(server);

      try {
        const client = await Client.connect({
          host: "127.0.0.1",
          port,
          eager: true,
        });
        const q = client.query("count(A)");
        try {
          await q;
          assert.fail("query should reject when the peer drops mid-handshake");
        } catch (err) {
          assert.ok(isPowDBError(err), `expected PowDBError, got ${err}`);
          assert.equal(err.code, "connect_failed");
          assert.ok(err.message.includes("closed during handshake"));
        }
        await client.close();
      } finally {
        await closeServer(server);
      }
    },
  );

  await test("non-eager connect keeps blocking-handshake semantics", async () => {
    const server = net.createServer((sock) => {
      let scratch = Buffer.alloc(0);
      sock.on("data", (chunk) => {
        const collected = collectFrames(scratch, Buffer.from(chunk));
        scratch = collected.rest;
        for (const msg of collected.messages) {
          if (msg.type === "Connect") {
            sock.write(encode({ type: "ConnectOk", version: "0.8.1" }));
          } else if (msg.type === "Disconnect") {
            sock.end();
          }
        }
      });
      sock.on("error", () => {});
    });
    const port = await listen(server);

    try {
      const client = await Client.connect({ host: "127.0.0.1", port });
      // Version is already known the moment connect() resolves.
      assert.equal(client.serverVersion, "0.8.1");
      await client.ready(); // resolved
      await client.close();
    } finally {
      await closeServer(server);
    }
  });

  await test("non-eager connect still throws auth_failed directly", async () => {
    const server = net.createServer((sock) => {
      sock.once("data", () => {
        sock.write(encode({ type: "Error", message: "authentication failed" }));
        sock.end();
      });
      sock.on("error", () => {});
    });
    const port = await listen(server);

    try {
      await assert.rejects(
        Client.connect({ host: "127.0.0.1", port, password: "wrong" }),
        (err: unknown) => isPowDBError(err) && err.code === "auth_failed",
      );
    } finally {
      await closeServer(server);
    }
  });

  if (PORT > 0) {
    console.log("\nEager connect — real server");

    const T = `E${Date.now().toString(36)}`;

    await test("eager connect + immediate queries, end to end", async () => {
      const client = await Client.connect({
        host: HOST,
        port: PORT,
        eager: true,
      });
      try {
        // Issue queries immediately — all pipelined behind the Connect frame.
        const ddl = client.query(`type ${T} { required name: str, n: int }`);
        const ins = client.query(`insert ${T} { name := "a", n := 1 }`);
        const cnt = client.query(`count(${T})`);

        assert.equal((await ddl).kind, "message");
        assert.equal((await ins).kind, "ok");
        const count = await cnt;
        assert.equal(count.kind, "scalar");
        if (count.kind === "scalar") assert.equal(count.value, "1");

        // Handshake settled along the way.
        await client.ready();
        assert.ok(client.serverVersion.length > 0);
      } finally {
        await client.query(`drop ${T}`).catch(() => {});
        await client.close();
      }
    });
  } else {
    console.log("\n(POWDB_PORT not set — skipping real-server eager tests)");
  }

  console.log("\n" + "═".repeat(50));
  console.log(`Results: ${passed} passed, ${failed} failed`);
  if (failures.length > 0) {
    console.log("\nFailures:");
    for (const f of failures) console.log(`  - ${f}`);
  }
  console.log("═".repeat(50));
  process.exit(failed > 0 ? 1 : 0);
}

main().catch((err) => {
  console.error("Test suite crashed:", err);
  process.exit(1);
});
