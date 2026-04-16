/**
 * Tests for the PowDB connection pool (clients/ts/src/pool.ts).
 *
 * These are LIVE tests — they require a running PowDB server. If no server
 * is reachable at `$POWDB_HOST:$POWDB_PORT` the suite exits 0 with a
 * "SKIPPED" message (we don't want CI to fail just because nobody started
 * the server).
 *
 *     POWDB_HOST=127.0.0.1 POWDB_PORT=15433 npx tsx test/pool.test.ts
 */

import * as net from "node:net";
import { strict as assert } from "node:assert";
import { Pool } from "../src/pool.js";
import { Client } from "../src/index.js";

const HOST = process.env.POWDB_HOST ?? "127.0.0.1";
const PORT = Number(process.env.POWDB_PORT ?? "15433");

let passed = 0;
let failed = 0;
const failures: string[] = [];

async function test(name: string, fn: () => Promise<void>) {
  try {
    await fn();
    passed++;
    console.log(`  ✓ ${name}`);
  } catch (err: any) {
    failed++;
    failures.push(`${name}: ${err.message}`);
    console.log(`  ✗ ${name}`);
    console.log(`    ${err.message}`);
  }
}

/** Probe the server by opening a raw TCP socket; no handshake. */
function isServerReachable(
  host: string,
  port: number,
  timeoutMs = 500
): Promise<boolean> {
  return new Promise((resolve) => {
    const socket = new net.Socket();
    let done = false;
    const finish = (ok: boolean) => {
      if (done) return;
      done = true;
      socket.destroy();
      resolve(ok);
    };
    socket.setTimeout(timeoutMs);
    socket.once("connect", () => finish(true));
    socket.once("timeout", () => finish(false));
    socket.once("error", () => finish(false));
    socket.connect(port, host);
  });
}

