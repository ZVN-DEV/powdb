/**
 * Server-backed statement-atomicity regressions for public TypeScript APIs.
 *
 *     POWDB_SERVER_BIN=../../target/release/powdb-server \
 *       pnpm test:statement-atomicity
 */

import { strict as assert } from "node:assert";
import {
  Client,
  Pool,
  isPowDBError,
  isPowDBScriptError,
  type NativeQueryResult,
  type QueryResult,
} from "../src/index.js";

const HOST = "127.0.0.1";
const PORT = Number(process.env.POWDB_PORT || "5433");
const PREFIX = `Atomic_${Date.now().toString(36)}`;

let passed = 0;
let failed = 0;
const failures: string[] = [];

function tbl(name: string): string {
  return `${PREFIX}_${name}`;
}

async function test(name: string, fn: () => Promise<void>) {
  try {
    await fn();
    passed++;
    console.log(`  ✓ ${name}`);
  } catch (err: any) {
    failed++;
    failures.push(`${name}: ${err?.stack || err?.message || err}`);
    console.log(`  ✗ ${name}`);
    console.log(`    ${err?.message || err}`);
  }
}

function scalar(result: QueryResult): string {
  assert.equal(result.kind, "scalar", `expected scalar, got ${result.kind}`);
  return result.value;
}

function rows(result: NativeQueryResult): unknown[][] {
  assert.equal(result.kind, "rows", `expected rows, got ${result.kind}`);
  return result.rows;
}

async function expectPowDBError(
  fn: () => Promise<unknown>,
  re: RegExp,
): Promise<void> {
  try {
    await fn();
  } catch (err) {
    assert.ok(isPowDBError(err), `expected PowDBError, got ${err}`);
    assert.match(err.message, re);
    return;
  }
  throw new Error("expected query failure, got success");
}

async function main() {
  const client = await Client.connect({ host: HOST, port: PORT });
  try {
    console.log("\nstatement atomicity — TypeScript client");

    await test("autocommit late unique error leaves rows unchanged", async () => {
      const t = tbl("LateUnique");
      await client.query(`type ${t} { required unique id: int, name: str }`);
      await client.query(`insert ${t} { id := 1, name := "old" }`);

      await expectPowDBError(
        () =>
          client.query(
            `insert ${t} { id := 2, name := "would-leak" }, { id := 1, name := "dupe" }`,
          ),
        /unique|duplicate|already exists/i,
      );

      assert.deepEqual(rows(await client.queryNative(`${t} order .id { .id, .name }`)), [
        [1, "old"],
      ]);
    });

    await test("aborted transaction refuses native reads and commit until rollback", async () => {
      const t = tbl("ExplicitAbort");
      await client.query(`type ${t} { required unique id: int, name: str }`);

      await client.query("begin");
      await client.query(`insert ${t} { id := 1, name := "tx" }`);
      await expectPowDBError(
        () => client.query(`insert ${t} { id := 1, name := "dupe" }`),
        /unique|duplicate|already exists/i,
      );

      await expectPowDBError(
        () => client.queryNative(`${t} { .id }`),
        /explicit transaction is aborted/i,
      );
      await expectPowDBError(() => client.query("commit"), /explicit transaction is aborted/i);

      await client.query("rollback");
      assert.equal(scalar(await client.query(`count(${t})`)), "0");

      await client.query(`insert ${t} { id := 2, name := "after" }`);
      assert.deepEqual(rows(await client.queryNative(`${t} { .id, .name }`)), [
        [2, "after"],
      ]);
    });
  } finally {
    await client.close();
  }

  await test("pool transactional execScript rolls back and leaves pool reusable", async () => {
    const pool = new Pool({ host: HOST, port: PORT, max: 1 });
    const t = tbl("PoolScript");
    try {
      await pool.withClient((c) =>
        c.query(`type ${t} { required unique id: int, name: str }`),
      );

      try {
        await pool.execScript(
          `
          insert ${t} { id := 1, name := "script" };
          insert ${t} { id := 1, name := "dupe" }
          `,
          { transactional: true },
        );
        assert.fail("transactional script should have failed");
      } catch (err) {
        assert.ok(isPowDBScriptError(err), `expected PowDBScriptError, got ${err}`);
      }

      const [count] = await pool.execScript(`count(${t})`, { transactional: true });
      assert.equal(scalar(count!), "0", "rolled-back insert must not survive");

      await pool.execScript(`insert ${t} { id := 2, name := "after" }`, {
        transactional: true,
      });
      const [after] = await pool.execScript(`count(${t})`);
      assert.equal(scalar(after!), "1", "pool should remain usable after cleanup");
    } finally {
      await pool.close();
    }
  });

  console.log(`\n${passed} passed, ${failed} failed`);
  if (failures.length) {
    console.error(failures.join("\n"));
  }
  if (failed > 0) {
    process.exit(1);
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
