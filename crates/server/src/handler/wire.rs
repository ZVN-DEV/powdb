//! Frame plumbing: bounded reply writes, the cancellation-safe frame
//! reader and its in-flight read-ahead queue, and the encoding of a
//! [`QueryResult`] into a response frame.

use super::classify::error_response;
use crate::protocol::{frame_payload_len, ErrorClass, Message};
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};

/// Maximum payload accepted by the post-auth cancellation-safe frame reader.
/// Keep this equal to the protocol reader's public wire limit.
pub(super) const MAX_WIRE_PAYLOAD_SIZE: usize = 64 * 1024 * 1024;

/// Frames received while a query is executing are retained for normal
/// pipelined processing. Both limits are deliberately much smaller than the
/// ordinary 64 MiB per-frame protocol limit: in-flight read-ahead is merely a
/// liveness aid, not a second request buffer.
///
/// Reaching either cap PAUSES read-ahead until the queue drains, so a burst
/// of any size still completes in order. Cancelling the query and closing the
/// connection (the previous behavior) turned an ordinary `Promise.all` of 200
/// queries into 22 answers and an ECONNRESET, with no Error frame to say why.
/// A client that has a frame queued here is by definition still connected, so
/// nothing is lost by not watching its socket for the rest of the statement.
pub(super) const MAX_IN_FLIGHT_READ_AHEAD_FRAMES: usize = 128;

pub(super) const MAX_IN_FLIGHT_READ_AHEAD_BYTES: usize = 1024 * 1024;

/// Maximum encoded response payload size (64 MB). The wire format is still a
/// single frame today, so oversized result sets must fail cleanly instead of
/// building an unbounded `Vec<Vec<String>>` and frame in memory.
#[cfg(not(test))]
pub(super) const MAX_RESPONSE_PAYLOAD_SIZE: usize = 64 * 1024 * 1024;

#[cfg(test)]
pub(super) const MAX_RESPONSE_PAYLOAD_SIZE: usize = 1024;

/// Timeout for writing a response to the client. Prevents slow-drain
/// clients from blocking the handler indefinitely.
pub(super) const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Write a message to the client with a timeout. Returns false if the
/// write failed or timed out (caller should close the connection).
pub(super) async fn write_msg<W: AsyncWrite + Unpin>(
    writer: &mut BufWriter<W>,
    msg: &Message,
) -> bool {
    write_msg_with_budget(writer, msg, WRITE_TIMEOUT).await
}

/// How long a reply to this connection may block, given the transaction
/// deadline (if any) it is being written under.
///
/// [`WRITE_TIMEOUT`] alone is not enough while a transaction holds the gate.
/// The reap only runs between frames, so a client that opens a transaction,
/// asks for a reply larger than the socket buffers, and then stops reading
/// parks the handler inside the write for the full `WRITE_TIMEOUT` with the
/// gate still held. That is the same outage `POWDB_TX_MAX_LIFETIME_MS` exists
/// to prevent, reached through the write side instead of the read side, and it
/// made the advertised budget false by 30s/budget (100x at the default). The
/// remaining lifetime therefore caps the write budget too, so the gate is
/// released on the budget the operator configured no matter which side of the
/// socket the client stalls.
pub(super) fn write_budget(tx_deadline: Option<Instant>) -> Duration {
    match tx_deadline {
        Some(deadline) => WRITE_TIMEOUT.min(deadline.saturating_duration_since(Instant::now())),
        None => WRITE_TIMEOUT,
    }
}

/// [`write_msg`] bounded by an explicit budget.
pub(super) async fn write_msg_with_budget<W: AsyncWrite + Unpin>(
    writer: &mut BufWriter<W>,
    msg: &Message,
    budget: Duration,
) -> bool {
    let write_fut = async {
        if msg.write_to(writer).await.is_err() {
            return false;
        }
        writer.flush().await.is_ok()
    };
    tokio::time::timeout(budget, write_fut)
        .await
        .unwrap_or_default()
}

/// How long the connection may spend flushing what is left in its buffer on
/// the way out. Long enough for a client that is merely slow, short enough
/// that a client which stopped reading cannot park the teardown.
pub(super) const FINAL_FLUSH_BUDGET: Duration = Duration::from_millis(250);

