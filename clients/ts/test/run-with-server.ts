/**
 * Run a live TypeScript client test with a disposable PowDB server.
 *
 * If POWDB_PORT is already set, this script assumes the caller supplied a
 * server and just runs the target test. Otherwise it starts powdb-server from
 * the repo with `--port 0 --port-file`, waits until the server publishes the
 * actual OS-assigned port, and tears it down afterward.
 */

import * as fs from "node:fs/promises";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const clientRoot = path.resolve(here, "..");
const repoRoot = path.resolve(clientRoot, "..", "..");
const host = process.env.POWDB_HOST ?? "127.0.0.1";

function serverCommand(): { cmd: string; prefix: string[] } {
  const serverBin = process.env.POWDB_SERVER_BIN;
  if (serverBin && serverBin.length > 0) return { cmd: serverBin, prefix: [] };
  return {
    cmd: "cargo",
    prefix: ["run", "--release", "-p", "powdb-server", "--"],
  };
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
    socket.connect(port, host);
  });
}

function parsePublishedPort(text: string): number | undefined {
  for (const line of text.split(/\r?\n/)) {
    const value = line.match(/^port=(\d+)$/)?.[1];
    if (value === undefined) continue;
    const port = Number(value);
    if (Number.isInteger(port) && port > 0 && port <= 65_535) return port;
  }
  return undefined;
}

async function waitForPublishedPort(
  portFile: string,
  child: ChildProcessWithoutNullStreams,
): Promise<number> {
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      throw new Error(`powdb-server exited early with code ${child.exitCode}`);
    }
    try {
      const text = await fs.readFile(portFile, "utf8");
      const port = parsePublishedPort(text);
      if (port !== undefined) return port;
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code !== "ENOENT") throw err;
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`timed out waiting for powdb-server to publish ${portFile}`);
}

async function waitForServer(port: number, child: ChildProcessWithoutNullStreams) {
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      throw new Error(`powdb-server exited early with code ${child.exitCode}`);
    }
    if (await canConnect(port)) return;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`timed out waiting for powdb-server on ${host}:${port}`);
}

async function runCommand(
  cmd: string,
  args: string[],
  env: NodeJS.ProcessEnv,
): Promise<number> {
  return await new Promise((resolve) => {
    const child = spawn(cmd, args, {
      cwd: clientRoot,
      env,
      stdio: "inherit",
    });
    child.once("exit", (code, signal) => {
      if (signal) resolve(1);
      else resolve(code ?? 1);
    });
  });
}

async function stopServer(child: ChildProcessWithoutNullStreams): Promise<void> {
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

async function main() {
  const target = process.argv[2] ?? "test/client.test.ts";
  const targetArgs = process.argv.slice(3);

  if (process.env.POWDB_PORT) {
    const code = await runCommand("tsx", [target, ...targetArgs], process.env);
    process.exit(code);
  }

  const dataDir = await fs.mkdtemp(path.join(os.tmpdir(), "powdb-ts-test-"));
  const portFile = path.join(dataDir, "ports");
  // Also expose a Unix domain socket so the client's `{ path }` connection
  // mode is exercised end-to-end alongside TCP.
  const socketPath = path.join(dataDir, "powdb.sock");
  const serverLog: string[] = [];
  const { cmd, prefix } = serverCommand();
  const server = spawn(
    cmd,
    [
      ...prefix,
      "--bind",
      host,
      "--port",
      "0",
      "--port-file",
      portFile,
      "--socket",
      socketPath,
      "--data-dir",
      dataDir,
    ],
    {
      cwd: repoRoot,
      env: process.env,
      stdio: ["ignore", "pipe", "pipe"],
    },
  );

  const capture = (chunk: Buffer) => {
    for (const line of chunk.toString("utf8").split(/\r?\n/)) {
      if (line.trim().length === 0) continue;
      serverLog.push(line);
      if (serverLog.length > 80) serverLog.shift();
    }
  };
  server.stdout.on("data", capture);
  server.stderr.on("data", capture);

  try {
    const port = await waitForPublishedPort(portFile, server);
    await waitForServer(port, server);
    console.log(`Started disposable PowDB server on ${host}:${port} (from ${portFile})`);
    const code = await runCommand("tsx", [target, ...targetArgs], {
      ...process.env,
      POWDB_HOST: host,
      POWDB_PORT: String(port),
      POWDB_SOCKET: socketPath,
    });
    if (code !== 0) {
      console.error("\nPowDB server log tail:");
      for (const line of serverLog) console.error(line);
    }
    process.exitCode = code;
  } catch (err) {
    console.error(err);
    console.error("\nPowDB server log tail:");
    for (const line of serverLog) console.error(line);
    process.exitCode = 1;
  } finally {
    await stopServer(server);
    await fs.rm(dataDir, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
