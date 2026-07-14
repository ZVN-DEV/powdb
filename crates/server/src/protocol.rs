use powdb_storage::pj1::pj1_validate;
use powdb_storage::types::{TypeId, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::Zeroizing;

const MSG_CONNECT: u8 = 0x01;
const MSG_CONNECT_OK: u8 = 0x02;
const MSG_QUERY: u8 = 0x03;
/// Query carrying positional `$N` parameters (Task 4). Pure protocol
/// addition: old clients never send it, and old servers reject it with the
/// existing "unknown message type" error — no existing frame changes shape.
const MSG_QUERY_PARAMS: u8 = 0x04;
/// SQL query frame. Plain Query remains PowQL for backward compatibility.
const MSG_QUERY_SQL: u8 = 0x05;
/// Private sync control frames. These are append-only protocol extensions:
/// legacy query/result frames keep their original tags and shape.
const MSG_SYNC_STATUS: u8 = 0x20;
const MSG_SYNC_PULL: u8 = 0x21;
const MSG_SYNC_ACK: u8 = 0x22;
const MSG_SYNC_STATUS_RESULT: u8 = 0x23;
const MSG_SYNC_PULL_RESULT: u8 = 0x24;
const MSG_SYNC_ACK_RESULT: u8 = 0x25;
const MSG_RESULT_ROWS: u8 = 0x07;
const MSG_RESULT_SCALAR: u8 = 0x08;
const MSG_RESULT_OK: u8 = 0x09;
const MSG_ERROR: u8 = 0x0A;
const MSG_RESULT_MSG: u8 = 0x0B;
const MSG_DISCONNECT: u8 = 0x10;
const MSG_PING: u8 = 0x11;
const MSG_PONG: u8 = 0x12;
/// Native typed PowQL request. The response is `MSG_RESULT_ROWS_NATIVE` or
/// `MSG_RESULT_SCALAR_NATIVE`; legacy result frames remain byte-identical.
pub const MSG_QUERY_NATIVE: u8 = 0x13;
/// Native typed PowQL request with positional `$N` parameters.
pub const MSG_QUERY_PARAMS_NATIVE: u8 = 0x14;
/// Native typed SQL request.
pub const MSG_QUERY_SQL_NATIVE: u8 = 0x15;
/// Native typed row result.
pub const MSG_RESULT_ROWS_NATIVE: u8 = 0x16;
/// Native typed scalar result.
pub const MSG_RESULT_SCALAR_NATIVE: u8 = 0x17;

/// Maximum payload size accepted from the wire (64 MB).
const MAX_PAYLOAD_SIZE: usize = 64 * 1024 * 1024;

/// Maximum payload size for pre-auth CONNECT messages (4 KB).
/// Only a database name and password are needed before authentication.
const MAX_CONNECT_PAYLOAD_SIZE: usize = 4096;

/// Maximum number of columns allowed in a result set.
const MAX_COLUMNS: usize = 4096;

/// Maximum number of rows allowed in a single result message.
const MAX_ROWS: usize = 10_000_000;

/// Maximum number of bound parameters in a single QueryWithParams message.
const MAX_PARAMS: usize = 4096;

/// Maximum retained units accepted in one sync pull frame.
const MAX_SYNC_UNITS: usize = 4096;

const STRING_LEN_PREFIX: usize = 4; // decode_string reads a 4-byte length prefix

/// A positional parameter value carried by [`Message::QueryWithParams`].
///
/// Wire encoding per param: a 1-byte tag followed by the body —
///   `0` null (no body), `1` int (8B LE i64), `2` float (8B LE f64),
///   `3` bool (1B), `4` str (length-prefixed UTF-8).
#[derive(Debug, Clone, PartialEq)]
pub enum WireParam {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireSyncRepairAction {
    None,
    Pull,
    AwaitArchive,
    Rebootstrap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireSyncStatus {
    pub replica_id: String,
    pub active: bool,
    pub last_applied_lsn: Option<u64>,
    pub remote_lsn: u64,
    pub servable_lsn: Option<u64>,
    pub unarchived_lsn: Option<u64>,
    pub lag_lsn: Option<u64>,
    pub lag_bytes: Option<u64>,
    pub lag_ms: Option<u64>,
    pub stale: bool,
    pub repair_action: WireSyncRepairAction,
    pub last_sync_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireRetainedUnit {
    pub tx_id: u64,
    pub record_type: u8,
    pub lsn: u64,
    pub data: Vec<u8>,
}

impl WireRetainedUnit {
    /// Encoded byte length of this retained unit inside a sync pull result
    /// payload. Keep this next to `encode_retained_unit` so metrics and
    /// max-byte enforcement evolve with the wire shape.
    pub fn encoded_len(&self) -> Result<u64, String> {
        let data_len = u64::try_from(self.data.len())
            .map_err(|_| "sync retained unit payload too large".to_string())?;
        8u64.checked_add(1)
            .and_then(|n| n.checked_add(8))
            .and_then(|n| n.checked_add(4))
            .and_then(|n| n.checked_add(data_len))
            .ok_or_else(|| "sync retained unit encoded length overflow".to_string())
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Connect {
        db_name: String,
        /// Client-supplied candidate password. Wrapped in `Zeroizing` so the
        /// raw bytes from the wire are wiped from memory once the `Message`
        /// (and thus this field) is dropped after the constant-time compare —
        /// the candidate never lingers in a plain `String`.
        password: Option<Zeroizing<String>>,
        /// Optional user name for multi-user authentication. Appended after the
        /// password on the wire so the format is a pure, backward-compatible
        /// extension: old clients that omit it decode as `None`.
        username: Option<String>,
    },
    ConnectOk {
        version: String,
    },
    Query {
        query: String,
    },
    /// A SQL query string.
    QuerySql {
        query: String,
    },
    /// A query string with positional `$N` parameters bound at the server.
    QueryWithParams {
        query: String,
        params: Vec<WireParam>,
    },
    /// PowQL request whose result preserves storage value types.
    QueryNative {
        query: String,
    },
    /// Parameterized PowQL request whose result preserves storage value types.
    QueryWithParamsNative {
        query: String,
        params: Vec<WireParam>,
    },
    /// SQL request whose result preserves storage value types.
    QuerySqlNative {
        query: String,
    },
    /// Request primary-side status for one embedded replica cursor.
    SyncStatus {
        replica_id: String,
    },
    /// Pull a bounded retained-unit chunk after the server-side replica cursor.
    SyncPull {
        replica_id: String,
        since_lsn: u64,
        max_units: u32,
        max_bytes: u64,
        database_id: [u8; 16],
        primary_generation: u64,
        wal_format_version: u16,
        catalog_version: u16,
        segment_format_version: u16,
    },
    /// Acknowledge that the replica applied retained history through `applied_lsn`.
    SyncAck {
        replica_id: String,
        applied_lsn: u64,
        remote_lsn: u64,
    },
    SyncStatusResult {
        status: WireSyncStatus,
    },
    SyncPullResult {
        status: WireSyncStatus,
        units: Vec<WireRetainedUnit>,
        has_more: bool,
    },
    SyncAckResult {
        previous_applied_lsn: u64,
        applied_lsn: u64,
        remote_lsn: u64,
        advanced: bool,
        status: WireSyncStatus,
    },
    ResultRows {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    ResultScalar {
        value: String,
    },
    ResultRowsNative {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    ResultScalarNative {
        value: Value,
    },
    ResultOk {
        affected: u64,
    },
    /// A descriptive status message (e.g. "type User created", "index dropped").
    ResultMessage {
        message: String,
    },
    Error {
        message: String,
    },
    Disconnect,
    Ping,
    Pong,
}

impl Message {
    /// Encode message into wire format: `[type(1)][flags(1)][len(4)][payload]`
    pub fn encode(&self) -> Vec<u8> {
        let (msg_type, payload) = match self {
            Message::Connect {
                db_name,
                password,
                username,
            } => {
                let mut buf = encode_string(db_name);
                // Password is encoded as a length-prefixed string. Empty (len=0) means None.
                match password {
                    Some(p) => buf.extend_from_slice(&encode_string(p)),
                    None => buf.extend_from_slice(&0u32.to_le_bytes()),
                }
                // Username is appended after the password (append-only extension).
                // Length-prefixed string; empty (len=0) means None.
                match username {
                    Some(u) => buf.extend_from_slice(&encode_string(u)),
                    None => buf.extend_from_slice(&0u32.to_le_bytes()),
                }
                (MSG_CONNECT, buf)
            }
            Message::ConnectOk { version } => (MSG_CONNECT_OK, encode_string(version)),
            Message::Query { query } => (MSG_QUERY, encode_string(query)),
            Message::QuerySql { query } => (MSG_QUERY_SQL, encode_string(query)),
            Message::QueryWithParams { query, params } => {
                (MSG_QUERY_PARAMS, encode_query_with_params(query, params))
            }
            Message::QueryNative { query } => (MSG_QUERY_NATIVE, encode_string(query)),
            Message::QueryWithParamsNative { query, params } => (
                MSG_QUERY_PARAMS_NATIVE,
                encode_query_with_params(query, params),
            ),
            Message::QuerySqlNative { query } => (MSG_QUERY_SQL_NATIVE, encode_string(query)),
            Message::SyncStatus { replica_id } => (MSG_SYNC_STATUS, encode_string(replica_id)),
            Message::SyncPull {
                replica_id,
                since_lsn,
                max_units,
                max_bytes,
                database_id,
                primary_generation,
                wal_format_version,
                catalog_version,
                segment_format_version,
            } => {
                let mut buf = encode_string(replica_id);
                buf.extend_from_slice(&since_lsn.to_le_bytes());
                buf.extend_from_slice(&max_units.to_le_bytes());
                buf.extend_from_slice(&max_bytes.to_le_bytes());
                buf.extend_from_slice(database_id);
                buf.extend_from_slice(&primary_generation.to_le_bytes());
                buf.extend_from_slice(&wal_format_version.to_le_bytes());
                buf.extend_from_slice(&catalog_version.to_le_bytes());
                buf.extend_from_slice(&segment_format_version.to_le_bytes());
                (MSG_SYNC_PULL, buf)
            }
            Message::SyncAck {
                replica_id,
                applied_lsn,
                remote_lsn,
            } => {
                let mut buf = encode_string(replica_id);
                buf.extend_from_slice(&applied_lsn.to_le_bytes());
                buf.extend_from_slice(&remote_lsn.to_le_bytes());
                (MSG_SYNC_ACK, buf)
            }
            Message::SyncStatusResult { status } => {
                (MSG_SYNC_STATUS_RESULT, encode_sync_status(status))
            }
            Message::SyncPullResult {
                status,
                units,
                has_more,
            } => {
                let mut buf = encode_sync_status(status);
                buf.extend_from_slice(&(units.len() as u32).to_le_bytes());
                for unit in units {
                    encode_retained_unit(&mut buf, unit);
                }
                buf.push(u8::from(*has_more));
                (MSG_SYNC_PULL_RESULT, buf)
            }
            Message::SyncAckResult {
                previous_applied_lsn,
                applied_lsn,
                remote_lsn,
                advanced,
                status,
            } => {
                let mut buf = Vec::new();
                buf.extend_from_slice(&previous_applied_lsn.to_le_bytes());
                buf.extend_from_slice(&applied_lsn.to_le_bytes());
                buf.extend_from_slice(&remote_lsn.to_le_bytes());
                buf.push(u8::from(*advanced));
                buf.extend_from_slice(&encode_sync_status(status));
                (MSG_SYNC_ACK_RESULT, buf)
            }
            Message::ResultRows { columns, rows } => {
                let mut buf = Vec::new();
                buf.extend_from_slice(&(columns.len() as u16).to_le_bytes());
                for col in columns {
                    buf.extend_from_slice(&encode_string(col));
                }
                buf.extend_from_slice(&(rows.len() as u32).to_le_bytes());
                for row in rows {
                    for val in row {
                        buf.extend_from_slice(&encode_string(val));
                    }
                }
                (MSG_RESULT_ROWS, buf)
            }
            Message::ResultScalar { value } => (MSG_RESULT_SCALAR, encode_string(value)),
            Message::ResultRowsNative { columns, rows } => {
                let mut buf = Vec::new();
                buf.extend_from_slice(&(columns.len() as u16).to_le_bytes());
                for column in columns {
                    buf.extend_from_slice(&encode_string(column));
                }
                buf.extend_from_slice(&(rows.len() as u32).to_le_bytes());
                for row in rows {
                    for value in row {
                        encode_typed_value(&mut buf, value);
                    }
                }
                (MSG_RESULT_ROWS_NATIVE, buf)
            }
            Message::ResultScalarNative { value } => {
                let mut buf = Vec::new();
                encode_typed_value(&mut buf, value);
                (MSG_RESULT_SCALAR_NATIVE, buf)
            }
            Message::ResultOk { affected } => (MSG_RESULT_OK, affected.to_le_bytes().to_vec()),
            Message::ResultMessage { message } => (MSG_RESULT_MSG, encode_string(message)),
            Message::Error { message } => (MSG_ERROR, encode_string(message)),
            Message::Disconnect => (MSG_DISCONNECT, Vec::new()),
            Message::Ping => (MSG_PING, Vec::new()),
            Message::Pong => (MSG_PONG, Vec::new()),
        };

        let mut frame = Vec::with_capacity(6 + payload.len());
        frame.push(msg_type);
        frame.push(0); // flags
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&payload);
        frame
    }

    /// Decode message from wire format.
    pub fn decode(data: &[u8]) -> Result<Message, String> {
        if data.len() < 6 {
            return Err("frame too short".into());
        }
        let msg_type = data[0];
        let _flags = data[1];
        let len_bytes: [u8; 4] = data[2..6]
            .try_into()
            .map_err(|_| "invalid header length field".to_string())?;
        let payload_len = u32::from_le_bytes(len_bytes) as usize;
        if 6 + payload_len > data.len() {
            return Err("payload length exceeds frame".into());
        }
        let payload = &data[6..6 + payload_len];

        match msg_type {
            MSG_CONNECT => {
                let mut pos = 0;
                let db_name = decode_string(payload, &mut pos)?;
                // Password is optional. If there are no more bytes, treat as None
                // (backwards compatible with old clients that don't send a password).
                let password = if pos < payload.len() {
                    // Wrap the candidate immediately so the only owned copy of
                    // the secret lives inside `Zeroizing` and is wiped on drop.
                    let p = Zeroizing::new(decode_string(payload, &mut pos)?);
                    if p.is_empty() {
                        None
                    } else {
                        Some(p)
                    }
                } else {
                    None
                };
                // Username is optional and appended after the password. Old
                // clients omit it entirely → no more bytes → None.
                let username = if pos < payload.len() {
                    let u = decode_string(payload, &mut pos)?;
                    if u.is_empty() {
                        None
                    } else {
                        Some(u)
                    }
                } else {
                    None
                };
                Ok(Message::Connect {
                    db_name,
                    password,
                    username,
                })
            }
            MSG_CONNECT_OK => {
                let version = decode_string(payload, &mut 0)?;
                Ok(Message::ConnectOk { version })
            }
            MSG_QUERY => {
                let query = decode_string(payload, &mut 0)?;
                Ok(Message::Query { query })
            }
            MSG_QUERY_SQL => {
                let query = decode_string(payload, &mut 0)?;
                Ok(Message::QuerySql { query })
            }
            MSG_QUERY_PARAMS => {
                let mut pos = 0;
                let query = decode_string(payload, &mut pos)?;
                if pos + 2 > payload.len() {
                    return Err("truncated param count".into());
                }
                let count_bytes: [u8; 2] = payload[pos..pos + 2]
                    .try_into()
                    .map_err(|_| "invalid param count bytes".to_string())?;
                let count = u16::from_le_bytes(count_bytes) as usize;
                pos += 2;
                if count > MAX_PARAMS {
                    return Err("too many parameters".into());
                }
                let mut params = Vec::with_capacity(count.min(payload.len() - pos));
                for _ in 0..count {
                    if pos >= payload.len() {
                        return Err("truncated param tag".into());
                    }
                    let tag = payload[pos];
                    pos += 1;
                    let p = match tag {
                        0 => WireParam::Null,
                        1 => {
                            if pos + 8 > payload.len() {
                                return Err("truncated int param".into());
                            }
                            let b: [u8; 8] = payload[pos..pos + 8]
                                .try_into()
                                .map_err(|_| "invalid int param bytes".to_string())?;
                            pos += 8;
                            WireParam::Int(i64::from_le_bytes(b))
                        }
                        2 => {
                            if pos + 8 > payload.len() {
                                return Err("truncated float param".into());
                            }
                            let b: [u8; 8] = payload[pos..pos + 8]
                                .try_into()
                                .map_err(|_| "invalid float param bytes".to_string())?;
                            pos += 8;
                            WireParam::Float(f64::from_le_bytes(b))
                        }
                        3 => {
                            if pos + 1 > payload.len() {
                                return Err("truncated bool param".into());
                            }
                            let v = payload[pos] != 0;
                            pos += 1;
                            WireParam::Bool(v)
                        }
                        4 => WireParam::Str(decode_string(payload, &mut pos)?),
                        other => return Err(format!("unknown param tag: {other}")),
                    };
                    params.push(p);
                }
                Ok(Message::QueryWithParams { query, params })
            }
            MSG_QUERY_NATIVE => {
                let query = decode_exact_string(payload, "native PowQL query")?;
                Ok(Message::QueryNative { query })
            }
            MSG_QUERY_PARAMS_NATIVE => {
                let (query, params) = decode_query_with_params_exact(payload)?;
                Ok(Message::QueryWithParamsNative { query, params })
            }
            MSG_QUERY_SQL_NATIVE => {
                let query = decode_exact_string(payload, "native SQL query")?;
                Ok(Message::QuerySqlNative { query })
            }
            MSG_SYNC_STATUS => {
                let replica_id = decode_string(payload, &mut 0)?;
                Ok(Message::SyncStatus { replica_id })
            }
            MSG_SYNC_PULL => {
                let mut pos = 0;
                let replica_id = decode_string(payload, &mut pos)?;
                let since_lsn = decode_u64(payload, &mut pos, "sync pull since LSN")?;
                let max_units = decode_u32(payload, &mut pos, "sync pull max units")?;
                let max_bytes = decode_u64(payload, &mut pos, "sync pull max bytes")?;
                let database_id = decode_16_bytes(payload, &mut pos, "sync database id")?;
                let primary_generation = decode_u64(payload, &mut pos, "sync primary generation")?;
                let wal_format_version = decode_u16(payload, &mut pos, "sync WAL format version")?;
                let catalog_version = decode_u16(payload, &mut pos, "sync catalog version")?;
                let segment_format_version =
                    decode_u16(payload, &mut pos, "sync segment format version")?;
                Ok(Message::SyncPull {
                    replica_id,
                    since_lsn,
                    max_units,
                    max_bytes,
                    database_id,
                    primary_generation,
                    wal_format_version,
                    catalog_version,
                    segment_format_version,
                })
            }
            MSG_SYNC_ACK => {
                let mut pos = 0;
                let replica_id = decode_string(payload, &mut pos)?;
                let applied_lsn = decode_u64(payload, &mut pos, "sync ack applied LSN")?;
                let remote_lsn = decode_u64(payload, &mut pos, "sync ack remote LSN")?;
                Ok(Message::SyncAck {
                    replica_id,
                    applied_lsn,
                    remote_lsn,
                })
            }
            MSG_SYNC_STATUS_RESULT => {
                let mut pos = 0;
                let status = decode_sync_status(payload, &mut pos)?;
                Ok(Message::SyncStatusResult { status })
            }
            MSG_SYNC_PULL_RESULT => {
                let mut pos = 0;
                let status = decode_sync_status(payload, &mut pos)?;
                let count = decode_u32(payload, &mut pos, "sync retained unit count")? as usize;
                if count > MAX_SYNC_UNITS {
                    return Err("too many retained units".into());
                }
                let mut units = Vec::with_capacity(count.min(payload.len().saturating_sub(pos)));
                for _ in 0..count {
                    units.push(decode_retained_unit(payload, &mut pos)?);
                }
                let has_more = decode_bool(payload, &mut pos, "sync has_more")?;
                Ok(Message::SyncPullResult {
                    status,
                    units,
                    has_more,
                })
            }
            MSG_SYNC_ACK_RESULT => {
                let mut pos = 0;
                let previous_applied_lsn = decode_u64(payload, &mut pos, "previous applied LSN")?;
                let applied_lsn = decode_u64(payload, &mut pos, "applied LSN")?;
                let remote_lsn = decode_u64(payload, &mut pos, "remote LSN")?;
                let advanced = decode_bool(payload, &mut pos, "sync ack advanced")?;
                let status = decode_sync_status(payload, &mut pos)?;
                Ok(Message::SyncAckResult {
                    previous_applied_lsn,
                    applied_lsn,
                    remote_lsn,
                    advanced,
                    status,
                })
            }
            MSG_RESULT_ROWS => {
                let mut pos = 0;
                if pos + 2 > payload.len() {
                    return Err("truncated column count".into());
                }
                let col_bytes: [u8; 2] = payload[pos..pos + 2]
                    .try_into()
                    .map_err(|_| "invalid column count bytes".to_string())?;
                let col_count = u16::from_le_bytes(col_bytes) as usize;
                pos += 2;
                if col_count > MAX_COLUMNS {
                    return Err("too many columns".into());
                }
                let mut columns =
                    Vec::with_capacity(col_count.min((payload.len() - pos) / STRING_LEN_PREFIX));
                for _ in 0..col_count {
                    columns.push(decode_string(payload, &mut pos)?);
                }
                if pos + 4 > payload.len() {
                    return Err("truncated row count".into());
                }
                let row_bytes: [u8; 4] = payload[pos..pos + 4]
                    .try_into()
                    .map_err(|_| "invalid row count bytes".to_string())?;
                let row_count = u32::from_le_bytes(row_bytes) as usize;
                pos += 4;
                if row_count > MAX_ROWS {
                    return Err("too many rows".into());
                }
                // Never preallocate (or iterate) proportional to an untrusted count: each row
                // carries `col_count` length-prefixed strings of >= STRING_LEN_PREFIX bytes, so
                // the remaining payload bounds how many rows can follow. A zero-column row
                // consumes no bytes (vacuous bound), so a tiny frame could otherwise declare
                // millions of rows and force a huge allocation (reachable pre-auth). Reject it.
                let max_rows = match col_count.checked_mul(STRING_LEN_PREFIX) {
                    Some(0) | None => 0,
                    Some(per_row) => (payload.len() - pos) / per_row,
                };
                if row_count > max_rows {
                    return Err("row count exceeds payload size".into());
                }
                let mut rows = Vec::with_capacity(row_count);
                for _ in 0..row_count {
                    let mut row = Vec::with_capacity(col_count);
                    for _ in 0..col_count {
                        row.push(decode_string(payload, &mut pos)?);
                    }
                    rows.push(row);
                }
                Ok(Message::ResultRows { columns, rows })
            }
            MSG_RESULT_SCALAR => {
                let value = decode_string(payload, &mut 0)?;
                Ok(Message::ResultScalar { value })
            }
            MSG_RESULT_ROWS_NATIVE => decode_native_rows(payload),
            MSG_RESULT_SCALAR_NATIVE => {
                let mut pos = 0;
                let value = decode_typed_value(payload, &mut pos)?;
                require_payload_end(payload, pos, "native scalar")?;
                Ok(Message::ResultScalarNative { value })
            }
            MSG_RESULT_OK => {
                if payload.len() < 8 {
                    return Err("truncated result ok payload".into());
                }
                let aff_bytes: [u8; 8] = payload[0..8]
                    .try_into()
                    .map_err(|_| "invalid affected count bytes".to_string())?;
                let affected = u64::from_le_bytes(aff_bytes);
                Ok(Message::ResultOk { affected })
            }
            MSG_RESULT_MSG => {
                let message = decode_string(payload, &mut 0)?;
                Ok(Message::ResultMessage { message })
            }
            MSG_ERROR => {
                let message = decode_string(payload, &mut 0)?;
                Ok(Message::Error { message })
            }
            MSG_DISCONNECT => Ok(Message::Disconnect),
            MSG_PING => Ok(Message::Ping),
            MSG_PONG => Ok(Message::Pong),
            _ => Err(format!("unknown message type: {msg_type:#x}")),
        }
    }

    /// Write this message to an async writer.
    pub async fn write_to<W: AsyncWriteExt + Unpin>(&self, writer: &mut W) -> std::io::Result<()> {
        let bytes = self.encode();
        writer.write_all(&bytes).await
    }

    /// Read a message from an async reader.
    pub async fn read_from<R: AsyncReadExt + Unpin>(
        reader: &mut R,
    ) -> std::io::Result<Option<Message>> {
        Self::read_from_with_limit(reader, MAX_PAYLOAD_SIZE).await
    }

    /// Read a pre-auth message with a smaller payload limit (4 KB).
    /// Use this before authentication is complete to prevent oversized
    /// CONNECT payloads from consuming server memory.
    pub async fn read_from_preauth<R: AsyncReadExt + Unpin>(
        reader: &mut R,
    ) -> std::io::Result<Option<Message>> {
        Self::read_from_with_limit(reader, MAX_CONNECT_PAYLOAD_SIZE).await
    }

    /// Read a message from an async reader with a configurable payload limit.
    async fn read_from_with_limit<R: AsyncReadExt + Unpin>(
        reader: &mut R,
        max_payload: usize,
    ) -> std::io::Result<Option<Message>> {
        let mut header = [0u8; 6];
        match reader.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let len_bytes: [u8; 4] = header[2..6].try_into().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid header length field",
            )
        })?;
        let payload_len = u32::from_le_bytes(len_bytes) as usize;
        if payload_len > max_payload {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("payload too large: {payload_len} bytes (max {max_payload})"),
            ));
        }
        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            reader.read_exact(&mut payload).await?;
        }

        let mut full = Vec::with_capacity(6 + payload_len);
        full.extend_from_slice(&header);
        full.extend_from_slice(&payload);

        Message::decode(&full)
            .map(Some)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

fn encode_query_with_params(query: &str, params: &[WireParam]) -> Vec<u8> {
    let mut buf = encode_string(query);
    buf.extend_from_slice(&(params.len() as u16).to_le_bytes());
    for param in params {
        match param {
            WireParam::Null => buf.push(0),
            WireParam::Int(value) => {
                buf.push(1);
                buf.extend_from_slice(&value.to_le_bytes());
            }
            WireParam::Float(value) => {
                buf.push(2);
                buf.extend_from_slice(&value.to_le_bytes());
            }
            WireParam::Bool(value) => {
                buf.push(3);
                buf.push(u8::from(*value));
            }
            WireParam::Str(value) => {
                buf.push(4);
                buf.extend_from_slice(&encode_string(value));
            }
        }
    }
    buf
}

fn decode_query_with_params_exact(payload: &[u8]) -> Result<(String, Vec<WireParam>), String> {
    let mut pos = 0;
    let query = decode_string_strict(payload, &mut pos, "native query")?;
    let count = decode_u16(payload, &mut pos, "native param count")? as usize;
    if count > MAX_PARAMS {
        return Err("too many parameters".into());
    }
    // Every parameter consumes at least its one-byte tag.
    if count > payload.len().saturating_sub(pos) {
        return Err("parameter count exceeds payload size".into());
    }
    let mut params = Vec::with_capacity(count);
    for _ in 0..count {
        if pos >= payload.len() {
            return Err("truncated param tag".into());
        }
        let tag = payload[pos];
        pos += 1;
        params.push(match tag {
            0 => WireParam::Null,
            1 => {
                let bytes = take_exact(payload, &mut pos, 8, "int param")?;
                WireParam::Int(i64::from_le_bytes(bytes.try_into().expect("8 bytes")))
            }
            2 => {
                let bytes = take_exact(payload, &mut pos, 8, "float param")?;
                WireParam::Float(f64::from_le_bytes(bytes.try_into().expect("8 bytes")))
            }
            3 => WireParam::Bool(decode_bool(payload, &mut pos, "bool param")?),
            4 => WireParam::Str(decode_string_strict(payload, &mut pos, "string param")?),
            other => return Err(format!("unknown param tag: {other}")),
        });
    }
    require_payload_end(payload, pos, "native parameterized query")?;
    Ok((query, params))
}

fn encode_typed_value(out: &mut Vec<u8>, value: &Value) {
    out.push(value.type_id() as u8);
    let body_len = match value {
        Value::Empty => 0,
        Value::Int(_) | Value::Float(_) | Value::DateTime(_) => 8,
        Value::Bool(_) => 1,
        Value::Str(value) => value.len(),
        Value::Uuid(_) => 16,
        Value::Bytes(value) => value.len(),
        Value::Json(value) => value.len(),
    };
    out.extend_from_slice(&(body_len as u32).to_le_bytes());
    match value {
        Value::Empty => {}
        Value::Int(value) | Value::DateTime(value) => out.extend_from_slice(&value.to_le_bytes()),
        Value::Float(value) => out.extend_from_slice(&value.to_le_bytes()),
        Value::Bool(value) => out.push(u8::from(*value)),
        Value::Str(value) => out.extend_from_slice(value.as_bytes()),
        Value::Uuid(value) => out.extend_from_slice(value),
        Value::Bytes(value) => out.extend_from_slice(value),
        Value::Json(value) => out.extend_from_slice(value),
    }
}

fn decode_typed_value(data: &[u8], pos: &mut usize) -> Result<Value, String> {
    if *pos >= data.len() {
        return Err("truncated typed value tag".into());
    }
    let raw_type = data[*pos];
    *pos += 1;
    let type_id =
        TypeId::from_u8(raw_type).ok_or_else(|| format!("unknown typed value tag: {raw_type}"))?;
    let body_len = decode_u32(data, pos, "typed value body length")? as usize;
    let body = take_exact(data, pos, body_len, "typed value body")?;

    let require_len = |expected: usize| {
        if body_len == expected {
            Ok(())
        } else {
            Err(format!(
                "invalid {type_id:?} typed value length: expected {expected}, got {body_len}"
            ))
        }
    };
    match type_id {
        TypeId::Empty => {
            require_len(0)?;
            Ok(Value::Empty)
        }
        TypeId::Int => {
            require_len(8)?;
            Ok(Value::Int(i64::from_le_bytes(
                body.try_into().expect("validated 8-byte int"),
            )))
        }
        TypeId::Float => {
            require_len(8)?;
            Ok(Value::Float(f64::from_le_bytes(
                body.try_into().expect("validated 8-byte float"),
            )))
        }
        TypeId::Bool => {
            require_len(1)?;
            match body[0] {
                0 => Ok(Value::Bool(false)),
                1 => Ok(Value::Bool(true)),
                other => Err(format!("invalid typed boolean: {other}")),
            }
        }
        TypeId::Str => Ok(Value::Str(
            std::str::from_utf8(body)
                .map_err(|error| format!("invalid UTF-8 in typed string: {error}"))?
                .to_owned(),
        )),
        TypeId::DateTime => {
            require_len(8)?;
            Ok(Value::DateTime(i64::from_le_bytes(
                body.try_into().expect("validated 8-byte datetime"),
            )))
        }
        TypeId::Uuid => {
            require_len(16)?;
            Ok(Value::Uuid(
                body.try_into().expect("validated 16-byte UUID"),
            ))
        }
        TypeId::Bytes => Ok(Value::Bytes(body.to_vec())),
        TypeId::Json => {
            pj1_validate(body).map_err(|error| format!("invalid typed PJ1 JSON: {error}"))?;
            Ok(Value::Json(body.into()))
        }
    }
}

fn decode_native_rows(payload: &[u8]) -> Result<Message, String> {
    let mut pos = 0;
    let col_count = decode_u16(payload, &mut pos, "native column count")? as usize;
    if col_count > MAX_COLUMNS {
        return Err("too many columns".into());
    }
    let mut columns = Vec::with_capacity(col_count);
    for _ in 0..col_count {
        columns.push(decode_string_strict(
            payload,
            &mut pos,
            "native column name",
        )?);
    }
    let row_count = decode_u32(payload, &mut pos, "native row count")? as usize;
    if row_count > MAX_ROWS {
        return Err("too many rows".into());
    }
    // Every cell has at least a one-byte type and four-byte body length.
    let minimum_row_len = col_count
        .checked_mul(5)
        .ok_or_else(|| "native row width overflow".to_string())?;
    if minimum_row_len == 0 {
        if row_count != 0 {
            return Err("nonzero native row count with zero columns".into());
        }
    } else if row_count > payload.len().saturating_sub(pos) / minimum_row_len {
        return Err("native row count exceeds payload size".into());
    }

    let mut rows = Vec::with_capacity(row_count);
    for _ in 0..row_count {
        let mut row = Vec::with_capacity(col_count);
        for _ in 0..col_count {
            row.push(decode_typed_value(payload, &mut pos)?);
        }
        rows.push(row);
    }
    require_payload_end(payload, pos, "native rows")?;
    Ok(Message::ResultRowsNative { columns, rows })
}

fn take_exact<'a>(
    data: &'a [u8],
    pos: &mut usize,
    len: usize,
    label: &str,
) -> Result<&'a [u8], String> {
    let end = pos
        .checked_add(len)
        .ok_or_else(|| format!("{label} length overflow"))?;
    if end > data.len() {
        return Err(format!("truncated {label}"));
    }
    let bytes = &data[*pos..end];
    *pos = end;
    Ok(bytes)
}

