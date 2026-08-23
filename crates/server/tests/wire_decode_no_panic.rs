//! Malformed-frame probes for every fixed-width decode on the wire path.
//!
//! The release profile builds with `panic = "abort"` (`Cargo.toml`), a
//! deliberate crash-only choice tied to the single `RwLock<Engine>`. Its
//! consequence is that a panic while decoding bytes a remote peer sent is not
//! one failed request: it is a process abort that takes every other connected
//! client down with it. Every fixed-width read on that path must therefore
//! return a typed protocol error, never assert an invariant with `.expect()`.
//!
//! Each probe below drives a real frame through the real `Message::decode`
//! entry point (or, for the frame-header reads the connection loop performs,
//! through the real `frame_payload_len` helper it calls) and asserts an `Err`
//! comes back. Under the test profile, which unwinds, a reintroduced
//! `.expect()` on any of these paths surfaces as a panicking test rather than
//! a silent regression.
//!
//! These probes are deliberately written against inputs whose *upstream*
//! length checks are what keeps the fixed-width read in range today. They
//! therefore pass both before and after the `.expect()` conversion: their job
//! is to pin the whole path as panic-free, so a future edit that weakens one
//! of those upstream checks fails here instead of aborting a production
//! server. Each was verified non-vacuous by weakening the matching upstream
//! check and observing the probe panic.

use powdb_server::protocol::{
    frame_payload_len, Message, MSG_QUERY_PARAMS_NATIVE, MSG_RESULT_ROWS_NATIVE,
    MSG_RESULT_SCALAR_NATIVE,
};
use powdb_storage::types::TypeId;

/// Wrap a payload in the 6-byte wire header the server frames everything with.
fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + payload.len());
    out.push(tag);
    out.push(0); // flags
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// A length-prefixed wire string.
fn wire_string(value: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + value.len());
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    out
}

/// A `MSG_QUERY_PARAMS_NATIVE` payload: query text, one parameter, and `body`
/// as that parameter's raw bytes after its `tag`.
fn params_native_payload(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut payload = wire_string("User");
    payload.extend_from_slice(&1u16.to_le_bytes()); // one parameter
    payload.push(tag);
    payload.extend_from_slice(body);
    payload
}

