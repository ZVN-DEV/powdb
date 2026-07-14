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
/**
 * Query carrying positional `$N` parameters. Pure protocol addition: old
 * servers reject it with the existing "unknown message type" error, and the
 * plain `MSG_QUERY` frame is unchanged. Requires powdb-server ≥ 0.4.7.
 */
export const MSG_QUERY_PARAMS = 0x04;
/** SQL query frame. Plain Query remains PowQL for backward compatibility. */
export const MSG_QUERY_SQL = 0x05;
/** Private embedded-sync control frames. Requires authenticated server support. */
export const MSG_SYNC_STATUS = 0x20;
export const MSG_SYNC_PULL = 0x21;
export const MSG_SYNC_ACK = 0x22;
export const MSG_SYNC_STATUS_RESULT = 0x23;
export const MSG_SYNC_PULL_RESULT = 0x24;
export const MSG_SYNC_ACK_RESULT = 0x25;
export const MSG_RESULT_ROWS = 0x07;
export const MSG_RESULT_SCALAR = 0x08;
export const MSG_RESULT_OK = 0x09;
export const MSG_ERROR = 0x0a;
export const MSG_RESULT_MSG = 0x0b;
export const MSG_DISCONNECT = 0x10;
export const MSG_PING = 0x11;
export const MSG_PONG = 0x12;
export const MSG_QUERY_NATIVE = 0x13;
export const MSG_QUERY_PARAMS_NATIVE = 0x14;
export const MSG_QUERY_SQL_NATIVE = 0x15;
export const MSG_RESULT_ROWS_NATIVE = 0x16;
export const MSG_RESULT_SCALAR_NATIVE = 0x17;

// ───── Size limits (mirror crates/server/src/protocol.rs) ──────────────────

/** Maximum payload size accepted from the wire (64 MB). */
export const MAX_PAYLOAD_SIZE = 64 * 1024 * 1024;

/** Maximum number of rows allowed in a single result message. */
export const MAX_ROWS = 10_000_000;

/** Maximum number of columns allowed in a result set. */
export const MAX_COLUMNS = 4096;

/** Maximum number of bound parameters in a QueryWithParams message. */
export const MAX_PARAMS = 4096;

/** Maximum retained units accepted in one sync pull result. */
export const MAX_SYNC_UNITS = 4096;

/** Maximum retained units accepted by the server for one sync pull request. */
export const MAX_SYNC_PULL_UNITS = 4096;

/** Maximum retained-unit payload budget accepted by the server for one sync pull. */
export const MAX_SYNC_PULL_BYTES = 16 * 1024 * 1024;

/**
 * A positional parameter value for {@link MSG_QUERY_PARAMS}. Wire encoding
 * per param: a 1-byte tag followed by the body —
 *   `0` null (no body), `1` int (8B LE i64), `2` float (8B LE f64),
 *   `3` bool (1B), `4` str (length-prefixed UTF-8).
 *
 * Integral numbers are sent as ints, non-integral numbers as floats; `null`
 * binds PowQL `null`. `bigint` is always an int.
 */
export type WireParam =
  | { tag: "null" }
  | { tag: "int"; value: bigint }
  | { tag: "float"; value: number }
  | { tag: "bool"; value: boolean }
  | { tag: "str"; value: string };

/** Recursively decoded PJ1 JSON. Integers outside JS's safe range are bigint. */
export type NativeJson =
  | null
  | boolean
  | number
  | bigint
  | string
  | NativeJson[]
  | { [key: string]: NativeJson };

/** Lossless typed cell used internally by the native result protocol. */
export type WireValue =
  | { type: "empty" }
  | { type: "int"; value: bigint }
  | { type: "float"; value: number }
  | { type: "bool"; value: boolean }
  | { type: "str"; value: string }
  | { type: "datetime"; value: bigint }
  | { type: "uuid"; value: Uint8Array }
  | { type: "bytes"; value: Uint8Array }
  | { type: "json"; value: NativeJson; pj1: Uint8Array };

export type SyncRepairAction =
  | "none"
  | "pull"
  | "awaitArchive"
  | "rebootstrap";

export interface WireSyncStatus {
  replicaId: string;
  active: boolean;
  lastAppliedLsn: bigint | null;
  remoteLsn: bigint;
  servableLsn: bigint | null;
  unarchivedLsn: bigint | null;
  lagLsn: bigint | null;
  lagBytes: bigint | null;
  lagMs: bigint | null;
  stale: boolean;
  repairAction: SyncRepairAction;
  lastSyncError: string | null;
}