async function main() {
  console.log(`\nProbing ${HOST}:${PORT}...`);
  const reachable = await isServerReachable(HOST, PORT);
  if (!reachable) {
    console.log(
      `SKIPPED: no PowDB server at ${HOST}:${PORT}. Set POWDB_HOST/POWDB_PORT to run pool tests.`
    );
    process.exit(0);
  }
  console.log(`Server reachable — running pool tests.\n`);

  // ──────────────────────────────────────────────────────────
  console.log("basic acquire / release");
  // ──────────────────────────────────────────────────────────

  await test("acquire hands out a usable client; release returns it; next acquire reuses it", async () => {
    const pool = new Pool({ host: HOST, port: PORT, max: 2 });
    try {
      const c1 = await pool.acquire();
      assert.ok(c1 instanceof Client);
      assert.equal(pool.size, 1);
      assert.equal(pool.idle, 0);

      pool.release(c1);
      assert.equal(pool.size, 1);
      assert.equal(pool.idle, 1);

      const c2 = await pool.acquire();
      assert.equal(c2, c1, "expected the same client back");
      assert.equal(pool.idle, 0);
      pool.release(c2);
    } finally {
      await pool.close();
    }
  });

  await test("size and idle getters track state", async () => {
    const pool = new Pool({ host: HOST, port: PORT, max: 3 });
    try {
      assert.equal(pool.size, 0);
      assert.equal(pool.idle, 0);

      const a = await pool.acquire();
      const b = await pool.acquire();
      assert.equal(pool.size, 2);
      assert.equal(pool.idle, 0);

      pool.release(a);
      assert.equal(pool.size, 2);
      assert.equal(pool.idle, 1);

      pool.release(b);
      assert.equal(pool.size, 2);
      assert.equal(pool.idle, 2);
    } finally {
      await pool.close();
    }
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nmax concurrency");
  // ──────────────────────────────────────────────────────────

  await test("max=2: third acquire blocks until a release", async () => {
    const pool = new Pool({
      host: HOST,
      port: PORT,
      max: 2,
      acquireTimeoutMs: 5_000,
    });
    try {
      const c1 = await pool.acquire();
      const c2 = await pool.acquire();
      assert.equal(pool.size, 2);

      let resolved = false;
      const third = pool.acquire().then((c) => {
        resolved = true;
        return c;
      });

      // Give the event loop a tick so any (incorrect) immediate resolution
      // would surface. The assertion is that `third` is still pending.
      await new Promise((r) => setTimeout(r, 20));
      assert.equal(resolved, false, "third acquire should be blocked");

      pool.release(c1);
      const c3 = await third;
      assert.ok(c3 instanceof Client);
      assert.equal(c3, c1, "waiter should receive the just-released client");

      pool.release(c2);
      pool.release(c3);
    } finally {
      await pool.close();
    }
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nacquireTimeoutMs");
  // ──────────────────────────────────────────────────────────

  await test("third acquire rejects after acquireTimeoutMs if nothing is released", async () => {
    const pool = new Pool({
      host: HOST,
      port: PORT,
      max: 2,
      acquireTimeoutMs: 100,
    });
    try {
      const c1 = await pool.acquire();
      const c2 = await pool.acquire();

      const start = Date.now();
      await assert.rejects(
        pool.acquire(),
        (err: Error) => err.message.includes("pool acquire timeout")
      );
      const elapsed = Date.now() - start;
      assert.ok(
        elapsed >= 90,
        `expected timeout to take >= 90ms, took ${elapsed}ms`
      );

      pool.release(c1);
      pool.release(c2);
    } finally {
      await pool.close();
    }
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nwithClient");
  // ──────────────────────────────────────────────────────────

  await test("withClient releases on success", async () => {
    const pool = new Pool({ host: HOST, port: PORT, max: 1 });
    try {
      const v = await pool.withClient(async (c) => {
        assert.ok(c instanceof Client);
        return 123;
      });
      assert.equal(v, 123);
      // After success, the client should be back in the idle queue.
      assert.equal(pool.idle, 1);
      assert.equal(pool.size, 1);
    } finally {
      await pool.close();
    }
  });

  await test("withClient destroys on error (no stale idle client)", async () => {
    const pool = new Pool({ host: HOST, port: PORT, max: 1 });
    try {
      await assert.rejects(
        pool.withClient(async () => {
          throw new Error("boom");
        }),
        /boom/
      );
      // After destroy, the slot is free and there's no idle client.
      assert.equal(pool.idle, 0);
      assert.equal(pool.size, 0);

      // Pool should still be usable after the error.
      const v = await pool.withClient(async () => "ok");
      assert.equal(v, "ok");
    } finally {
      await pool.close();
    }
  });

  // ──────────────────────────────────────────────────────────
  console.log("\ndestroy");
  // ──────────────────────────────────────────────────────────

  await test("destroy frees a slot and lets a blocked waiter proceed", async () => {
    const pool = new Pool({
      host: HOST,
      port: PORT,
      max: 1,
      acquireTimeoutMs: 5_000,
    });
    try {
      const c1 = await pool.acquire();

      let resolved = false;
      const second = pool.acquire().then((c) => {
        resolved = true;
        return c;
      });

      await new Promise((r) => setTimeout(r, 20));
      assert.equal(resolved, false);

      pool.destroy(c1);
      const c2 = await second;
      assert.ok(c2 instanceof Client);
      assert.notEqual(c2, c1, "destroy should have closed the original");
      pool.release(c2);
    } finally {
      await pool.close();
    }
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nclose");
  // ──────────────────────────────────────────────────────────

  await test("close rejects pending waiters with 'pool closed'", async () => {
    const pool = new Pool({
      host: HOST,
      port: PORT,
      max: 1,
      acquireTimeoutMs: 10_000,
    });
    const c1 = await pool.acquire();

    const pending = pool.acquire();
    // Don't await pending — close() should reject it.
    const closed = pool.close();

    await assert.rejects(pending, /pool closed/);
    // Caller is responsible for closing their still-held client.
    await c1.close();
    await closed;
  });

  await test("acquire after close rejects immediately", async () => {
    const pool = new Pool({ host: HOST, port: PORT, max: 1 });
    await pool.close();
    await assert.rejects(pool.acquire(), /pool closed/);
  });

  await test("close is idempotent", async () => {
    const pool = new Pool({ host: HOST, port: PORT, max: 1 });
    await pool.close();
    await pool.close();
    assert.equal(pool.closed, true);
  });

  await test("close awaits in-flight idle client .close()", async () => {
    const pool = new Pool({ host: HOST, port: PORT, max: 2 });
    const a = await pool.acquire();
    const b = await pool.acquire();
    pool.release(a);
    pool.release(b);
    assert.equal(pool.idle, 2);
    await pool.close();
    assert.equal(pool.idle, 0);
    assert.equal(pool.closed, true);
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nconstructor validation");
  // ──────────────────────────────────────────────────────────

  await test("max < 1 rejected", async () => {
    assert.throws(() => new Pool({ host: HOST, port: PORT, max: 0 }), TypeError);
  });

  await test("negative acquireTimeoutMs rejected", async () => {
    assert.throws(
      () => new Pool({ host: HOST, port: PORT, acquireTimeoutMs: -1 }),
      TypeError
    );
  });

  // ──────────────────────────────────────────────────────────
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