fn require_payload_end(payload: &[u8], pos: usize, label: &str) -> Result<(), String> {
    if pos == payload.len() {
        Ok(())
    } else {
        Err(format!("trailing bytes in {label} payload"))
    }
}

fn decode_exact_string(payload: &[u8], label: &str) -> Result<String, String> {
    let mut pos = 0;
    let value = decode_string_strict(payload, &mut pos, label)?;
    require_payload_end(payload, pos, label)?;
    Ok(value)
}

fn decode_string_strict(data: &[u8], pos: &mut usize, label: &str) -> Result<String, String> {
    let len = decode_u32(data, pos, label)? as usize;
    let bytes = take_exact(data, pos, len, label)?;
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|error| format!("invalid UTF-8 in {label}: {error}"))
}

fn encode_string(s: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + s.len());
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
    buf
}

fn decode_string(data: &[u8], pos: &mut usize) -> Result<String, String> {
    if *pos + 4 > data.len() {
        return Err("truncated string length".into());
    }
    let len_bytes: [u8; 4] = data[*pos..*pos + 4]
        .try_into()
        .map_err(|_| "invalid string length bytes".to_string())?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    *pos += 4;
    if *pos + len > data.len() {
        return Err("truncated string data".into());
    }
    let s = String::from_utf8_lossy(&data[*pos..*pos + len]).into_owned();
    *pos += len;
    Ok(s)
}