export interface WireRetainedUnit {
  txId: bigint;
  recordType: number;
  lsn: bigint;
  data: Buffer;
}

export type Message =
  | {
      type: "Connect";
      dbName: string;
      password: string | null;
      /**
       * Optional user name for multi-user authentication (server ≥0.4.6
       * enforces roles). Encoded as a length-prefixed string appended AFTER
       * the password field. When `null` the field is omitted entirely, so the
       * frame is byte-identical to the pre-username (0.3.x) shape and old
       * servers accept it unchanged.
       */
      username: string | null;
    }
  | { type: "ConnectOk"; version: string }
  | { type: "Query"; query: string }
  | { type: "QuerySql"; query: string }
  | { type: "QueryWithParams"; query: string; params: WireParam[] }
  | { type: "QueryNative"; query: string }
  | { type: "QueryWithParamsNative"; query: string; params: WireParam[] }
  | { type: "QuerySqlNative"; query: string }
  | { type: "SyncStatus"; replicaId: string }
  | {
      type: "SyncPull";
      replicaId: string;
      sinceLsn: bigint;
      maxUnits: number;
      maxBytes: bigint;
      databaseId: Buffer;
      primaryGeneration: bigint;
      walFormatVersion: number;
      catalogVersion: number;
      segmentFormatVersion: number;
    }
  | {
      type: "SyncAck";
      replicaId: string;
      appliedLsn: bigint;
      remoteLsn: bigint;
    }
  | { type: "SyncStatusResult"; status: WireSyncStatus }
  | {
      type: "SyncPullResult";
      status: WireSyncStatus;
      units: WireRetainedUnit[];
      hasMore: boolean;
    }
  | {
      type: "SyncAckResult";
      previousAppliedLsn: bigint;
      appliedLsn: bigint;
      remoteLsn: bigint;
      advanced: boolean;
      status: WireSyncStatus;
    }
  | { type: "ResultRows"; columns: string[]; rows: string[][] }
  | { type: "ResultScalar"; value: string }
  | { type: "ResultRowsNative"; columns: string[]; rows: WireValue[][] }
  | { type: "ResultScalarNative"; value: WireValue }
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
      // Username rides after the password (append-only extension, mirrors
      // crates/server/src/protocol.rs). The server reads it only when bytes
      // remain after the password, so omitting it when null keeps the frame
      // byte-identical to the legacy 0.3.x shape.
      const parts = [dbBuf, pwBuf];
      if (msg.username !== null) {
        parts.push(encodeString(msg.username));
      }
      payload = Buffer.concat(parts);
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
    case "QuerySql":
      payload = encodeString(msg.query);
      msgType = MSG_QUERY_SQL;
      break;
    case "QueryWithParams": {
      payload = encodeQueryWithParams(msg.query, msg.params);
      msgType = MSG_QUERY_PARAMS;
      break;
    }
    case "QueryNative":
      payload = encodeString(msg.query);
      msgType = MSG_QUERY_NATIVE;
      break;
    case "QueryWithParamsNative":
      payload = encodeQueryWithParams(msg.query, msg.params);
      msgType = MSG_QUERY_PARAMS_NATIVE;
      break;
    case "QuerySqlNative":
      payload = encodeString(msg.query);
      msgType = MSG_QUERY_SQL_NATIVE;
      break;
    case "SyncStatus":
      payload = encodeString(msg.replicaId);
      msgType = MSG_SYNC_STATUS;
      break;
    case "SyncPull": {
      if (msg.databaseId.length !== 16) {
        throw new Error(
          `sync databaseId must be exactly 16 bytes, got ${msg.databaseId.length}`,
        );
      }
      payload = Buffer.concat([
        encodeString(msg.replicaId),
        u64LE(msg.sinceLsn),
        u32LE(msg.maxUnits),
        u64LE(msg.maxBytes),
        msg.databaseId,
        u64LE(msg.primaryGeneration),
        u16LE(msg.walFormatVersion),
        u16LE(msg.catalogVersion),
        u16LE(msg.segmentFormatVersion),
      ]);
      msgType = MSG_SYNC_PULL;
      break;
    }
    case "SyncAck":
      payload = Buffer.concat([
        encodeString(msg.replicaId),
        u64LE(msg.appliedLsn),
        u64LE(msg.remoteLsn),
      ]);
      msgType = MSG_SYNC_ACK;
      break;
    case "SyncStatusResult":
      payload = encodeSyncStatus(msg.status);
      msgType = MSG_SYNC_STATUS_RESULT;
      break;
    case "SyncPullResult": {
      const parts: Buffer[] = [
        encodeSyncStatus(msg.status),
        u32LE(msg.units.length),
      ];
      for (const unit of msg.units) {
        parts.push(encodeRetainedUnit(unit));
      }
      parts.push(Buffer.from([msg.hasMore ? 1 : 0]));
      payload = Buffer.concat(parts);
      msgType = MSG_SYNC_PULL_RESULT;
      break;
    }
    case "SyncAckResult":
      payload = Buffer.concat([
        u64LE(msg.previousAppliedLsn),
        u64LE(msg.appliedLsn),
        u64LE(msg.remoteLsn),
        Buffer.from([msg.advanced ? 1 : 0]),
        encodeSyncStatus(msg.status),
      ]);
      msgType = MSG_SYNC_ACK_RESULT;
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
    case "ResultRowsNative": {
      const parts: Buffer[] = [u16LE(msg.columns.length)];
      for (const column of msg.columns) parts.push(encodeString(column));
      parts.push(u32LE(msg.rows.length));
      for (const row of msg.rows) {
        for (const value of row) parts.push(encodeWireValue(value));
      }
      payload = Buffer.concat(parts);
      msgType = MSG_RESULT_ROWS_NATIVE;
      break;
    }
    case "ResultScalarNative":
      payload = encodeWireValue(msg.value);
      msgType = MSG_RESULT_SCALAR_NATIVE;
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

