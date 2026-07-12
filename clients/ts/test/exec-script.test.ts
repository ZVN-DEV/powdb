/**
 * End-to-end tests for pipelined script execution (`execScript`).
 *
 * Run through package.json to start a disposable PowDB server automatically:
 *   pnpm test:exec-script
 *
 * Or point at an existing server:
 *   POWDB_HOST=127.0.0.1 POWDB_PORT=15433 npx tsx test/exec-script.test.ts
 */

import type * as net from "node:net";
import { strict as assert } from "node:assert";
import {
  Client,
  Pool,
  PowDBScriptError,
  isPowDBScriptError,
  type QueryResult,
} from "../src/index.js";

const HOST = process.env.POWDB_HOST ?? "127.0.0.1";
const PORT = Number(process.env.POWDB_PORT ?? "15433");

let passed = 0;
let failed = 0;
const failures: string[] = [];

// Unique table prefix so tests don't collide across runs
const T = `S${Date.now().toString(36)}`;
const tbl = (name: string) => `${T}_${name}`;

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

function scalar(r: QueryResult): string {
  assert.equal(r.kind, "scalar", `expected scalar, got ${r.kind}`);
  return r.kind === "scalar" ? r.value : "";
}

/**
 * Count the socket-level write() calls a client performs. Each request frame
 * is exactly one write, so this observes dispatch without any timing games.
 */
function instrumentWrites(client: Client): { count: () => number } {
  const sock = (client as unknown as { socket: net.Socket }).socket;
  const orig = sock.write.bind(sock);
  let writes = 0;
  (sock as { write: unknown }).write = (
    ...args: Parameters<net.Socket["write"]>
  ) => {
    writes++;
    return orig(...args);
  };
  return { count: () => writes };
}