fn encode_option_u64(out: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(value) => {
            out.push(1);
            out.extend_from_slice(&value.to_le_bytes());
        }
        None => out.push(0),
    }
}

fn decode_option_u64(data: &[u8], pos: &mut usize, label: &str) -> Result<Option<u64>, String> {
    let present = decode_bool(data, pos, label)?;
    if present {
        Ok(Some(decode_u64(data, pos, label)?))
    } else {
        Ok(None)
    }
}

fn encode_option_string(out: &mut Vec<u8>, value: Option<&String>) {
    match value {
        Some(value) => {
            out.push(1);
            out.extend_from_slice(&encode_string(value));
        }
        None => out.push(0),
    }
}

fn decode_option_string(
    data: &[u8],
    pos: &mut usize,
    label: &str,
) -> Result<Option<String>, String> {
    let present = decode_bool(data, pos, label)?;
    if present {
        Ok(Some(decode_string(data, pos)?))
    } else {
        Ok(None)
    }
}

fn encode_sync_status(status: &WireSyncStatus) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_string(&status.replica_id));
    out.push(u8::from(status.active));
    encode_option_u64(&mut out, status.last_applied_lsn);
    out.extend_from_slice(&status.remote_lsn.to_le_bytes());
    encode_option_u64(&mut out, status.servable_lsn);
    encode_option_u64(&mut out, status.unarchived_lsn);
    encode_option_u64(&mut out, status.lag_lsn);
    encode_option_u64(&mut out, status.lag_bytes);
    encode_option_u64(&mut out, status.lag_ms);
    out.push(u8::from(status.stale));
    out.push(match status.repair_action {
        WireSyncRepairAction::None => 0,
        WireSyncRepairAction::Pull => 1,
        WireSyncRepairAction::AwaitArchive => 2,
        WireSyncRepairAction::Rebootstrap => 3,
    });
    encode_option_string(&mut out, status.last_sync_error.as_ref());
    out
}

