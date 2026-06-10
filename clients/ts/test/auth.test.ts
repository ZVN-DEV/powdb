/**
 * Live multi-user authentication tests for the PowDB TypeScript client.
 *
 * Spawns its own powdb-server instances (this suite needs a data dir seeded
 * with named users via `powdb-cli useradd`, so it cannot reuse the plain
 * run-with-server harness):
 *
 *   - port 7771: multi-user server (alice=readwrite, bob=readonly)
 *   - port 7772: legacy server (no users, no password) for back-compat
 *
 * Uses the prebuilt binaries in ../../target/release when present, falling
 * back to `cargo run --release` otherwise. Data dirs live under
 * /tmp/powdb-sweep/ts-auth/ and are wiped on entry and exit.
 *
 * Run with:
 *   pnpm run test:auth
 *
 * Role enforcement (the readonly rejection assertions) requires a server
 * with RBAC enforcement (≥0.4.6 / the ecosystem-sweep build).
 */

import * as fs from "node:fs";
import * as fsp from "node:fs/promises";
import * as net from "node:net";
import * as path from "node:path";
import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { fileURLToPath } from "node:url";
import { strict as assert } from "node:assert";
import { Client } from "../src/index.js";
import { isPowDBError } from "../src/errors.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "..", "..", "..");
const HOST = "127.0.0.1";
const MULTI_PORT = 7771;
const LEGACY_PORT = 7772;
const BASE_DIR = "/tmp/powdb-sweep/ts-auth";
const MULTI_DIR = path.join(BASE_DIR, "multi");
const LEGACY_DIR = path.join(BASE_DIR, "legacy");

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

// ───── server / cli plumbing ────────────────────────────────────────────────

function bin(name: string): { cmd: string; prefix: string[] } {
  const release = path.join(repoRoot, "target", "release", name);
  if (fs.existsSync(release)) {
    return { cmd: release, prefix: [] };
  }
  // Fall back to cargo (slower, but works on a fresh checkout).
  return {
    cmd: "cargo",
    prefix: ["run", "--release", "-p", name, "--"],
  };
}

function runCli(args: string[]): void {
  const { cmd, prefix } = bin("powdb-cli");
  const r = spawnSync(cmd, [...prefix, ...args], {
    cwd: repoRoot,
    encoding: "utf8",
  });
  if (r.status !== 0) {
    throw new Error(
      `powdb-cli ${args.join(" ")} failed (${r.status}): ${r.stderr}`,
    );
  }
}

function canConnect(port: number): Promise<boolean> {
  return new Promise((resolve) => {
    const socket = new net.Socket();
    let done = false;
    const finish = (ok: boolean) => {
      if (done) return;
      done = true;
      socket.destroy();
      resolve(ok);
    };
    socket.setTimeout(250);
    socket.once("connect", () => finish(true));
    socket.once("timeout", () => finish(false));
    socket.once("error", () => finish(false));
    socket.connect(port, HOST);
  });
}

async function startServer(
  port: number,
  dataDir: string,
): Promise<ChildProcess> {
  const { cmd, prefix } = bin("powdb-server");
  const child = spawn(
    cmd,
    [...prefix, "--bind", HOST, "--port", String(port), "--data-dir", dataDir],
    { cwd: repoRoot, stdio: ["ignore", "pipe", "pipe"] },
  );
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      throw new Error(`powdb-server exited early with code ${child.exitCode}`);
    }
    if (await canConnect(port)) return child;
    await new Promise((r) => setTimeout(r, 200));
  }
  child.kill("SIGKILL");
  throw new Error(`timed out waiting for powdb-server on ${HOST}:${port}`);
}

async function stopServer(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null) return;
  child.kill("SIGINT");
  await new Promise<void>((resolve) => {
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      resolve();
    }, 5_000);
    child.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
  });
}

// ───── suite ────────────────────────────────────────────────────────────────