/// A typed-value block: 1-byte type tag, 4-byte LE declared body length, body.
fn typed_value(type_id: TypeId, declared_len: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + body.len());
    out.push(type_id as u8);
    out.extend_from_slice(&declared_len.to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// Assert `Message::decode` refuses `frame` with an error instead of panicking.
#[track_caller]
fn must_refuse(frame: &[u8], what: &str) -> String {
    match Message::decode(frame) {
        Ok(message) => panic!("{what}: expected a decode error, got {message:?}"),
        Err(error) => error,
    }
}

// ---------------------------------------------------------------------------
// decode_query_with_params_exact: the 8-byte int and float parameter bodies
// ---------------------------------------------------------------------------

#[test]
fn truncated_int_parameter_is_refused_not_panicked() {
    // An int parameter promises 8 bytes and delivers 7.
    let payload = params_native_payload(1, &[0u8; 7]);
    let error = must_refuse(
        &frame(MSG_QUERY_PARAMS_NATIVE, &payload),
        "truncated int param",
    );
    assert!(
        error.contains("int param"),
        "the error must name the field it refused, got: {error}"
    );
}

#[test]
fn truncated_float_parameter_is_refused_not_panicked() {
    let payload = params_native_payload(2, &[0u8; 3]);
    let error = must_refuse(
        &frame(MSG_QUERY_PARAMS_NATIVE, &payload),
        "truncated float param",
    );
    assert!(
        error.contains("float param"),
        "the error must name the field it refused, got: {error}"
    );
}

#[test]
fn int_parameter_with_no_body_at_all_is_refused() {
    let payload = params_native_payload(1, &[]);
    must_refuse(
        &frame(MSG_QUERY_PARAMS_NATIVE, &payload),
        "int param with an empty body",
    );
}

// ---------------------------------------------------------------------------
// decode_typed_value: the int, float, datetime and UUID fixed-width bodies
// ---------------------------------------------------------------------------

/// A fixed-width typed value whose declared body length does not match the
/// width the variant requires. The declared length is honest about the bytes
/// that follow, so the frame is well-formed right up to the fixed-width read:
/// only the variant's own width check stands between the wire and it.
#[track_caller]
fn refuse_wrong_width(type_id: TypeId, wrong_len: usize) {
    let payload = typed_value(type_id, wrong_len as u32, &vec![0u8; wrong_len]);
    let error = must_refuse(
        &frame(MSG_RESULT_SCALAR_NATIVE, &payload),
        &format!("{type_id:?} with a {wrong_len}-byte body"),
    );
    assert!(
        error.contains("length"),
        "the error must explain the length mismatch for {type_id:?}, got: {error}"
    );
}

#[test]
fn int_typed_value_with_a_wrong_fixed_width_is_refused_not_panicked() {
    refuse_wrong_width(TypeId::Int, 4);
}

#[test]
fn float_typed_value_with_a_wrong_fixed_width_is_refused_not_panicked() {
    refuse_wrong_width(TypeId::Float, 7);
}

#[test]
fn datetime_typed_value_with_a_wrong_fixed_width_is_refused_not_panicked() {
    refuse_wrong_width(TypeId::DateTime, 9);
}

#[test]
fn uuid_typed_value_with_a_wrong_fixed_width_is_refused_not_panicked() {
    refuse_wrong_width(TypeId::Uuid, 15);
}

/// The same widths, but the declared length is correct and the payload is cut
/// short. This exercises the other side of the invariant: the width check
/// passes and the bytes are missing.
#[test]
fn typed_values_truncated_below_their_declared_width_are_refused() {
    for (type_id, width) in [
        (TypeId::Int, 8usize),
        (TypeId::Float, 8),
        (TypeId::DateTime, 8),
        (TypeId::Uuid, 16),
    ] {
        let body = vec![0u8; width - 1];
        let payload = typed_value(type_id, width as u32, &body);
        must_refuse(
            &frame(MSG_RESULT_SCALAR_NATIVE, &payload),
            &format!("{type_id:?} truncated one byte below its declared width"),
        );
    }
}

/// The same fixed-width reads reached through the row decoder rather than the
/// scalar decoder, so a fast path that skips the scalar entry point is covered
/// too.
#[test]
fn typed_values_inside_native_rows_are_refused_not_panicked() {
    let mut payload = 1u16.to_le_bytes().to_vec(); // one column
    payload.extend_from_slice(&wire_string("id"));
    payload.extend_from_slice(&1u32.to_le_bytes()); // one row
    payload.extend_from_slice(&typed_value(TypeId::Uuid, 15, &[0u8; 15]));
    must_refuse(
        &frame(MSG_RESULT_ROWS_NATIVE, &payload),
        "a 15-byte UUID cell inside a native row",
    );
}

// ---------------------------------------------------------------------------
// frame_payload_len: the header read the connection read loop performs
// ---------------------------------------------------------------------------

#[test]
fn frame_payload_len_refuses_a_short_header() {
    for len in 0..6usize {
        assert_eq!(
            frame_payload_len(&vec![0xFFu8; len]),
            None,
            "a {len}-byte buffer is not a readable frame header"
        );
    }
}

#[test]
fn frame_payload_len_reads_a_complete_header() {
    let encoded = frame(
        MSG_RESULT_SCALAR_NATIVE,
        &typed_value(TypeId::Int, 8, &[0u8; 8]),
    );
    assert_eq!(frame_payload_len(&encoded), Some(13));
    // A header alone is enough: the payload need not have arrived yet, which
    // is exactly the state the read loop calls this in.
    assert_eq!(frame_payload_len(&encoded[..6]), Some(13));
}

#[test]
fn frame_payload_len_reads_the_maximum_declared_length() {
    // u32::MAX must come back as a number to be range-checked by the caller,
    // not saturate or wrap.
    let header = [0x03, 0x00, 0xFF, 0xFF, 0xFF, 0xFF];
    assert_eq!(frame_payload_len(&header), Some(u32::MAX));
}
