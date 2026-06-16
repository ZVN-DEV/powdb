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
const MSG_RESULT_ROWS: u8 = 0x07;
const MSG_RESULT_SCALAR: u8 = 0x08;
const MSG_RESULT_OK: u8 = 0x09;
const MSG_ERROR: u8 = 0x0A;
const MSG_RESULT_MSG: u8 = 0x0B;
const MSG_DISCONNECT: u8 = 0x10;
const MSG_PING: u8 = 0x11;
const MSG_PONG: u8 = 0x12;

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
    ResultRows {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    ResultScalar {
        value: String,
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
    /// Encode message into wire format: [type(1)][flags(1)][len(4)][payload]
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
                let mut buf = encode_string(query);
                buf.extend_from_slice(&(params.len() as u16).to_le_bytes());
                for p in params {
                    match p {
                        WireParam::Null => buf.push(0),
                        WireParam::Int(v) => {
                            buf.push(1);
                            buf.extend_from_slice(&v.to_le_bytes());
                        }
                        WireParam::Float(v) => {
                            buf.push(2);
                            buf.extend_from_slice(&v.to_le_bytes());
                        }
                        WireParam::Bool(v) => {
                            buf.push(3);
                            buf.push(if *v { 1 } else { 0 });
                        }
                        WireParam::Str(s) => {
                            buf.push(4);
                            buf.extend_from_slice(&encode_string(s));
                        }
                    }
                }
                (MSG_QUERY_PARAMS, buf)
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
                let mut params = Vec::with_capacity(count);
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
                let mut columns = Vec::with_capacity(col_count);
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
