/**
 * PowDB wire protocol.
 *
 * Frame format: [type(1)][flags(1)][len(4 LE)][payload]
 * Strings are encoded as [len(4 LE)][utf-8 bytes].
 *
 * This mirrors crates/server/src/protocol.rs — keep them in sync.
 */

export const MSG_CONNECT = 0x01;
export const MSG_CONNECT_OK = 0x02;
export const MSG_QUERY = 0x03;
export const MSG_RESULT_ROWS = 0x07;
export const MSG_RESULT_SCALAR = 0x08;
export const MSG_RESULT_OK = 0x09;
export const MSG_ERROR = 0x0a;
export const MSG_RESULT_MSG = 0x0b;
export const MSG_DISCONNECT = 0x10;
export const MSG_PING = 0x11;
export const MSG_PONG = 0x12;

// ───── Size limits (mirror crates/server/src/protocol.rs) ──────────────────

/** Maximum payload size accepted from the wire (64 MB). */
export const MAX_PAYLOAD_SIZE = 64 * 1024 * 1024;

/** Maximum number of rows allowed in a single result message. */
export const MAX_ROWS = 10_000_000;

/** Maximum number of columns allowed in a result set. */
export const MAX_COLUMNS = 4096;

export type Message =
  | { type: "Connect"; dbName: string; password: string | null }
  | { type: "ConnectOk"; version: string }
  | { type: "Query"; query: string }
  | { type: "ResultRows"; columns: string[]; rows: string[][] }
  | { type: "ResultScalar"; value: string }
  | { type: "ResultOk"; affected: bigint }
  | { type: "ResultMessage"; message: string }
  | { type: "Error"; message: string }
  | { type: "Disconnect" }
  | { type: "Ping" }
  | { type: "Pong" };

// ───── Encoding ────────────────────────────────────────────────────────────

export function encode(msg: Message): Buffer {
  let msgType: number;
  let payload: Buffer;

  switch (msg.type) {
    case "Connect": {
      const dbBuf = encodeString(msg.dbName);
      const pwBuf =
        msg.password === null ? u32LE(0) : encodeString(msg.password);
      payload = Buffer.concat([dbBuf, pwBuf]);
      msgType = MSG_CONNECT;
      break;
    }
    case "ConnectOk":
      payload = encodeString(msg.version);
      msgType = MSG_CONNECT_OK;
      break;
    case "Query":
      payload = encodeString(msg.query);
      msgType = MSG_QUERY;
      break;
    case "ResultRows": {
      const parts: Buffer[] = [];
      const colCount = Buffer.alloc(2);
      colCount.writeUInt16LE(msg.columns.length, 0);
      parts.push(colCount);
      for (const col of msg.columns) parts.push(encodeString(col));
      parts.push(u32LE(msg.rows.length));
      for (const row of msg.rows) {
        for (const val of row) parts.push(encodeString(val));
      }
      payload = Buffer.concat(parts);
      msgType = MSG_RESULT_ROWS;
      break;
    }
    case "ResultScalar":
      payload = encodeString(msg.value);
      msgType = MSG_RESULT_SCALAR;
      break;
    case "ResultOk": {
      payload = Buffer.alloc(8);
      payload.writeBigUInt64LE(msg.affected, 0);
      msgType = MSG_RESULT_OK;
      break;
    }
    case "ResultMessage":
      payload = encodeString(msg.message);
      msgType = MSG_RESULT_MSG;
      break;
    case "Error":
      payload = encodeString(msg.message);
      msgType = MSG_ERROR;
      break;
    case "Disconnect":
      payload = Buffer.alloc(0);
      msgType = MSG_DISCONNECT;
      break;
    case "Ping":
      payload = Buffer.alloc(0);
      msgType = MSG_PING;
      break;
    case "Pong":
      payload = Buffer.alloc(0);
      msgType = MSG_PONG;
      break;
  }

  const frame = Buffer.alloc(6 + payload.length);
  frame.writeUInt8(msgType, 0);
  frame.writeUInt8(0, 1); // flags
  frame.writeUInt32LE(payload.length, 2);
  payload.copy(frame, 6);
  return frame;
}

// ───── Decoding ────────────────────────────────────────────────────────────

/**
 * Attempts to parse a single frame from the start of `buf`. Returns the parsed
 * message and the number of bytes consumed, or `null` if the buffer does not
 * yet contain a complete frame.
 */