async function main() {
  console.log(`\nConnecting to ${HOST}:${PORT}...`);
  const client = await Client.connect({ host: HOST, port: PORT });
  console.log(`Connected — server v${client.serverVersion}\n`);

  // ──────────────────────────────────────────────────────────
  console.log("execScript — pipelining and ordering");
  // ──────────────────────────────────────────────────────────

  const items = tbl("Items");
  await client.query(`type ${items} { required name: str, n: int }`);

  await test("all statements are written before any reply is awaited (pipelined)", async () => {
    const script = Array.from(
      { length: 10 },
      (_, i) => `insert ${items} { name := "row${i}", n := ${i} }`,
    ).join(";\n");

    const writes = instrumentWrites(client);
    const before = writes.count();
    const p = client.execScript(script);
    // execScript dispatches synchronously (no reply can even be processed
    // until a later tick), so all 10 Query frames must already be written
    // HERE — before we await anything. A sequential implementation would
    // have written exactly one.
    assert.equal(
      writes.count() - before,
      10,
      "expected all 10 statements on the wire before the first reply",
    );

    const results = await p;
    assert.equal(results.length, 10);
    for (const r of results) assert.equal(r.kind, "ok");
  });

  await test("results arrive in statement order", async () => {
    const seq = tbl("Seq");
    const results = await client.execScript(`
      type ${seq} { required name: str };
      insert ${seq} { name := "a" };
      count(${seq});
      insert ${seq} { name := "b" };
      count(${seq});
    `);
    assert.equal(results.length, 5);
    assert.equal(results[0]!.kind, "message"); // DDL
    assert.equal(results[1]!.kind, "ok");
    assert.equal(scalar(results[2]!), "1"); // after first insert
    assert.equal(results[3]!.kind, "ok");
    assert.equal(scalar(results[4]!), "2"); // after second insert
  });

  await test("splitting is statement-aware end to end (`;` in strings, # comments)", async () => {
    const notes = tbl("Notes");
    const results = await client.execScript(
      `type ${notes} { required body: str };\n` +
        `# seed data; two rows\n` +
        `insert ${notes} { body := "hello; world" };\n` +
        `insert ${notes} { body := "semi\\"; colon" };\n` +
        `count(${notes})`,
    );
    // 4 statements: the comment attaches to the insert, never splits on `;`.
    assert.equal(results.length, 4);
    assert.equal(scalar(results[3]!), "2");

    const check = await client.query(`${notes} { .body }`);
    assert.equal(check.kind, "rows");
    if (check.kind === "rows") {
      const bodies = check.rows.map((r) => r[0]);
      assert.ok(bodies.includes("hello; world"), `got ${bodies}`);
    }
  });

  await test("empty script resolves to an empty result list", async () => {
    assert.deepEqual(await client.execScript(""), []);
    assert.deepEqual(await client.execScript("  ;;  \n;"), []);
    assert.deepEqual(await client.execScript(";;", { continueOnError: true }), []);
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nexecScript — fail-fast (default)");
  // ──────────────────────────────────────────────────────────

  await test("rejects with PowDBScriptError carrying index + results so far", async () => {
    const ff = tbl("FF");
    try {
      await client.execScript(`
        type ${ff} { required name: str };
        insert ${ff} { name := "first" };
        this is not valid powql;
        insert ${ff} { name := "second" }
      `);
      assert.fail("should have rejected");
    } catch (err) {
      assert.ok(isPowDBScriptError(err), `expected PowDBScriptError, got ${err}`);
      assert.ok(err instanceof PowDBScriptError);
      assert.equal(err.code, "query_failed");
      assert.equal(err.statementIndex, 2);
      assert.equal(err.statement, "this is not valid powql");
      // Results of the statements before the failure, in order.
      assert.equal(err.results.length, 2);
      assert.equal(err.results[0]!.kind, "message");
      assert.equal(err.results[1]!.kind, "ok");
      assert.ok(err.message.includes("statement 3/4"));
    }

    // Documented pipelining semantics: the statement after the failure was
    // already on the wire and still executed server-side.
    const c = await client.query(`count(${ff})`);
    assert.equal(scalar(c), "2");
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nexecScript — continueOnError");
  // ──────────────────────────────────────────────────────────

  await test("returns a dense per-statement outcome array", async () => {
    const ce = tbl("CE");
    const outcomes = await client.execScript(
      `
        type ${ce} { required name: str };
        insert ${ce} { name := "first" };
        this is not valid powql;
        insert ${ce} { name := "second" };
        count(${ce})
      `,
      { continueOnError: true },
    );
    assert.equal(outcomes.length, 5);
    assert.equal(outcomes[0]!.ok, true);
    assert.equal(outcomes[1]!.ok, true);
    assert.equal(outcomes[2]!.ok, false);
    if (!outcomes[2]!.ok) {
      assert.ok(outcomes[2]!.error.message.includes("query failed"));
      assert.equal(outcomes[2]!.statement, "this is not valid powql");
    }
    assert.equal(outcomes[3]!.ok, true);
    assert.equal(outcomes[4]!.ok, true);
    if (outcomes[4]!.ok) {
      // Both inserts ran despite the failure in between.
      assert.equal(scalar(outcomes[4]!.result), "2");
    }
  });

  await test("pre-aborted signal fails fast with statementIndex 0", async () => {
    const ctrl = new AbortController();
    ctrl.abort();
    try {
      await client.execScript(`count(${items}); count(${items})`, {
        signal: ctrl.signal,
      });
      assert.fail("should have rejected");
    } catch (err) {
      assert.ok(isPowDBScriptError(err), `expected PowDBScriptError, got ${err}`);
      assert.equal(err.code, "aborted");
      assert.equal(err.statementIndex, 0);
      assert.equal(err.results.length, 0);
    }
    // The connection is still healthy afterwards (nothing was dispatched).
    assert.equal(scalar(await client.query(`count(${items})`)), "10");
  });

  await test("pre-aborted signal with continueOnError marks every statement", async () => {
    const ctrl = new AbortController();
    ctrl.abort();
    const outcomes = await client.execScript(
      `count(${items}); count(${items})`,
      { continueOnError: true, signal: ctrl.signal },
    );
    assert.equal(outcomes.length, 2);
    for (const o of outcomes) {
      assert.equal(o.ok, false);
    }
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nexecScript — transactional");
  // ──────────────────────────────────────────────────────────

  const tx = tbl("TX");
  await client.query(`type ${tx} { required name: str }`);

  await test("transactional script commits atomically on success", async () => {
    const results = await client.execScript(
      `
        insert ${tx} { name := "a" };
        insert ${tx} { name := "b" };
        count(${tx})
      `,
      { transactional: true },
    );
    // begin/commit replies are not part of the returned results.
    assert.equal(results.length, 3);
    assert.equal(scalar(results[2]!), "2");
    assert.equal(scalar(await client.query(`count(${tx})`)), "2");
  });

  await test("transactional script rolls back on failure — no partial work survives", async () => {
    const tr = tbl("TR");
    await client.query(`type ${tr} { required name: str }`);
    try {
      await client.execScript(
        `
          insert ${tr} { name := "first" };
          this is not valid powql;
          insert ${tr} { name := "second" }
        `,
        { transactional: true },
      );
      assert.fail("should have rejected");
    } catch (err) {
      assert.ok(isPowDBScriptError(err), `expected PowDBScriptError, got ${err}`);
      assert.equal(err.statementIndex, 1);
      assert.equal(err.results.length, 1);
    }
    // The insert before the failure (and the pipelined one after it) must
    // NOT survive — this is exactly the partial-commit hazard of embedding
    // begin/commit in a pipelined script yourself.
    assert.equal(scalar(await client.query(`count(${tr})`)), "0");
    // The connection is out of the transaction and healthy afterwards.
    assert.equal(scalar(await client.query(`count(${items})`)), "10");
  });

  await test("transactional rejects scripts containing their own transaction control", async () => {
    // "# note\ncommit" would slip past a naive check but the server's lexer
    // skips the comment and executes the commit — the guard must catch it.
    for (const stmt of ["begin", "COMMIT", "Rollback", "# note\ncommit", "  # a\n# b\n begin"]) {
      try {
        await client.execScript(`${stmt}; count(${items})`, {
          transactional: true,
        });
        assert.fail("should have rejected");
      } catch (err: any) {
        assert.equal(err.code, "protocol_error");
        assert.ok(err.message.includes("transaction control"));
      }
    }
    // Nothing was dispatched — the connection is still healthy.
    assert.equal(scalar(await client.query(`count(${items})`)), "10");
  });

  await test("transactional and continueOnError are mutually exclusive", async () => {
    try {
      await client.execScript(`count(${items})`, {
        transactional: true,
        continueOnError: true,
      } as never);
      assert.fail("should have rejected");
    } catch (err: any) {
      assert.equal(err.code, "protocol_error");
      assert.ok(err.message.includes("mutually exclusive"));
    }
  });

  await test("pre-aborted signal in transactional mode leaves no open transaction", async () => {
    const ta = tbl("TA");
    await client.query(`type ${ta} { required name: str }`);
    const ctrl = new AbortController();
    ctrl.abort();
    try {
      await client.execScript(`insert ${ta} { name := "x" }`, {
        transactional: true,
        signal: ctrl.signal,
      });
      assert.fail("should have rejected");
    } catch (err) {
      assert.ok(isPowDBScriptError(err), `expected PowDBScriptError, got ${err}`);
      assert.equal(err.code, "aborted");
    }
    assert.equal(scalar(await client.query(`count(${ta})`)), "0");
    // A follow-up write commits normally — proof no transaction was left
    // open by the aborted script.
    await client.query(`insert ${ta} { name := "after" }`);
    assert.equal(scalar(await client.query(`count(${ta})`)), "1");
  });

  // ──────────────────────────────────────────────────────────
  console.log("\nPool.execScript");
  // ──────────────────────────────────────────────────────────

  await test("runs the whole script on one pooled connection and releases it", async () => {
    const pool = new Pool({ host: HOST, port: PORT, max: 1 });
    try {
      const pl = tbl("PL");
      const results = await pool.execScript(`
        type ${pl} { required name: str };
        insert ${pl} { name := "a" };
        count(${pl})
      `);
      assert.equal(results.length, 3);
      assert.equal(scalar(results[2]!), "1");
      assert.equal(pool.idle, 1, "client should be released back to the pool");

      // Fail-fast errors destroy the checked-out client (withClient policy).
      try {
        await pool.execScript("this is not valid powql");
        assert.fail("should have rejected");
      } catch (err) {
        assert.ok(isPowDBScriptError(err));
      }
      assert.equal(pool.idle, 0, "failed script's client should be destroyed");

      // ...and the pool still hands out a fresh connection afterwards.
      const again = await pool.execScript(`count(${pl})`);
      assert.equal(scalar(again[0]!), "1");
    } finally {
      await pool.close();
    }
  });

  await test("continueOnError works through the pool", async () => {
    const pool = new Pool({ host: HOST, port: PORT, max: 1 });
    try {
      const outcomes = await pool.execScript(
        `count(${items}); this is not valid powql; count(${items})`,
        { continueOnError: true },
      );
      assert.equal(outcomes.length, 3);
      assert.equal(outcomes[0]!.ok, true);
      assert.equal(outcomes[1]!.ok, false);
      assert.equal(outcomes[2]!.ok, true);
      assert.equal(pool.idle, 1, "continue-mode script resolves, so release");
    } finally {
      await pool.close();
    }
  });

  await test("transactional works through the pool (rollback on failure)", async () => {
    const pool = new Pool({ host: HOST, port: PORT, max: 1 });
    try {
      const tp = tbl("TP");
      await pool.withClient((c) => c.query(`type ${tp} { required name: str }`));
      try {
        await pool.execScript(
          `insert ${tp} { name := "a" }; this is not valid powql`,
          { transactional: true },
        );
        assert.fail("should have rejected");
      } catch (err) {
        assert.ok(isPowDBScriptError(err));
      }
      const [count] = await pool.execScript(`count(${tp})`, {
        transactional: true,
      });
      assert.equal(scalar(count!), "0", "rolled-back insert must not survive");
    } finally {
      await pool.close();
    }
  });

  // ──────────────────────────────────────────────────────────
  console.log("\neager connect + execScript");
  // ──────────────────────────────────────────────────────────

  await test("a fresh eager connection runs a script in one round trip", async () => {
    const eager = await Client.connect({ host: HOST, port: PORT, eager: true });
    try {
      // Connect frame + every statement all go out before any reply.
      const writes = instrumentWrites(eager);
      const p = eager.execScript(`count(${items}); count(${items})`);
      assert.equal(
        writes.count(),
        2,
        "both statements pipelined right behind the Connect frame",
      );
      const results = await p;
      assert.equal(scalar(results[0]!), "10");
      assert.equal(scalar(results[1]!), "10");
    } finally {
      await eager.close();
    }
  });

  // ──────────────────────────────────────────────────────────
  // Cleanup
  // ──────────────────────────────────────────────────────────

  for (const t of ["Items", "Seq", "Notes", "FF", "CE", "PL", "TX", "TR", "TA", "TP"]) {
    await client.query(`drop ${tbl(t)}`).catch(() => {});
  }
  await client.close();

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
