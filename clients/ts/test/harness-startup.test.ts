import { strict as assert } from "node:assert";
import { Client } from "../src/index.js";

const HOST = process.env.POWDB_HOST ?? "127.0.0.1";
const PORT = Number(process.env.POWDB_PORT ?? "0");
const SOCKET = process.env.POWDB_SOCKET;

async function main() {
  assert.ok(PORT > 0, "run-with-server must provide the server's bound TCP port");

  const tcp = await Client.connect({ host: HOST, port: PORT });
  assert.ok(tcp.serverVersion, "TCP connection should complete the server handshake");
  await tcp.close();

  assert.ok(SOCKET, "run-with-server must expose the Unix socket path");
  const uds = await Client.connect({ path: SOCKET });
  assert.ok(uds.serverVersion, "Unix socket connection should complete the server handshake");
  await uds.close();

  console.log(`harness startup ok on ${HOST}:${PORT} and ${SOCKET}`);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