async function main() {
  await fsp.rm(BASE_DIR, { recursive: true, force: true });
  await fsp.mkdir(MULTI_DIR, { recursive: true });
  await fsp.mkdir(LEGACY_DIR, { recursive: true });

  // Seed the multi-user store offline, exactly as documented in
  // docs/getting-started.md ("Multi-user authentication").
  runCli(["--data-dir", MULTI_DIR, "useradd", "alice", "--role", "readwrite", "--password", "s3cret"]);
  runCli(["--data-dir", MULTI_DIR, "useradd", "bob", "--role", "readonly", "--password", "hunter2"]);

  console.log(`\nStarting multi-user server on ${HOST}:${MULTI_PORT}...`);
  const multiServer = await startServer(MULTI_PORT, MULTI_DIR);
  console.log(`Starting legacy (no-users) server on ${HOST}:${LEGACY_PORT}...`);
  const legacyServer = await startServer(LEGACY_PORT, LEGACY_DIR);

  try {
    console.log("\nMulti-user server — readwrite (alice)");

    let alice: Client | null = null;

    await test("alice (readwrite) authenticates", async () => {
      alice = await Client.connect({
        host: HOST,
        port: MULTI_PORT,
        user: "alice",
        password: "s3cret",
      });
      assert.ok(alice.serverVersion, "should report a server version");
    });

    await test("alice can create a type and insert", async () => {
      assert.ok(alice, "alice must be connected");
      const ddl = await alice.query(
        "type AuthUsers { required name: str, age: int }",
      );
      assert.equal(ddl.kind, "message");
      const ins = await alice.query(
        'insert AuthUsers { name := "Zed", age := 41 }',
      );
      assert.equal(ins.kind, "ok");
      if (ins.kind === "ok") assert.equal(Number(ins.affected), 1);
    });

    console.log("\nMulti-user server — readonly (bob)");

    let bob: Client | null = null;

    await test("bob (readonly) authenticates", async () => {
      bob = await Client.connect({
        host: HOST,
        port: MULTI_PORT,
        user: "bob",
        password: "hunter2",
      });
      assert.ok(bob.serverVersion);
    });

    await test("bob can read", async () => {
      assert.ok(bob, "bob must be connected");
      const r = await bob.query("AuthUsers { .name, .age }");
      assert.equal(r.kind, "rows");
      if (r.kind === "rows") {
        assert.equal(r.rows.length, 1);
        assert.equal(r.rows[0]![0], "Zed");
      }
    });

    await test("bob's insert is rejected with permission denied (no crash)", async () => {
      assert.ok(bob, "bob must be connected");
      try {
        await bob.query('insert AuthUsers { name := "Mallory", age := 99 }');
        assert.fail("readonly insert should have been rejected");
      } catch (err) {
        assert.ok(isPowDBError(err), `expected PowDBError, got ${err}`);
        assert.equal(err.code, "query_failed");
        assert.ok(
          /permission denied/i.test(err.message),
          `expected 'permission denied', got: ${err.message}`,
        );
        console.log(`    server said: ${err.message}`);
      }
      // The connection must survive the rejection — reads still work.
      const r = await bob.query("count(AuthUsers)");
      assert.equal(r.kind, "scalar");
      if (r.kind === "scalar") assert.equal(r.value, "1");
    });

    await test("bob's write never landed (count still 1 via alice)", async () => {
      assert.ok(alice, "alice must be connected");
      const r = await alice.query("count(AuthUsers)");
      assert.equal(r.kind, "scalar");
      if (r.kind === "scalar") assert.equal(r.value, "1");
    });

    console.log("\nMulti-user server — rejected connects");

    await test("connect with no user fails with auth_failed", async () => {
      try {
        await Client.connect({ host: HOST, port: MULTI_PORT });
        assert.fail("connect without a username should have been rejected");
      } catch (err) {
        assert.ok(isPowDBError(err), `expected PowDBError, got ${err}`);
        assert.equal(err.code, "auth_failed");
        console.log(`    server said: ${err.message}`);
      }
    });

    await test("connect with wrong password fails with auth_failed", async () => {
      try {
        await Client.connect({
          host: HOST,
          port: MULTI_PORT,
          user: "alice",
          password: "wrong",
        });
        assert.fail("wrong password should have been rejected");
      } catch (err) {
        assert.ok(isPowDBError(err), `expected PowDBError, got ${err}`);
        assert.equal(err.code, "auth_failed");
      }
    });

    await test("connect as unknown user fails with auth_failed", async () => {
      try {
        await Client.connect({
          host: HOST,
          port: MULTI_PORT,
          user: "mallory",
          password: "s3cret",
        });
        assert.fail("unknown user should have been rejected");
      } catch (err) {
        assert.ok(isPowDBError(err), `expected PowDBError, got ${err}`);
        assert.equal(err.code, "auth_failed");
      }
    });

    console.log("\nLegacy (no-users) server — back-compat");

    await test("connect without user works against a no-auth server", async () => {
      const c = await Client.connect({ host: HOST, port: LEGACY_PORT });
      const r = await c.query("type LegacyT { required name: str }");
      assert.equal(r.kind, "message");
      const ins = await c.query('insert LegacyT { name := "ok" }');
      assert.equal(ins.kind, "ok");
      await c.close();
    });

    if (alice !== null) await (alice as Client).close();
    if (bob !== null) await (bob as Client).close();
  } finally {
    await stopServer(multiServer);
    await stopServer(legacyServer);
    await fsp.rm(BASE_DIR, { recursive: true, force: true });
  }

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