export function tryDecode(
  buf: Buffer,
): { msg: Message; consumed: number } | null {
  if (buf.length < 6) return null;
  const msgType = buf.readUInt8(0);
  // flags byte at offset 1 is currently unused
  const payloadLen = buf.readUInt32LE(2);
  if (payloadLen > MAX_PAYLOAD_SIZE) {
    throw new Error(
      `payload too large: ${payloadLen} bytes (max ${MAX_PAYLOAD_SIZE})`,
    );
  }
  if (buf.length < 6 + payloadLen) return null;
  const payload = buf.subarray(6, 6 + payloadLen);
  const msg = decodePayload(msgType, payload);
  return { msg, consumed: 6 + payloadLen };
}

function decodePayload(msgType: number, payload: Buffer): Message {
  const cursor = { pos: 0 };
  switch (msgType) {
    case MSG_CONNECT: {
      const dbName = decodeString(payload, cursor);
      let password: string | null = null;
      if (cursor.pos < payload.length) {
        const p = decodeString(payload, cursor);
        password = p.length === 0 ? null : p;
      }
      return { type: "Connect", dbName, password };
    }
    case MSG_CONNECT_OK:
      return { type: "ConnectOk", version: decodeString(payload, cursor) };
    case MSG_QUERY:
      return { type: "Query", query: decodeString(payload, cursor) };
    case MSG_RESULT_ROWS: {
      const colCount = readU16(payload, cursor, "column count");
      if (colCount > MAX_COLUMNS) {
        throw new Error(
          `too many columns: ${colCount} (max ${MAX_COLUMNS})`,
        );
      }
      const columns: string[] = [];
      for (let i = 0; i < colCount; i++) {
        columns.push(decodeString(payload, cursor));
      }
      const rowCount = readU32(payload, cursor, "row count");
      if (rowCount > MAX_ROWS) {
        throw new Error(
          `too many rows: ${rowCount} (max ${MAX_ROWS})`,
        );
      }
      const rows: string[][] = [];
      for (let r = 0; r < rowCount; r++) {
        const row: string[] = [];
        for (let c = 0; c < colCount; c++) {
          row.push(decodeString(payload, cursor));
        }
        rows.push(row);
      }
      return { type: "ResultRows", columns, rows };
    }
    case MSG_RESULT_SCALAR:
      return { type: "ResultScalar", value: decodeString(payload, cursor) };
    case MSG_RESULT_OK: {
      const affected = readU64(payload, cursor, "affected count");
      return { type: "ResultOk", affected };
    }
    case MSG_RESULT_MSG:
      return { type: "ResultMessage", message: decodeString(payload, cursor) };
    case MSG_ERROR:
      return { type: "Error", message: decodeString(payload, cursor) };
    case MSG_DISCONNECT:
      return { type: "Disconnect" };
    case MSG_PING:
      return { type: "Ping" };
    case MSG_PONG:
      return { type: "Pong" };
    default:
      throw new Error(`unknown message type: 0x${msgType.toString(16)}`);
  }
}

// ───── String helpers ──────────────────────────────────────────────────────

function encodeString(s: string): Buffer {
  const bytes = Buffer.from(s, "utf8");
  const out = Buffer.alloc(4 + bytes.length);
  out.writeUInt32LE(bytes.length, 0);
  bytes.copy(out, 4);
  return out;
}

function decodeString(buf: Buffer, cursor: { pos: number }): string {
  if (cursor.pos + 4 > buf.length) {
    throw new Error("truncated string length");
  }
  const len = readU32(buf, cursor, "string length");
  if (cursor.pos + len > buf.length) {
    throw new Error("truncated string data");
  }
  const s = buf.toString("utf8", cursor.pos, cursor.pos + len);
  cursor.pos += len;
  return s;
}

function readU16(buf: Buffer, cursor: { pos: number }, label: string): number {
  if (cursor.pos + 2 > buf.length) {
    throw new Error(`truncated ${label}`);
  }
  const value = buf.readUInt16LE(cursor.pos);
  cursor.pos += 2;
  return value;
}

function readU32(buf: Buffer, cursor: { pos: number }, label: string): number {
  if (cursor.pos + 4 > buf.length) {
    throw new Error(`truncated ${label}`);
  }
  const value = buf.readUInt32LE(cursor.pos);
  cursor.pos += 4;
  return value;
}

function readU64(buf: Buffer, cursor: { pos: number }, label: string): bigint {
  if (cursor.pos + 8 > buf.length) {
    throw new Error(`truncated ${label}`);
  }
  const value = buf.readBigUInt64LE(cursor.pos);
  cursor.pos += 8;
  return value;
}

function u32LE(n: number): Buffer {
  const b = Buffer.alloc(4);
  b.writeUInt32LE(n, 0);
  return b;
}