/// Push whatever is still buffered to the socket before the connection ends.
///
/// `BufWriter` has no `Drop` that flushes, so a frame that was written into it
/// but not yet drained is discarded when the connection object goes away. Every
/// reply path flushes on its own, but a flush that is cancelled by its budget
/// leaves the remainder in the buffer, and that remainder is the tail of a
/// frame the client is waiting on. Returns whether the buffer actually drained.
pub(super) async fn flush_before_close<W: AsyncWrite + Unpin>(writer: &mut BufWriter<W>) -> bool {
    tokio::time::timeout(FINAL_FLUSH_BUDGET, writer.flush())
        .await
        .is_ok_and(|result| result.is_ok())
}

/// [`write_msg`] for replies sent while this connection may be holding the
/// transaction gate: the write can never outlast the transaction's remaining
/// lifetime. See [`write_budget`].
pub(super) async fn write_msg_within<W: AsyncWrite + Unpin>(
    writer: &mut BufWriter<W>,
    msg: &Message,
    tx_deadline: Option<Instant>,
) -> bool {
    write_msg_with_budget(writer, msg, write_budget(tx_deadline)).await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WireResultMode {
    LegacyText,
    Native,
}

pub(super) fn is_success_response(msg: &Message) -> bool {
    matches!(
        msg,
        Message::ResultRows { .. }
            | Message::ResultScalar { .. }
            | Message::ResultRowsNative { .. }
            | Message::ResultScalarNative { .. }
            | Message::ResultOk { .. }
            | Message::ResultMessage { .. }
    )
}

pub(super) fn is_query_cancellation_response(message: &Message) -> bool {
    matches!(
        message,
        Message::Error { message } | Message::ErrorWithClass { message, .. }
            if message.starts_with("query timeout after")
                || message == "query cancelled by client disconnect"
    )
}

/// Read one post-auth wire frame without losing partially-read bytes when the
/// future is cancelled by `tokio::select!`.
///
/// `Message::read_from` uses `read_exact`, whose future is not cancellation
/// safe: racing it against query completion can consume part of the next frame
/// and then drop those bytes. This reader stores every completed `read` in a
/// connection-owned buffer before awaiting again, so a query may safely race
/// socket EOF / `DISCONNECT` while preserving ordinary pipelined frames.
pub(super) struct DecodedWireMessage {
    pub(super) message: Message,
    pub(super) wire_len: usize,
}

#[derive(Default)]
pub(super) struct InFlightReadAhead {
    pub(super) frames: VecDeque<DecodedWireMessage>,
    pub(super) wire_bytes: usize,
}

impl InFlightReadAhead {
    /// Whether another frame may be read ahead while a query runs.
    ///
    /// Reaching either cap pauses the read arm rather than closing the
    /// connection. The queue may overshoot the byte cap by at most the one
    /// frame that crosses it, which the protocol already bounds.
    pub(super) fn has_room(&self) -> bool {
        self.frames.len() < MAX_IN_FLIGHT_READ_AHEAD_FRAMES
            && self.wire_bytes < MAX_IN_FLIGHT_READ_AHEAD_BYTES
    }

    pub(super) fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub(super) fn push_back(&mut self, frame: DecodedWireMessage) {
        self.wire_bytes += frame.wire_len;
        self.frames.push_back(frame);
    }

    pub(super) fn pop_front(&mut self) -> Option<Message> {
        let frame = self.frames.pop_front()?;
        self.wire_bytes -= frame.wire_len;
        Some(frame.message)
    }
}

/// The socket side of one connection, as a running query sees it: the buffered
/// reader, the partial frame accumulated ahead of it, and the complete frames
/// that arrived while the query was executing.
///
/// The three are one piece of state. Every query path that watches the socket
/// needs all three and none of them is meaningful without the others, so they
/// travel as a single borrow rather than as three arguments repeated down the
/// call chain.
pub(super) struct FrameStream<'a, R> {
    pub(super) reader: &'a mut BufReader<R>,
    pub(super) buffered: &'a mut Vec<u8>,
    pub(super) pending: &'a mut InFlightReadAhead,
}