fn decode_sync_status(data: &[u8], pos: &mut usize) -> Result<WireSyncStatus, String> {
    let replica_id = decode_string(data, pos)?;
    let active = decode_bool(data, pos, "sync status active")?;
    let last_applied_lsn = decode_option_u64(data, pos, "sync status last applied LSN")?;
    let remote_lsn = decode_u64(data, pos, "sync status remote LSN")?;
    let servable_lsn = decode_option_u64(data, pos, "sync status servable LSN")?;
    let unarchived_lsn = decode_option_u64(data, pos, "sync status unarchived LSN")?;
    let lag_lsn = decode_option_u64(data, pos, "sync status lag LSN")?;
    let lag_bytes = decode_option_u64(data, pos, "sync status lag bytes")?;
    let lag_ms = decode_option_u64(data, pos, "sync status lag milliseconds")?;
    let stale = decode_bool(data, pos, "sync status stale")?;
    if *pos >= data.len() {
        return Err("truncated sync repair action".into());
    }
    let repair_action = match data[*pos] {
        0 => WireSyncRepairAction::None,
        1 => WireSyncRepairAction::Pull,
        2 => WireSyncRepairAction::AwaitArchive,
        3 => WireSyncRepairAction::Rebootstrap,
        other => return Err(format!("unknown sync repair action: {other}")),
    };
    *pos += 1;
    let last_sync_error = decode_option_string(data, pos, "sync status last error")?;
    Ok(WireSyncStatus {
        replica_id,
        active,
        last_applied_lsn,
        remote_lsn,
        servable_lsn,
        unarchived_lsn,
        lag_lsn,
        lag_bytes,
        lag_ms,
        stale,
        repair_action,
        last_sync_error,
    })
}