function encodeQueryWithParams(query: string, params: WireParam[]): Buffer {
  const parts: Buffer[] = [encodeString(query), u16LE(params.length)];
  for (const param of params) {
    switch (param.tag) {
      case "null":
        parts.push(Buffer.from([0]));
        break;
      case "int": {
        const body = Buffer.alloc(9);
        body.writeUInt8(1, 0);
        body.writeBigInt64LE(param.value, 1);
        parts.push(body);
        break;
      }
      case "float": {
        const body = Buffer.alloc(9);
        body.writeUInt8(2, 0);
        body.writeDoubleLE(param.value, 1);
        parts.push(body);
        break;
      }
      case "bool":
        parts.push(Buffer.from([3, param.value ? 1 : 0]));
        break;
      case "str":
        parts.push(Buffer.from([4]), encodeString(param.value));
        break;
    }
  }
  return Buffer.concat(parts);
}

function encodeWireValue(value: WireValue): Buffer {
  let tag: number;
  let body: Buffer;
  switch (value.type) {
    case "empty":
      tag = 0;
      body = Buffer.alloc(0);
      break;
    case "int":
    case "datetime":
      tag = value.type === "int" ? 1 : 5;
      body = Buffer.alloc(8);
      body.writeBigInt64LE(value.value, 0);
      break;
    case "float":
      tag = 2;
      body = Buffer.alloc(8);
      body.writeDoubleLE(value.value, 0);
      break;
    case "bool":
      tag = 3;
      body = Buffer.from([value.value ? 1 : 0]);
      break;
    case "str":
      tag = 4;
      body = Buffer.from(value.value, "utf8");
      break;
    case "uuid":
      if (value.value.length !== 16) {
        throw new Error(`native UUID must be exactly 16 bytes, got ${value.value.length}`);
      }
      tag = 6;
      body = Buffer.from(value.value);
      break;
    case "bytes":
      tag = 7;
      body = Buffer.from(value.value);
      break;
    case "json":
      tag = 8;
      body = Buffer.from(value.pj1);
      decodePj1(body);
      break;
  }
  return Buffer.concat([Buffer.from([tag]), u32LE(body.length), body]);
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
      // Optional trailing username (mirror the server: present only when
      // bytes remain after the password; empty string means null).
      let username: string | null = null;
      if (cursor.pos < payload.length) {
        const u = decodeString(payload, cursor);
        username = u.length === 0 ? null : u;
      }
      return { type: "Connect", dbName, password, username };
    }
    case MSG_CONNECT_OK:
      return { type: "ConnectOk", version: decodeString(payload, cursor) };
    case MSG_QUERY:
      return { type: "Query", query: decodeString(payload, cursor) };
    case MSG_QUERY_SQL: {
      const query = decodeString(payload, cursor);
      return { type: "QuerySql", query };
    }
    case MSG_QUERY_PARAMS: {
      const query = decodeString(payload, cursor);
      const count = readU16(payload, cursor, "param count");
      if (count > MAX_PARAMS) {
        throw new Error(`too many parameters: ${count} (max ${MAX_PARAMS})`);
      }
      const params: WireParam[] = [];
      for (let i = 0; i < count; i++) {
        const tag = readU8(payload, cursor, "param tag");
        switch (tag) {
          case 0:
            params.push({ tag: "null" });
            break;
          case 1:
            params.push({ tag: "int", value: readI64(payload, cursor, "int param") });
            break;
          case 2:
            params.push({ tag: "float", value: readF64(payload, cursor, "float param") });
            break;
          case 3:
            params.push({ tag: "bool", value: readU8(payload, cursor, "bool param") !== 0 });
            break;
          case 4:
            params.push({ tag: "str", value: decodeString(payload, cursor) });
            break;
          default:
            throw new Error(`unknown param tag: ${tag}`);
        }
      }
      return { type: "QueryWithParams", query, params };
    }
    case MSG_QUERY_NATIVE: {
      const query = decodeStringStrict(payload, cursor, "native PowQL query");
      requirePayloadEnd(payload, cursor, "native PowQL query");
      return { type: "QueryNative", query };
    }
    case MSG_QUERY_PARAMS_NATIVE: {
      const { query, params } = decodeNativeQueryWithParams(payload, cursor);
      requirePayloadEnd(payload, cursor, "native parameterized query");
      return { type: "QueryWithParamsNative", query, params };
    }
    case MSG_QUERY_SQL_NATIVE: {
      const query = decodeStringStrict(payload, cursor, "native SQL query");
      requirePayloadEnd(payload, cursor, "native SQL query");
      return { type: "QuerySqlNative", query };
    }
    case MSG_SYNC_STATUS:
      return { type: "SyncStatus", replicaId: decodeString(payload, cursor) };
    case MSG_SYNC_PULL: {
      const replicaId = decodeString(payload, cursor);
      const sinceLsn = readU64(payload, cursor, "sync pull since LSN");
      const maxUnits = readU32(payload, cursor, "sync pull max units");
      const maxBytes = readU64(payload, cursor, "sync pull max bytes");
      const databaseId = readFixedBytes(payload, cursor, 16, "sync database id");
      const primaryGeneration = readU64(
        payload,
        cursor,
        "sync primary generation",
      );
      const walFormatVersion = readU16(
        payload,
        cursor,
        "sync WAL format version",
      );
      const catalogVersion = readU16(
        payload,
        cursor,
        "sync catalog version",
      );
      const segmentFormatVersion = readU16(
        payload,
        cursor,
        "sync segment format version",
      );
      return {
        type: "SyncPull",
        replicaId,
        sinceLsn,
        maxUnits,
        maxBytes,
        databaseId,
        primaryGeneration,
        walFormatVersion,
        catalogVersion,
        segmentFormatVersion,
      };
    }
    case MSG_SYNC_ACK: {
      const replicaId = decodeString(payload, cursor);
      const appliedLsn = readU64(payload, cursor, "sync ack applied LSN");
      const remoteLsn = readU64(payload, cursor, "sync ack remote LSN");
      return { type: "SyncAck", replicaId, appliedLsn, remoteLsn };
    }
    case MSG_SYNC_STATUS_RESULT:
      return {
        type: "SyncStatusResult",
        status: decodeSyncStatus(payload, cursor),
      };
    case MSG_SYNC_PULL_RESULT: {
      const status = decodeSyncStatus(payload, cursor);
      const count = readU32(payload, cursor, "sync retained unit count");
      if (count > MAX_SYNC_UNITS) {
        throw new Error(
          `too many retained units: ${count} (max ${MAX_SYNC_UNITS})`,
        );
      }
      const units: WireRetainedUnit[] = [];
      for (let i = 0; i < count; i++) {
        units.push(decodeRetainedUnit(payload, cursor));
      }
      const hasMore = readBool(payload, cursor, "sync has_more");
      return { type: "SyncPullResult", status, units, hasMore };
    }
    case MSG_SYNC_ACK_RESULT: {
      const previousAppliedLsn = readU64(
        payload,
        cursor,
        "previous applied LSN",
      );
      const appliedLsn = readU64(payload, cursor, "applied LSN");
      const remoteLsn = readU64(payload, cursor, "remote LSN");
      const advanced = readBool(payload, cursor, "sync ack advanced");
      const status = decodeSyncStatus(payload, cursor);
      return {
        type: "SyncAckResult",
        previousAppliedLsn,
        appliedLsn,
        remoteLsn,
        advanced,
        status,
      };
    }
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
      if (colCount === 0 && rowCount > 0) {
        throw new Error("nonzero row count with zero columns");
      }
      const remaining = BigInt(payload.length - cursor.pos);
      const minRowBytes = BigInt(rowCount) * BigInt(colCount) * 4n;
      if (remaining < minRowBytes) {
        throw new Error(
          `row data too short for declared shape: ${rowCount} rows x ${colCount} columns`,
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
    case MSG_RESULT_ROWS_NATIVE: {
      const colCount = readU16(payload, cursor, "native column count");
      if (colCount > MAX_COLUMNS) {
        throw new Error(`too many columns: ${colCount} (max ${MAX_COLUMNS})`);
      }
      const columns: string[] = [];
      for (let i = 0; i < colCount; i++) {
        columns.push(decodeStringStrict(payload, cursor, "native column name"));
      }
      const rowCount = readU32(payload, cursor, "native row count");
      if (rowCount > MAX_ROWS) {
        throw new Error(`too many rows: ${rowCount} (max ${MAX_ROWS})`);
      }
      if (colCount === 0 && rowCount > 0) {
        throw new Error("nonzero native row count with zero columns");
      }
      const minimumBytes = BigInt(rowCount) * BigInt(colCount) * 5n;
      if (BigInt(payload.length - cursor.pos) < minimumBytes) {
        throw new Error(
          `native row data too short for declared shape: ${rowCount} rows x ${colCount} columns`,
        );
      }
      const rows: WireValue[][] = [];
      for (let rowIndex = 0; rowIndex < rowCount; rowIndex++) {
        const row: WireValue[] = [];
        for (let columnIndex = 0; columnIndex < colCount; columnIndex++) {
          row.push(decodeWireValue(payload, cursor));
        }
        rows.push(row);
      }
      requirePayloadEnd(payload, cursor, "native rows");
      return { type: "ResultRowsNative", columns, rows };
    }
    case MSG_RESULT_SCALAR_NATIVE: {
      const value = decodeWireValue(payload, cursor);
      requirePayloadEnd(payload, cursor, "native scalar");
      return { type: "ResultScalarNative", value };
    }
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

function decodeNativeQueryWithParams(
  payload: Buffer,
  cursor: { pos: number },
): { query: string; params: WireParam[] } {
  const query = decodeStringStrict(payload, cursor, "native query");
  const count = readU16(payload, cursor, "native param count");
  if (count > MAX_PARAMS) {
    throw new Error(`too many parameters: ${count} (max ${MAX_PARAMS})`);
  }
  if (count > payload.length - cursor.pos) {
    throw new Error("parameter count exceeds payload size");
  }
  const params: WireParam[] = [];
  for (let index = 0; index < count; index++) {
    const tag = readU8(payload, cursor, "param tag");
    switch (tag) {
      case 0:
        params.push({ tag: "null" });
        break;
      case 1:
        params.push({ tag: "int", value: readI64(payload, cursor, "int param") });
        break;
      case 2:
        params.push({ tag: "float", value: readF64(payload, cursor, "float param") });
        break;
      case 3:
        params.push({ tag: "bool", value: readBool(payload, cursor, "bool param") });
        break;
      case 4:
        params.push({
          tag: "str",
          value: decodeStringStrict(payload, cursor, "string param"),
        });
        break;
      default:
        throw new Error(`unknown param tag: ${tag}`);
    }
  }
  return { query, params };
}

function decodeWireValue(buf: Buffer, cursor: { pos: number }): WireValue {
  const tag = readU8(buf, cursor, "typed value tag");
  const bodyLength = readU32(buf, cursor, "typed value body length");
  const body = readFixedBytes(buf, cursor, bodyLength, "typed value body");
  const requireLength = (expected: number, label: string): void => {
    if (body.length !== expected) {
      throw new Error(
        `invalid ${label} typed value length: expected ${expected}, got ${body.length}`,
      );
    }
  };

  switch (tag) {
    case 0:
      requireLength(0, "Empty");
      return { type: "empty" };
    case 1:
      requireLength(8, "Int");
      return { type: "int", value: body.readBigInt64LE(0) };
    case 2: {
      requireLength(8, "Float");
      return { type: "float", value: body.readDoubleLE(0) };
    }
    case 3:
      requireLength(1, "Bool");
      if (body[0] !== 0 && body[0] !== 1) {
        throw new Error(`invalid typed boolean: ${body[0]}`);
      }
      return { type: "bool", value: body[0] === 1 };
    case 4:
      return { type: "str", value: decodeUtf8Strict(body, "typed string") };
    case 5:
      requireLength(8, "DateTime");
      return { type: "datetime", value: body.readBigInt64LE(0) };
    case 6:
      requireLength(16, "UUID");
      return { type: "uuid", value: new Uint8Array(body) };
    case 7:
      return { type: "bytes", value: new Uint8Array(body) };
    case 8:
      return {
        type: "json",
        value: decodePj1(body),
        pj1: new Uint8Array(body),
      };
    default:
      throw new Error(`unknown typed value tag: ${tag}`);
  }
}

function requirePayloadEnd(
  payload: Buffer,
  cursor: { pos: number },
  label: string,
): void {
  if (cursor.pos !== payload.length) {
    throw new Error(`trailing bytes in ${label} payload`);
  }
}

function encodeSyncStatus(status: WireSyncStatus): Buffer {
  return Buffer.concat([
    encodeString(status.replicaId),
    Buffer.from([status.active ? 1 : 0]),
    encodeOptionalU64(status.lastAppliedLsn),
    u64LE(status.remoteLsn),
    encodeOptionalU64(status.servableLsn),
    encodeOptionalU64(status.unarchivedLsn),
    encodeOptionalU64(status.lagLsn),
    encodeOptionalU64(status.lagBytes),
    encodeOptionalU64(status.lagMs),
    Buffer.from([
      status.stale ? 1 : 0,
      encodeRepairAction(status.repairAction),
    ]),
    encodeOptionalString(status.lastSyncError),
  ]);
}

function decodeSyncStatus(buf: Buffer, cursor: { pos: number }): WireSyncStatus {
  const replicaId = decodeString(buf, cursor);
  const active = readBool(buf, cursor, "sync status active");
  const lastAppliedLsn = readOptionalU64(
    buf,
    cursor,
    "sync status last applied LSN",
  );
  const remoteLsn = readU64(buf, cursor, "sync status remote LSN");
  const servableLsn = readOptionalU64(buf, cursor, "sync status servable LSN");
  const unarchivedLsn = readOptionalU64(
    buf,
    cursor,
    "sync status unarchived LSN",
  );
  const lagLsn = readOptionalU64(buf, cursor, "sync status lag LSN");
  const lagBytes = readOptionalU64(buf, cursor, "sync status lag bytes");
  const lagMs = readOptionalU64(
    buf,
    cursor,
    "sync status lag milliseconds",
  );
  const stale = readBool(buf, cursor, "sync status stale");
  const repairAction = decodeRepairAction(
    readU8(buf, cursor, "sync repair action"),
  );
  const lastSyncError = readOptionalString(
    buf,
    cursor,
    "sync status last error",
  );
  return {
    replicaId,
    active,
    lastAppliedLsn,
    remoteLsn,
    servableLsn,
    unarchivedLsn,
    lagLsn,
    lagBytes,
    lagMs,
    stale,
    repairAction,
    lastSyncError,
  };
}

function encodeRetainedUnit(unit: WireRetainedUnit): Buffer {
  return Buffer.concat([
    u64LE(unit.txId),
    Buffer.from([u8(unit.recordType, "sync retained unit record type")]),
    u64LE(unit.lsn),
    encodeBytes(unit.data),
  ]);
}

function decodeRetainedUnit(
  buf: Buffer,
  cursor: { pos: number },
): WireRetainedUnit {
  const txId = readU64(buf, cursor, "sync retained unit tx id");
  const recordType = readU8(buf, cursor, "sync retained unit record type");
  const lsn = readU64(buf, cursor, "sync retained unit LSN");
  const data = readBytes(buf, cursor, "sync retained unit payload");
  return { txId, recordType, lsn, data };
}

function encodeRepairAction(action: SyncRepairAction): number {
  switch (action) {
    case "none":
      return 0;
    case "pull":
      return 1;
    case "awaitArchive":
      return 2;
    case "rebootstrap":
      return 3;
  }
}

function decodeRepairAction(raw: number): SyncRepairAction {
  switch (raw) {
    case 0:
      return "none";
    case 1:
      return "pull";
    case 2:
      return "awaitArchive";
    case 3:
      return "rebootstrap";
    default:
      throw new Error(`unknown sync repair action: ${raw}`);
  }
}

function encodeOptionalU64(value: bigint | null): Buffer {
  return value === null
    ? Buffer.from([0])
    : Buffer.concat([Buffer.from([1]), u64LE(value)]);
}

function readOptionalU64(
  buf: Buffer,
  cursor: { pos: number },
  label: string,
): bigint | null {
  return readBool(buf, cursor, label) ? readU64(buf, cursor, label) : null;
}

function encodeOptionalString(value: string | null): Buffer {
  return value === null
    ? Buffer.from([0])
    : Buffer.concat([Buffer.from([1]), encodeString(value)]);
}

function readOptionalString(
  buf: Buffer,
  cursor: { pos: number },
  label: string,
): string | null {
  return readBool(buf, cursor, label) ? decodeString(buf, cursor) : null;
}

function encodeBytes(bytes: Buffer): Buffer {
  return Buffer.concat([u32LE(bytes.length), bytes]);
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

const strictUtf8 = new TextDecoder("utf-8", { fatal: true });

function decodeUtf8Strict(bytes: Uint8Array, label: string): string {
  try {
    return strictUtf8.decode(bytes);
  } catch {
    throw new Error(`invalid UTF-8 in ${label}`);
  }
}

function decodeStringStrict(
  buf: Buffer,
  cursor: { pos: number },
  label: string,
): string {
  const len = readU32(buf, cursor, `${label} length`);
  const bytes = readFixedBytes(buf, cursor, len, label);
  return decodeUtf8Strict(bytes, label);
}

function decodePj1(doc: Buffer): NativeJson {
  const [value, end] = decodePj1Node(doc, 0, 0);
  if (end !== doc.length) throw new Error("invalid typed PJ1 JSON: trailing bytes");
  return value;
}

function decodePj1Node(
  doc: Buffer,
  start: number,
  depth: number,
): [NativeJson, number] {
  if (depth > 128) throw new Error("invalid typed PJ1 JSON: nesting too deep");
  if (start >= doc.length) throw new Error("invalid typed PJ1 JSON: truncated node");
  const tag = doc[start];
  switch (tag) {
    case 0:
      return [null, start + 1];
    case 1:
      return [false, start + 1];
    case 2:
      return [true, start + 1];
    case 3: {
      ensurePj1Range(doc, start + 1, 8);
      const integer = doc.readBigInt64LE(start + 1);
      const value =
        integer >= BigInt(Number.MIN_SAFE_INTEGER) &&
        integer <= BigInt(Number.MAX_SAFE_INTEGER)
          ? Number(integer)
          : integer;
      return [value, start + 9];
    }
    case 4: {
      ensurePj1Range(doc, start + 1, 8);
      const value = doc.readDoubleLE(start + 1);
      if (!Number.isFinite(value)) {
        throw new Error("invalid typed PJ1 JSON: non-finite float");
      }
      return [value, start + 9];
    }
    case 5: {
      const [value, end] = decodePj1String(doc, start + 1);
      return [value, end];
    }
    case 6:
      return decodePj1Array(doc, start, depth);
    case 7:
      return decodePj1Object(doc, start, depth);
    default:
      throw new Error(`invalid typed PJ1 JSON: reserved/invalid tag ${tag}`);
  }
}

function decodePj1Array(
  doc: Buffer,
  start: number,
  depth: number,
): [NativeJson[], number] {
  const count = pj1U32(doc, start + 1);
  const headerSize = 5 + 4 * (count + 1);
  ensurePj1Range(doc, start, headerSize);
  let expectedOffset = headerSize;
  const values: NativeJson[] = [];
  for (let index = 0; index < count; index++) {
    const offset = pj1U32(doc, start + 5 + index * 4);
    const nextOffset = pj1U32(doc, start + 5 + (index + 1) * 4);
    if (offset !== expectedOffset || nextOffset < offset) {
      throw new Error("invalid typed PJ1 JSON: array offsets not canonical");
    }
    const [value, end] = decodePj1Node(doc, start + offset, depth + 1);
    if (end !== start + nextOffset) {
      throw new Error("invalid typed PJ1 JSON: array element length mismatch");
    }
    values.push(value);
    expectedOffset = nextOffset;
  }
  const total = pj1U32(doc, start + 5 + count * 4);
  if (total !== expectedOffset) {
    throw new Error("invalid typed PJ1 JSON: array end offset mismatch");
  }
  ensurePj1Range(doc, start, total);
  return [values, start + total];
}

function decodePj1Object(
  doc: Buffer,
  start: number,
  depth: number,
): [Record<string, NativeJson>, number] {
  const count = pj1U32(doc, start + 1);
  const headerSize = 5 + count * 8 + 4;
  ensurePj1Range(doc, start, headerSize);
  const keys: Array<{ text: string; bytes: Buffer }> = [];
  let expectedKeyOffset = headerSize;
  for (let index = 0; index < count; index++) {
    const keyOffset = pj1U32(doc, start + 5 + index * 8);
    if (keyOffset !== expectedKeyOffset) {
      throw new Error("invalid typed PJ1 JSON: object key offset mismatch");
    }
    const [text, end, bytes] = decodePj1String(doc, start + keyOffset);
    const previous = keys[keys.length - 1];
    if (previous && Buffer.compare(previous.bytes, bytes) >= 0) {
      throw new Error("invalid typed PJ1 JSON: object keys not strictly sorted");
    }
    keys.push({ text, bytes });
    expectedKeyOffset = end - start;
  }

  const result: Record<string, NativeJson> = {};
  let expectedValueOffset = expectedKeyOffset;
  for (let index = 0; index < count; index++) {
    const valueOffset = pj1U32(doc, start + 5 + index * 8 + 4);
    if (valueOffset !== expectedValueOffset) {
      throw new Error("invalid typed PJ1 JSON: object value offset mismatch");
    }
    const [value, end] = decodePj1Node(doc, start + valueOffset, depth + 1);
    const key = keys[index];
    if (!key) throw new Error("invalid typed PJ1 JSON: missing object key");
    Object.defineProperty(result, key.text, {
      value,
      enumerable: true,
      configurable: true,
      writable: true,
    });
    expectedValueOffset = end - start;
  }
  const total = pj1U32(doc, start + 5 + count * 8);
  if (total !== expectedValueOffset) {
    throw new Error("invalid typed PJ1 JSON: object end offset mismatch");
  }
  ensurePj1Range(doc, start, total);
  return [result, start + total];
}

function decodePj1String(
  doc: Buffer,
  lengthOffset: number,
): [string, number, Buffer] {
  const len = pj1U32(doc, lengthOffset);
  const dataStart = lengthOffset + 4;
  ensurePj1Range(doc, dataStart, len);
  const bytes = doc.subarray(dataStart, dataStart + len);
  return [decodeUtf8Strict(bytes, "PJ1 string"), dataStart + len, bytes];
}

function pj1U32(doc: Buffer, offset: number): number {
  ensurePj1Range(doc, offset, 4);
  return doc.readUInt32LE(offset);
}

function ensurePj1Range(doc: Buffer, offset: number, len: number): void {
  if (
    !Number.isSafeInteger(offset) ||
    !Number.isSafeInteger(len) ||
    offset < 0 ||
    len < 0 ||
    offset + len > doc.length
  ) {
    throw new Error("invalid typed PJ1 JSON: truncated or overflowing range");
  }
}

function readU8(buf: Buffer, cursor: { pos: number }, label: string): number {
  if (cursor.pos + 1 > buf.length) {
    throw new Error(`truncated ${label}`);
  }
  const value = buf.readUInt8(cursor.pos);
  cursor.pos += 1;
  return value;
}

function readBool(buf: Buffer, cursor: { pos: number }, label: string): boolean {
  const raw = readU8(buf, cursor, label);
  if (raw === 0) return false;
  if (raw === 1) return true;
  throw new Error(`invalid boolean for ${label}: ${raw}`);
}

function readI64(buf: Buffer, cursor: { pos: number }, label: string): bigint {
  if (cursor.pos + 8 > buf.length) {
    throw new Error(`truncated ${label}`);
  }
  const value = buf.readBigInt64LE(cursor.pos);
  cursor.pos += 8;
  return value;
}

function readF64(buf: Buffer, cursor: { pos: number }, label: string): number {
  if (cursor.pos + 8 > buf.length) {
    throw new Error(`truncated ${label}`);
  }
  const value = buf.readDoubleLE(cursor.pos);
  cursor.pos += 8;
  return value;
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

function readFixedBytes(
  buf: Buffer,
  cursor: { pos: number },
  len: number,
  label: string,
): Buffer {
  if (cursor.pos + len > buf.length) {
    throw new Error(`truncated ${label}`);
  }
  const value = Buffer.from(buf.subarray(cursor.pos, cursor.pos + len));
  cursor.pos += len;
  return value;
}

function readBytes(buf: Buffer, cursor: { pos: number }, label: string): Buffer {
  const len = readU32(buf, cursor, label);
  return readFixedBytes(buf, cursor, len, label);
}

function u16LE(n: number): Buffer {
  const b = Buffer.alloc(2);
  b.writeUInt16LE(n, 0);
  return b;
}

function u8(n: number, label: string): number {
  if (!Number.isInteger(n) || n < 0 || n > 0xff) {
    throw new Error(`${label} must fit in u8`);
  }
  return n;
}

function u32LE(n: number): Buffer {
  const b = Buffer.alloc(4);
  b.writeUInt32LE(n, 0);
  return b;
}

function u64LE(n: bigint): Buffer {
  if (n < 0n || n > 0xffff_ffff_ffff_ffffn) {
    throw new Error(`u64 value out of range: ${n}`);
  }
  const b = Buffer.alloc(8);
  b.writeBigUInt64LE(n, 0);
  return b;
}