impl<R> FrameStream<'_, R> {
    /// Borrow the same stream again for a shorter lifetime, so a caller that
    /// owns it can hand it to several calls in sequence.
    pub(super) fn reborrow(&mut self) -> FrameStream<'_, R> {
        FrameStream {
            reader: &mut *self.reader,
            buffered: &mut *self.buffered,
            pending: &mut *self.pending,
        }
    }
}

/// Why the frame reader stopped.
///
/// The distinction is what a client is owed. A frame refused on its own
/// content is the client's mistake and must be named before the connection
/// closes; a transport failure has no reply and nowhere to put one. Before
/// this split both ended as a bare close, so a client that sent 4097
/// parameters and a client whose server crashed saw the same ECONNRESET.
pub(super) enum FrameReadError {
    /// The frame is refused on its own content: it declares more payload than
    /// the wire limit, or its bytes do not decode. The reply is boxed because
    /// `Message` is a large enum and this error travels in every frame read's
    /// `Result`.
    Refused {
        reply: Box<Message>,
        detail: String,
    },
    /// The socket failed, or the peer went away mid-frame.
    Transport(std::io::Error),
}

impl FrameReadError {
    fn refused(detail: String, class: ErrorClass) -> Self {
        FrameReadError::Refused {
            reply: Box::new(error_response(detail.clone(), class)),
            detail,
        }
    }

    /// Whether the peer simply went away, as opposed to something the
    /// operator should look at. A load balancer that opens a TCP connection
    /// and closes it produces one of these on every probe.
    pub(super) fn is_peer_gone(&self) -> bool {
        match self {
            FrameReadError::Refused { .. } => false,
            FrameReadError::Transport(e) => matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ),
        }
    }
}

impl std::fmt::Display for FrameReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameReadError::Refused { detail, .. } => f.write_str(detail),
            FrameReadError::Transport(e) => write!(f, "{e}"),
        }
    }
}

impl From<std::io::Error> for FrameReadError {
    fn from(e: std::io::Error) -> Self {
        FrameReadError::Transport(e)
    }
}

pub(super) async fn read_message_cancel_safe<R>(
    reader: &mut BufReader<R>,
    buffered: &mut Vec<u8>,
    max_frame_len: usize,
) -> Result<Option<DecodedWireMessage>, FrameReadError>
where
    R: AsyncRead + Unpin,
{
    loop {
        if let Some(declared_payload_len) = frame_payload_len(buffered) {
            let payload_len = declared_payload_len as usize;
            if payload_len > MAX_WIRE_PAYLOAD_SIZE {
                return Err(FrameReadError::refused(
                    format!("payload too large: {payload_len} bytes (max {MAX_WIRE_PAYLOAD_SIZE})"),
                    ErrorClass::LimitExceeded,
                ));
            }
            let frame_len = 6usize.checked_add(payload_len).ok_or_else(|| {
                FrameReadError::refused(
                    "wire frame length overflow".to_string(),
                    ErrorClass::LimitExceeded,
                )
            })?;
            if frame_len > max_frame_len {
                return Err(FrameReadError::refused(
                    format!(
                        "wire frame exceeds the available in-flight read-ahead budget: \
                         {frame_len} bytes (available {max_frame_len})"
                    ),
                    ErrorClass::LimitExceeded,
                ));
            }
            if buffered.len() >= frame_len {
                let frame: Vec<u8> = buffered.drain(..frame_len).collect();
                return match Message::decode(&frame) {
                    Ok(message) => Ok(Some(DecodedWireMessage {
                        message,
                        wire_len: frame_len,
                    })),
                    Err(e) => Err(FrameReadError::refused(e.to_string(), e.class())),
                };
            }
        }

        let mut chunk = [0u8; 8192];
        // Read only the bytes needed for this frame stage. Besides preserving
        // cancellation safety, this prevents a large pipelined payload from
        // overshooting the in-flight byte budget in one buffered read.
        let wanted = match frame_payload_len(buffered) {
            // Not a whole header yet, so the next read is the rest of it.
            None => 6 - buffered.len(),
            Some(payload_len) => 6usize
                .checked_add(payload_len as usize)
                .and_then(|frame_len| frame_len.checked_sub(buffered.len()))
                .ok_or_else(|| {
                    FrameReadError::refused(
                        "invalid buffered wire frame length".to_string(),
                        ErrorClass::LimitExceeded,
                    )
                })?,
        };
        let read_limit = wanted.min(chunk.len());
        let read = reader.read(&mut chunk[..read_limit]).await?;
        if read == 0 {
            if buffered.len() < 6 {
                // Match the existing protocol behavior: EOF before a complete
                // header is a clean connection close.
                buffered.clear();
                return Ok(None);
            }
            return Err(FrameReadError::Transport(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed in the middle of a wire frame",
            )));
        }
        buffered.extend_from_slice(&chunk[..read]);
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ConnectionTermination {
    Closed,
    ReadError,
}

/// The encoded response would not fit in one wire frame.
///
/// A server-side refusal with no engine error behind it: the statement
/// succeeded and only its rendering is too big. It is its own type rather
/// than a `QueryError::Execution` whose text the classifier had to recognize,
/// because a substring search is what decided its wire class before, and a
/// reworded message would silently have downgraded it to class 2.
#[derive(Debug)]
pub(super) struct ResponseTooLarge {
    limit_bytes: usize,
}

impl std::fmt::Display for ResponseTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "result too large: encoded response exceeds {} bytes; add a limit or narrower projection",
            self.limit_bytes
        )
    }
}