fn encode_retained_unit(out: &mut Vec<u8>, unit: &WireRetainedUnit) {
    out.extend_from_slice(&unit.tx_id.to_le_bytes());
    out.push(unit.record_type);
    out.extend_from_slice(&unit.lsn.to_le_bytes());
    out.extend_from_slice(&(unit.data.len() as u32).to_le_bytes());
    out.extend_from_slice(&unit.data);
}

fn decode_retained_unit(data: &[u8], pos: &mut usize) -> Result<WireRetainedUnit, String> {
    let tx_id = decode_u64(data, pos, "sync retained unit tx id")?;
    if *pos >= data.len() {
        return Err("truncated sync retained unit record type".into());
    }
    let record_type = data[*pos];
    *pos += 1;
    let lsn = decode_u64(data, pos, "sync retained unit LSN")?;
    let data = decode_bytes(data, pos, "sync retained unit payload")?;
    Ok(WireRetainedUnit {
        tx_id,
        record_type,
        lsn,
        data,
    })
}

fn decode_bool(data: &[u8], pos: &mut usize, label: &str) -> Result<bool, String> {
    if *pos >= data.len() {
        return Err(format!("truncated {label}"));
    }
    let raw = data[*pos];
    *pos += 1;
    match raw {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(format!("invalid boolean for {label}: {other}")),
    }
}

fn decode_u16(data: &[u8], pos: &mut usize, label: &str) -> Result<u16, String> {
    if *pos + 2 > data.len() {
        return Err(format!("truncated {label}"));
    }
    let bytes: [u8; 2] = data[*pos..*pos + 2]
        .try_into()
        .map_err(|_| format!("invalid {label} bytes"))?;
    *pos += 2;
    Ok(u16::from_le_bytes(bytes))
}

fn decode_u32(data: &[u8], pos: &mut usize, label: &str) -> Result<u32, String> {
    if *pos + 4 > data.len() {
        return Err(format!("truncated {label}"));
    }
    let bytes: [u8; 4] = data[*pos..*pos + 4]
        .try_into()
        .map_err(|_| format!("invalid {label} bytes"))?;
    *pos += 4;
    Ok(u32::from_le_bytes(bytes))
}