fn charge_response_bytes(total: &mut usize, bytes: usize) -> Result<(), ResponseTooLarge> {
    *total = total.saturating_add(bytes);
    if *total > MAX_RESPONSE_PAYLOAD_SIZE {
        return Err(ResponseTooLarge {
            limit_bytes: MAX_RESPONSE_PAYLOAD_SIZE,
        });
    }
    Ok(())
}

pub(super) fn native_value_body_len(value: &Value) -> usize {
    match value {
        Value::Empty => 0,
        Value::Int(_) | Value::Float(_) | Value::DateTime(_) => 8,
        Value::Bool(_) => 1,
        Value::Str(value) => value.len(),
        Value::Uuid(_) => 16,
        Value::Bytes(value) => value.len(),
        Value::Json(value) => value.len(),
    }
}

pub(super) fn query_result_to_message(
    result: QueryResult,
    result_mode: WireResultMode,
) -> Result<Message, ResponseTooLarge> {
    match result {
        QueryResult::Rows { columns, rows } => {
            let mut encoded_bytes = 2usize; // column count
            for col in &columns {
                charge_response_bytes(&mut encoded_bytes, 4 + col.len())?;
            }
            charge_response_bytes(&mut encoded_bytes, 4)?; // row count

            match result_mode {
                WireResultMode::Native => {
                    for row in &rows {
                        for value in row {
                            charge_response_bytes(
                                &mut encoded_bytes,
                                5 + native_value_body_len(value),
                            )?;
                        }
                    }
                    Ok(Message::ResultRowsNative { columns, rows })
                }
                WireResultMode::LegacyText => {
                    let mut str_rows = Vec::with_capacity(rows.len());
                    for row in rows {
                        let mut str_row = Vec::with_capacity(row.len());
                        for value in row {
                            let display = value_to_display(&value);
                            charge_response_bytes(&mut encoded_bytes, 4 + display.len())?;
                            str_row.push(display);
                        }
                        str_rows.push(str_row);
                    }
                    Ok(Message::ResultRows {
                        columns,
                        rows: str_rows,
                    })
                }
            }
        }
        QueryResult::Scalar(value) => match result_mode {
            WireResultMode::Native => {
                let mut encoded_bytes = 0;
                charge_response_bytes(&mut encoded_bytes, 5 + native_value_body_len(&value))?;
                Ok(Message::ResultScalarNative { value })
            }
            WireResultMode::LegacyText => Ok(Message::ResultScalar {
                value: value_to_display(&value),
            }),
        },
        QueryResult::Modified(n) => Ok(Message::ResultOk { affected: n }),
        QueryResult::Created(name) => Ok(Message::ResultMessage {
            message: format!("type {name} created"),
        }),
        QueryResult::Executed { message } => Ok(Message::ResultMessage { message }),
    }
}

// Canonical wire rendering lives on `Value` (`powdb_storage`) so the server,
// CLI, and embedded bindings render results identically. Kept as a thin alias
// to minimize churn at the call sites in this module.
pub(super) fn value_to_display(v: &Value) -> String {
    v.to_wire_string()
}