fn decode_u64(data: &[u8], pos: &mut usize, label: &str) -> Result<u64, String> {
    if *pos + 8 > data.len() {
        return Err(format!("truncated {label}"));
    }
    let bytes: [u8; 8] = data[*pos..*pos + 8]
        .try_into()
        .map_err(|_| format!("invalid {label} bytes"))?;
    *pos += 8;
    Ok(u64::from_le_bytes(bytes))
}

fn decode_16_bytes(data: &[u8], pos: &mut usize, label: &str) -> Result<[u8; 16], String> {
    if *pos + 16 > data.len() {
        return Err(format!("truncated {label}"));
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&data[*pos..*pos + 16]);
    *pos += 16;
    Ok(out)
}

fn decode_bytes(data: &[u8], pos: &mut usize, label: &str) -> Result<Vec<u8>, String> {
    let len = decode_u32(data, pos, label)? as usize;
    if *pos + len > data.len() {
        return Err(format!("truncated {label}"));
    }
    let out = data[*pos..*pos + len].to_vec();
    *pos += len;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_query() {
        let msg = Message::Query {
            query: "User filter .age > 30".into(),
        };
        let bytes = msg.encode();
        let decoded = Message::decode(&bytes).unwrap();
        match decoded {
            Message::Query { query } => assert_eq!(query, "User filter .age > 30"),
            _ => panic!("expected Query"),
        }
    }

    #[test]
    fn test_encode_decode_connect_with_username() {
        let msg = Message::Connect {
            db_name: "mydb".into(),
            password: Some(Zeroizing::new("secret".into())),
            username: Some("alice".into()),
        };
        let bytes = msg.encode();
        match Message::decode(&bytes).unwrap() {
            Message::Connect {
                db_name,
                password,
                username,
            } => {
                assert_eq!(db_name, "mydb");
                assert_eq!(password.as_deref().map(|s| s.as_str()), Some("secret"));
                assert_eq!(username.as_deref(), Some("alice"));
            }
            other => panic!("expected Connect, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_connect_without_username() {
        // New-format Connect that explicitly carries no username.
        let msg = Message::Connect {
            db_name: "mydb".into(),
            password: Some(Zeroizing::new("secret".into())),
            username: None,
        };
        let bytes = msg.encode();
        match Message::decode(&bytes).unwrap() {
            Message::Connect {
                db_name,
                password,
                username,
            } => {
                assert_eq!(db_name, "mydb");
                assert_eq!(password.as_deref().map(|s| s.as_str()), Some("secret"));
                assert_eq!(username, None);
            }
            other => panic!("expected Connect, got {other:?}"),
        }
    }

    #[test]
    fn test_decode_old_client_connect_db_and_password_only() {
        // Simulate an OLD client frame: db_name + password, with NO username
        // bytes at all. Must decode with username: None (backward compat).
        let mut payload = encode_string("mydb");
        payload.extend_from_slice(&encode_string("pw"));
        let mut frame = vec![MSG_CONNECT, 0];
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&payload);
        match Message::decode(&frame).unwrap() {
            Message::Connect {
                db_name,
                password,
                username,
            } => {
                assert_eq!(db_name, "mydb");
                assert_eq!(password.as_deref().map(|s| s.as_str()), Some("pw"));
                assert_eq!(username, None, "old-client frame must yield username=None");
            }
            other => panic!("expected Connect, got {other:?}"),
        }
    }

    #[test]
    fn test_decode_old_client_connect_db_only() {
        // Oldest client: db_name only, no password and no username bytes.
        let payload = encode_string("mydb");
        let mut frame = vec![MSG_CONNECT, 0];
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&payload);
        match Message::decode(&frame).unwrap() {
            Message::Connect {
                db_name,
                password,
                username,
            } => {
                assert_eq!(db_name, "mydb");
                assert_eq!(password, None);
                assert_eq!(username, None);
            }
            other => panic!("expected Connect, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_result_rows() {
        let msg = Message::ResultRows {
            columns: vec!["name".into(), "age".into()],
            rows: vec![
                vec!["Alice".into(), "30".into()],
                vec!["Bob".into(), "25".into()],
            ],
        };
        let bytes = msg.encode();
        let decoded = Message::decode(&bytes).unwrap();
        match decoded {
            Message::ResultRows { columns, rows } => {
                assert_eq!(columns, vec!["name", "age"]);
                assert_eq!(rows.len(), 2);
            }
            _ => panic!("expected ResultRows"),
        }
    }

    #[test]
    fn legacy_rows_frame_is_byte_identical() {
        let encoded = Message::ResultRows {
            columns: vec!["x".into()],
            rows: vec![vec!["y".into()]],
        }
        .encode();
        assert_eq!(
            encoded,
            vec![
                0x07, 0x00, 0x10, 0x00, 0x00, 0x00, // frame header
                0x01, 0x00, // one column
                0x01, 0x00, 0x00, 0x00, b'x', // column name
                0x01, 0x00, 0x00, 0x00, // one row
                0x01, 0x00, 0x00, 0x00, b'y', // one legacy string cell
            ]
        );
    }

    #[test]
    fn native_request_tags_and_params_round_trip() {
        let cases = [
            (
                MSG_QUERY_NATIVE,
                Message::QueryNative {
                    query: "T { .x }".into(),
                },
            ),
            (
                MSG_QUERY_SQL_NATIVE,
                Message::QuerySqlNative {
                    query: "SELECT x FROM T".into(),
                },
            ),
        ];
        for (tag, message) in cases {
            let encoded = message.encode();
            assert_eq!(encoded[0], tag);
            match Message::decode(&encoded).expect("native request round trip") {
                Message::QueryNative { query } => assert_eq!(query, "T { .x }"),
                Message::QuerySqlNative { query } => assert_eq!(query, "SELECT x FROM T"),
                other => panic!("unexpected native request: {other:?}"),
            }
        }

        let encoded = Message::QueryWithParamsNative {
            query: "T filter .x = $1".into(),
            params: vec![WireParam::Int(7), WireParam::Bool(false)],
        }
        .encode();
        assert_eq!(encoded[0], MSG_QUERY_PARAMS_NATIVE);
        match Message::decode(&encoded).expect("native params round trip") {
            Message::QueryWithParamsNative { query, params } => {
                assert_eq!(query, "T filter .x = $1");
                assert_eq!(params, vec![WireParam::Int(7), WireParam::Bool(false)]);
            }
            other => panic!("unexpected native parameterized request: {other:?}"),
        }
    }

    fn every_native_value() -> Vec<Value> {
        vec![
            Value::Empty,
            Value::Int(-9_007_199_254_740_993),
            Value::Float(2.5),
            Value::Bool(true),
            Value::Str("héllo".into()),
            Value::DateTime(1_723_650_123_456_789),
            Value::Uuid([
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ]),
            Value::Bytes(vec![0x00, 0x7f, 0x80, 0xff]),
            Value::Json(
                powdb_storage::pj1::parse_json_text("9007199254740993")
                    .expect("PJ1 fixture")
                    .into_boxed_slice(),
            ),
        ]
    }

    #[test]
    fn native_rows_and_scalar_round_trip_every_type() {
        let values = every_native_value();
        let columns = (0..values.len()).map(|index| format!("c{index}")).collect();
        let encoded = Message::ResultRowsNative {
            columns,
            rows: vec![values.clone()],
        }
        .encode();
        assert_eq!(encoded[0], MSG_RESULT_ROWS_NATIVE);
        match Message::decode(&encoded).expect("native rows round trip") {
            Message::ResultRowsNative { columns, rows } => {
                assert_eq!(columns.len(), values.len());
                assert_eq!(rows, vec![values.clone()]);
            }
            other => panic!("unexpected native rows: {other:?}"),
        }

        for value in values {
            let encoded = Message::ResultScalarNative {
                value: value.clone(),
            }
            .encode();
            assert_eq!(encoded[0], MSG_RESULT_SCALAR_NATIVE);
            match Message::decode(&encoded).expect("native scalar round trip") {
                Message::ResultScalarNative { value: decoded } => assert_eq!(decoded, value),
                other => panic!("unexpected native scalar: {other:?}"),
            }
        }
    }

    #[test]
    fn native_mixed_row_matches_cross_client_golden() {
        let encoded = Message::ResultRowsNative {
            columns: ["e", "i", "f", "b", "s", "d", "u", "x", "j"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            rows: vec![every_native_value()],
        }
        .encode();
        let hex = encoded
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            hex,
            "16009c000000090001000000650100000069010000006601000000620100000073010000006401000000750100000078010000006a0100000000000000000108000000ffffffffffffdfff02080000000000000000000440030100000001040600000068c3a96c6c6f050800000015615391a61f0600061000000000112233445566778899aabbccddeeff0704000000007f80ff0809000000030100000000002000"
        );
    }

    fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![tag, 0];
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    fn typed_cell(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut payload = vec![tag];
        payload.extend_from_slice(&(body.len() as u32).to_le_bytes());
        payload.extend_from_slice(body);
        payload
    }

    #[test]
    fn native_scalar_rejects_malformed_cells() {
        let malformed = [
            typed_cell(0xff, &[]),
            typed_cell(TypeId::Int as u8, &[0; 7]),
            typed_cell(TypeId::Bool as u8, &[2]),
            typed_cell(TypeId::Str as u8, &[0xff]),
            typed_cell(TypeId::Json as u8, &[0xff]),
            typed_cell(TypeId::Json as u8, &[0, 0]),
        ];
        for payload in malformed {
            assert!(Message::decode(&frame(MSG_RESULT_SCALAR_NATIVE, &payload)).is_err());
        }

        let mut trailing = typed_cell(TypeId::Empty as u8, &[]);
        trailing.push(0);
        assert!(Message::decode(&frame(MSG_RESULT_SCALAR_NATIVE, &trailing)).is_err());
    }

    #[test]
    fn native_rows_rejects_bad_counts_and_trailing_data() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u16.to_le_bytes());
        payload.extend_from_slice(&encode_string("x"));
        payload.extend_from_slice(&2u32.to_le_bytes());
        payload.extend_from_slice(&typed_cell(TypeId::Empty as u8, &[]));
        assert!(Message::decode(&frame(MSG_RESULT_ROWS_NATIVE, &payload)).is_err());

        payload[7..11].copy_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&[0xaa]);
        assert!(Message::decode(&frame(MSG_RESULT_ROWS_NATIVE, &payload)).is_err());
    }

    #[test]
    fn test_encode_decode_result_message() {
        let msg = Message::ResultMessage {
            message: "type User created".into(),
        };
        let bytes = msg.encode();
        let decoded = Message::decode(&bytes).unwrap();
        match decoded {
            Message::ResultMessage { message } => assert_eq!(message, "type User created"),
            _ => panic!("expected ResultMessage"),
        }
    }

    #[test]
    fn test_encode_decode_error() {
        let msg = Message::Error {
            message: "table not found".into(),
        };
        let bytes = msg.encode();
        let decoded = Message::decode(&bytes).unwrap();
        match decoded {
            Message::Error { message } => assert_eq!(message, "table not found"),
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn test_encode_decode_query_sql() {
        let msg = Message::QuerySql {
            query: "SELECT * FROM User".into(),
        };
        let decoded = Message::decode(&msg.encode()).unwrap();
        match decoded {
            Message::QuerySql { query } => assert_eq!(query, "SELECT * FROM User"),
            other => panic!("expected QuerySql, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_query_with_params() {
        let msg = Message::QueryWithParams {
            query: "insert User { name := $1, age := $2, ok := $3, note := $4 }".into(),
            params: vec![
                WireParam::Str(r#"a"b\c; drop User"#.into()),
                WireParam::Int(-7),
                WireParam::Bool(true),
                WireParam::Null,
            ],
        };
        let bytes = msg.encode();
        // The new frame must use the dedicated 0x04 tag.
        assert_eq!(bytes[0], 0x04);
        match Message::decode(&bytes).unwrap() {
            Message::QueryWithParams { query, params } => {
                assert!(query.contains("$1"));
                assert_eq!(params.len(), 4);
                assert!(matches!(&params[0], WireParam::Str(s) if s == r#"a"b\c; drop User"#));
                assert!(matches!(&params[1], WireParam::Int(-7)));
                assert!(matches!(&params[2], WireParam::Bool(true)));
                assert!(matches!(&params[3], WireParam::Null));
            }
            other => panic!("expected QueryWithParams, got {other:?}"),
        }
    }

    #[test]
    fn test_query_with_params_float_round_trip() {
        let msg = Message::QueryWithParams {
            query: "T filter .f = $1".into(),
            params: vec![WireParam::Float(2.5)],
        };
        match Message::decode(&msg.encode()).unwrap() {
            Message::QueryWithParams { params, .. } => {
                assert!(matches!(&params[0], WireParam::Float(f) if (*f - 2.5).abs() < 1e-12));
            }
            other => panic!("expected QueryWithParams, got {other:?}"),
        }
    }

    fn sample_sync_status() -> WireSyncStatus {
        WireSyncStatus {
            replica_id: "replica-a".into(),
            active: true,
            last_applied_lsn: Some(7),
            remote_lsn: 10,
            servable_lsn: Some(10),
            unarchived_lsn: Some(0),
            lag_lsn: Some(3),
            lag_bytes: Some(2048),
            lag_ms: Some(5000),
            stale: true,
            repair_action: WireSyncRepairAction::Pull,
            last_sync_error: None,
        }
    }

    #[test]
    fn test_encode_decode_sync_requests() {
        let database_id = *b"sync-protocol!!!";
        let pull = Message::SyncPull {
            replica_id: "replica-a".into(),
            since_lsn: 7,
            max_units: 128,
            max_bytes: 4096,
            database_id,
            primary_generation: 9,
            wal_format_version: 1,
            catalog_version: 2,
            segment_format_version: 1,
        };
        let bytes = pull.encode();
        assert_eq!(bytes[0], MSG_SYNC_PULL);
        match Message::decode(&bytes).unwrap() {
            Message::SyncPull {
                replica_id,
                since_lsn,
                max_units,
                max_bytes,
                database_id: decoded_database_id,
                primary_generation,
                wal_format_version,
                catalog_version,
                segment_format_version,
            } => {
                assert_eq!(replica_id, "replica-a");
                assert_eq!(since_lsn, 7);
                assert_eq!(max_units, 128);
                assert_eq!(max_bytes, 4096);
                assert_eq!(decoded_database_id, database_id);
                assert_eq!(primary_generation, 9);
                assert_eq!(wal_format_version, 1);
                assert_eq!(catalog_version, 2);
                assert_eq!(segment_format_version, 1);
            }
            other => panic!("expected SyncPull, got {other:?}"),
        }

        let ack = Message::SyncAck {
            replica_id: "replica-a".into(),
            applied_lsn: 10,
            remote_lsn: 10,
        };
        match Message::decode(&ack.encode()).unwrap() {
            Message::SyncAck {
                replica_id,
                applied_lsn,
                remote_lsn,
            } => {
                assert_eq!(replica_id, "replica-a");
                assert_eq!(applied_lsn, 10);
                assert_eq!(remote_lsn, 10);
            }
            other => panic!("expected SyncAck, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_decode_sync_results() {
        let status = sample_sync_status();
        match Message::decode(
            &Message::SyncStatusResult {
                status: status.clone(),
            }
            .encode(),
        )
        .unwrap()
        {
            Message::SyncStatusResult { status: decoded } => assert_eq!(decoded, status),
            other => panic!("expected SyncStatusResult, got {other:?}"),
        }

        let await_archive_status = WireSyncStatus {
            servable_lsn: Some(7),
            unarchived_lsn: Some(3),
            repair_action: WireSyncRepairAction::AwaitArchive,
            last_sync_error: Some("primary WAL is not yet archived".into()),
            ..status.clone()
        };
        match Message::decode(
            &Message::SyncStatusResult {
                status: await_archive_status.clone(),
            }
            .encode(),
        )
        .unwrap()
        {
            Message::SyncStatusResult { status: decoded } => {
                assert_eq!(decoded, await_archive_status)
            }
            other => panic!("expected AwaitArchive SyncStatusResult, got {other:?}"),
        }

        let units = vec![
            WireRetainedUnit {
                tx_id: 1,
                record_type: 4,
                lsn: 8,
                data: vec![1, 2, 3],
            },
            WireRetainedUnit {
                tx_id: 1,
                record_type: 4,
                lsn: 9,
                data: vec![4, 5],
            },
        ];
        let empty_pull_len = Message::SyncPullResult {
            status: status.clone(),
            units: Vec::new(),
            has_more: true,
        }
        .encode()
        .len();
        let populated_pull_len = Message::SyncPullResult {
            status: status.clone(),
            units: units.clone(),
            has_more: true,
        }
        .encode()
        .len();
        let expected_unit_len: u64 = units
            .iter()
            .map(WireRetainedUnit::encoded_len)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .into_iter()
            .sum();
        assert_eq!(
            u64::try_from(populated_pull_len - empty_pull_len).unwrap(),
            expected_unit_len,
            "retained-unit encoded length must track SyncPullResult wire shape"
        );

        match Message::decode(
            &Message::SyncPullResult {
                status: status.clone(),
                units: units.clone(),
                has_more: true,
            }
            .encode(),
        )
        .unwrap()
        {
            Message::SyncPullResult {
                status: decoded_status,
                units: decoded_units,
                has_more,
            } => {
                assert_eq!(decoded_status, status);
                assert_eq!(decoded_units, units);
                assert!(has_more);
            }
            other => panic!("expected SyncPullResult, got {other:?}"),
        }

        match Message::decode(
            &Message::SyncAckResult {
                previous_applied_lsn: 7,
                applied_lsn: 10,
                remote_lsn: 10,
                advanced: true,
                status: WireSyncStatus {
                    stale: false,
                    repair_action: WireSyncRepairAction::None,
                    lag_lsn: Some(0),
                    lag_bytes: Some(0),
                    lag_ms: Some(0),
                    ..status
                },
            }
            .encode(),
        )
        .unwrap()
        {
            Message::SyncAckResult {
                previous_applied_lsn,
                applied_lsn,
                remote_lsn,
                advanced,
                status,
            } => {
                assert_eq!(previous_applied_lsn, 7);
                assert_eq!(applied_lsn, 10);
                assert_eq!(remote_lsn, 10);
                assert!(advanced);
                assert!(!status.stale);
            }
            other => panic!("expected SyncAckResult, got {other:?}"),
        }
    }

    #[test]
    fn test_decode_garbage_never_panics() {
        // Feed a wide range of malformed/truncated byte sequences to the
        // wire-protocol decode path. Every one must return Err, never panic:
        // a malformed client message must not crash the server.
        let cases: Vec<Vec<u8>> = vec![
            vec![],                                   // empty
            vec![0x03],                               // 1 byte, shorter than header
            vec![0x03, 0x00, 0x00, 0x00, 0x00],       // 5 bytes, header truncated by one
            vec![0xFF, 0x00, 0x00, 0x00, 0x00, 0x00], // unknown message type
            // QUERY with payload_len far exceeding the frame.
            vec![0x03, 0x00, 0xFF, 0xFF, 0xFF, 0xFF],
            // SYNC_PULL with only a replica id and no fixed fields.
            {
                let mut payload = encode_string("replica-a");
                let mut frame = vec![MSG_SYNC_PULL, 0];
                frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                frame.append(&mut payload);
                frame
            },
            // SYNC_PULL_RESULT with an amplified retained-unit count.
            {
                let mut payload = encode_sync_status(&sample_sync_status());
                payload.extend_from_slice(&((MAX_SYNC_UNITS as u32) + 1).to_le_bytes());
                let mut frame = vec![MSG_SYNC_PULL_RESULT, 0];
                frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                frame.extend_from_slice(&payload);
                frame
            },
            // CONNECT claiming a string len of 0xFFFFFFFF but no data.
            vec![0x01, 0x00, 0x04, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF],
            // RESULT_ROWS claiming a huge column count with no data.
            vec![0x07, 0x00, 0x02, 0x00, 0x00, 0x00, 0xFF, 0xFF],
            // RESULT_OK with a truncated 8-byte affected field.
            vec![0x09, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03],
            // QUERY_PARAMS (0x04) claiming a query string len with no data.
            vec![0x04, 0x00, 0x04, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF],
            // QUERY_PARAMS: empty query string, claims 1 param, no param bytes.
            vec![
                0x04, 0x00, 0x06, 0x00, 0x00, 0x00, // header, payload_len=6
                0x00, 0x00, 0x00, 0x00, // query string len = 0
                0x01, 0x00, // param count = 1, then nothing
            ],
            // QUERY_PARAMS: 1 int param with a truncated i64 body.
            vec![
                0x04, 0x00, 0x0B, 0x00, 0x00, 0x00, // header, payload_len=11
                0x00, 0x00, 0x00, 0x00, // query len = 0
                0x01, 0x00, // param count = 1
                0x01, // tag = int, then only 3 of 8 bytes
                0x01, 0x02, 0x03,
            ],
            // QUERY_PARAMS: 1 str param with a truncated string body.
            vec![
                0x04, 0x00, 0x0F, 0x00, 0x00, 0x00, // header, payload_len=15
                0x00, 0x00, 0x00, 0x00, // query len = 0
                0x01, 0x00, // param count = 1
                0x04, // tag = str
                0xFF, 0xFF, 0xFF, 0xFF, // str len huge, no data
            ],
            // QUERY_PARAMS: unknown param tag byte.
            vec![
                0x04, 0x00, 0x0B, 0x00, 0x00, 0x00, // header, payload_len=11
                0x00, 0x00, 0x00, 0x00, // query len = 0
                0x01, 0x00, // param count = 1
                0x63, // bogus tag
            ],
        ];
        for bytes in cases {
            let result = Message::decode(&bytes);
            assert!(
                result.is_err(),
                "expected Err for malformed input {bytes:?}, got {result:?}"
            );
        }
    }

    #[test]
    fn test_decode_arbitrary_prefixes_never_panic() {
        // Exhaustively walk every truncation of a well-formed frame plus a
        // few byte mutations. None may panic.
        let valid = Message::ResultRows {
            columns: vec!["a".into(), "b".into()],
            rows: vec![vec!["1".into(), "2".into()]],
        }
        .encode();
        for end in 0..valid.len() {
            // Every prefix that is not the full frame must be rejected, not panic.
            let _ = Message::decode(&valid[..end]);
        }
        // Mutate each byte to a high value and confirm no panic.
        for i in 0..valid.len() {
            let mut m = valid.clone();
            m[i] = 0xFF;
            let _ = Message::decode(&m);
        }
    }

    #[test]
    fn test_decode_result_rows_rejects_amplified_row_count() {
        // A tiny frame that declares col_count=0 and row_count=10_000_000.
        // With zero columns each row consumes no bytes, so the old decoder
        // would allocate/iterate 10M empty rows from ~12 bytes (reachable
        // pre-auth). The amplification guard must reject it.
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u16.to_le_bytes()); // col_count = 0
        payload.extend_from_slice(&10_000_000u32.to_le_bytes()); // row_count = 10M
        let mut frame = vec![MSG_RESULT_ROWS, 0];
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&payload);
        assert!(
            Message::decode(&frame).is_err(),
            "amplified row count must be rejected"
        );

        // A normal small ResultRows must still round-trip unchanged.
        let msg = Message::ResultRows {
            columns: vec!["a".into()],
            rows: vec![vec!["x".into()]],
        };
        match Message::decode(&msg.encode()).unwrap() {
            Message::ResultRows { columns, rows } => {
                assert_eq!(columns, vec!["a"]);
                assert_eq!(rows, vec![vec!["x".to_string()]]);
            }
            other => panic!("expected ResultRows, got {other:?}"),
        }
    }

    #[test]
    fn test_frame_length() {
        let msg = Message::Query {
            query: "User".into(),
        };
        let bytes = msg.encode();
        assert!(bytes.len() >= 6);
        let payload_len = u32::from_le_bytes(bytes[2..6].try_into().unwrap()) as usize;
        assert_eq!(bytes.len(), 6 + payload_len);
    }
}
