//! PJ1: PowDB JSON 1 — the engine-owned canonical binary encoding for the
//! `Json` value type (design doc 2026-07-13, section 4.2).
//!
//! PJ1 is a self-describing, order-defined binary format. It is hand-rolled and
//! carries no runtime dependency on serde_json (a dev-dependency parser backs
//! the differential tests only). The on-disk bytes are PERMANENT: this spec can
//! never change incompatibly, so it ships with fuzz, property, and model-based
//! total-order tests before the first persisted byte.
//!
//! # Layout
//!
//! Every node begins with a one-byte tag. The tag value selects the type:
//!
//! ```text
//! 0 null | 1 false | 2 true | 3 int | 4 float | 5 string | 6 array | 7 object
//! (8..=15 RESERVED for future scalar types: datetime, uuid, decimal, ...)
//! ```
//!
//! Any tag byte greater than 7 is rejected on decode (reserved or invalid).
//!
//! ```text
//! null/false/true : [tag]
//! int             : [tag][i64 LE]
//! float           : [tag][f64 LE]
//! string          : [tag][len u32 LE][UTF-8 bytes]
//! array           : [tag][count u32][elem_off u32 x (count+1)][elements]
//! object          : [tag][count u32][(key_off u32, val_off u32) x count][end u32]
//!                   [key data: (len u32 + bytes) x count, keys sorted bytewise]
//!                   [values]
//! ```
//!
//! ## Offset base
//!
//! All `elem_off`, `key_off`, `val_off` and `end` offsets are relative to the
//! START of the node they belong to (the byte holding that node's tag). This is
//! the load-bearing invariant that makes extraction zero-copy: a sub-slice that
//! begins exactly at a child node's tag is itself a valid, self-contained PJ1
//! document, so `pj1_get` returns a borrowed `&[u8]` that later lanes can walk
//! or compare directly with no re-basing.
//!
//! For an array of `count` elements, `elem_off[i]` is the start of element `i`
//! and `elem_off[count]` is the node's total length; element `i` occupies
//! `[elem_off[i], elem_off[i+1])`. For an object, `key_off[i]` points at key
//! `i`'s length prefix in the key-data region, `val_off[i]` at value `i`'s node,
//! and `end` is the node's total length (so value `i` spans `[val_off[i], next)`
//! where `next` is `val_off[i+1]` or, for the last value, `end`).
//!
//! # Canonicalization (permanent semantics)
//!
//! - Object keys are sorted bytewise; INSERTION ORDER IS NOT PRESERVED (JSONB
//!   precedent). Duplicate keys are last-wins, deduplicated at encode time.
//! - The int/float distinction is taken from the input text: `1` stays an int,
//!   `1.0` stays a float. A number with a fraction or exponent, or an integer
//!   too large for `i64`, is a float (`f64`).
//! - NaN and Infinity are rejected at parse (JSON has no such literals, and a
//!   number whose magnitude overflows `f64` to infinity is `NumberOutOfRange`).
//! - Canonical float text uses Rust's shortest round-tripping `{}`; a decimal
//!   point is appended when the shortest form has none, so a float always
//!   re-parses as a float (idempotent canonicalization).
//! - Equal documents therefore have EQUAL BYTES.
//!
//! Total order (used for min/max, index keys, and group keys):
//!
//! ```text
//! null < false < true < numbers < strings < arrays < objects
//! ```
//!
//! Numbers compare numerically across the int/float split; strings compare
//! bytewise; arrays compare lexicographically elementwise; objects compare by
//! their sorted `(key, value)` pairs.

use std::cmp::Ordering;
use std::fmt;

/// Maximum nesting depth accepted by [`parse_json_text`]. Documents deeper than
/// this are rejected with [`JsonError::TooDeep`], bounding parser stack use.
pub const MAX_DEPTH: usize = 128;

// Node tags (the low nibble of the tag byte; 8..=15 are reserved).
const TAG_NULL: u8 = 0;
const TAG_FALSE: u8 = 1;
const TAG_TRUE: u8 = 2;
const TAG_INT: u8 = 3;
const TAG_FLOAT: u8 = 4;
const TAG_STRING: u8 = 5;
const TAG_ARRAY: u8 = 6;
const TAG_OBJECT: u8 = 7;

/// A single step in a JSON path, used by [`pj1_get`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSeg<'a> {
    /// Object member access by key.
    Key(&'a str),
    /// Array element access by zero-based index.
    Index(u32),
}

/// Errors from PJ1 parsing, validation, and rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonError {
    /// Invalid JSON syntax or structure during text parsing.
    InvalidJson(String),
    /// A `\uXXXX` escape produced a lone/unpaired UTF-16 surrogate.
    InvalidUtf8,
    /// Nesting exceeded [`MAX_DEPTH`].
    TooDeep,
    /// A JSON number overflowed `f64` to infinity (e.g. `1e400`).
    NumberOutOfRange,
    /// PJ1 bytes ended before a node was fully read.
    Truncated,
    /// A tag byte in the reserved range (8..=15) or otherwise undefined.
    ReservedTag(u8),
    /// PJ1 bytes were structurally malformed (bad offsets, unsorted keys, ...).
    MalformedPj1(String),
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JsonError::InvalidJson(m) => write!(f, "invalid JSON: {m}"),
            JsonError::InvalidUtf8 => write!(f, "invalid JSON: lone UTF-16 surrogate"),
            JsonError::TooDeep => write!(f, "invalid JSON: nesting exceeds depth cap {MAX_DEPTH}"),
            JsonError::NumberOutOfRange => write!(f, "invalid JSON: number out of range"),
            JsonError::Truncated => write!(f, "malformed PJ1: truncated"),
            JsonError::ReservedTag(t) => write!(f, "malformed PJ1: reserved/invalid tag {t}"),
            JsonError::MalformedPj1(m) => write!(f, "malformed PJ1: {m}"),
        }
    }
}

impl std::error::Error for JsonError {}

// ─── intermediate tree (parse target, then canonically encoded) ──────────────

enum Node {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<Node>),
    /// Pairs are stored already sorted bytewise and deduplicated (last-wins).
    Object(Vec<(String, Node)>),
}

// ─── text -> canonical PJ1 ───────────────────────────────────────────────────

/// Parse JSON `text` into canonical PJ1 bytes.
///
/// Enforces a nesting depth cap of [`MAX_DEPTH`] and rejects invalid JSON,
/// lone surrogates, non-finite numbers, leading zeros, and trailing garbage.
pub fn parse_json_text(text: &str) -> Result<Vec<u8>, JsonError> {
    let mut p = Parser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let node = p.parse_value(0)?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(JsonError::InvalidJson("trailing characters".into()));
    }
    let mut out = Vec::new();
    encode_node(&node, &mut out);
    Ok(out)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            // JSON insignificant whitespace: space, tab, LF, CR.
            if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<Node, JsonError> {
        match self.peek() {
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') => Ok(Node::Str(self.parse_string()?)),
            Some(b't') => {
                self.expect_lit(b"true")?;
                Ok(Node::Bool(true))
            }
            Some(b'f') => {
                self.expect_lit(b"false")?;
                Ok(Node::Bool(false))
            }
            Some(b'n') => {
                self.expect_lit(b"null")?;
                Ok(Node::Null)
            }
            Some(c) if c == b'-' || c.is_ascii_digit() => self.parse_number(),
            Some(c) => Err(JsonError::InvalidJson(format!(
                "unexpected character '{}'",
                c as char
            ))),
            None => Err(JsonError::InvalidJson("unexpected end of input".into())),
        }
    }

    fn expect_lit(&mut self, lit: &[u8]) -> Result<(), JsonError> {
        if self.bytes[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(())
        } else {
            Err(JsonError::InvalidJson(format!(
                "expected `{}`",
                std::str::from_utf8(lit).unwrap_or("literal")
            )))
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<Node, JsonError> {
        if depth + 1 > MAX_DEPTH {
            return Err(JsonError::TooDeep);
        }
        self.pos += 1; // consume '['
        let mut elems = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Node::Array(elems));
        }
        loop {
            self.skip_ws();
            elems.push(self.parse_value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Node::Array(elems));
                }
                _ => return Err(JsonError::InvalidJson("expected ',' or ']'".into())),
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<Node, JsonError> {
        if depth + 1 > MAX_DEPTH {
            return Err(JsonError::TooDeep);
        }
        self.pos += 1; // consume '{'
        let mut pairs: Vec<(String, Node)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(canonical_object(pairs));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(JsonError::InvalidJson("expected object key string".into()));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(JsonError::InvalidJson(
                    "expected ':' after object key".into(),
                ));
            }
            self.pos += 1;
            self.skip_ws();
            let val = self.parse_value(depth + 1)?;
            pairs.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(canonical_object(pairs));
                }
                _ => return Err(JsonError::InvalidJson("expected ',' or '}'".into())),
            }
        }
    }

    fn parse_string(&mut self) -> Result<String, JsonError> {
        self.pos += 1; // consume opening quote
        let mut s = String::new();
        loop {
            let c = self
                .peek()
                .ok_or_else(|| JsonError::InvalidJson("unterminated string".into()))?;
            match c {
                b'"' => {
                    self.pos += 1;
                    return Ok(s);
                }
                b'\\' => {
                    self.pos += 1;
                    let esc = self
                        .peek()
                        .ok_or_else(|| JsonError::InvalidJson("unterminated escape".into()))?;
                    match esc {
                        b'"' => {
                            s.push('"');
                            self.pos += 1;
                        }
                        b'\\' => {
                            s.push('\\');
                            self.pos += 1;
                        }
                        b'/' => {
                            s.push('/');
                            self.pos += 1;
                        }
                        b'b' => {
                            s.push('\u{08}');
                            self.pos += 1;
                        }
                        b'f' => {
                            s.push('\u{0C}');
                            self.pos += 1;
                        }
                        b'n' => {
                            s.push('\n');
                            self.pos += 1;
                        }
                        b'r' => {
                            s.push('\r');
                            self.pos += 1;
                        }
                        b't' => {
                            s.push('\t');
                            self.pos += 1;
                        }
                        b'u' => {
                            self.pos += 1;
                            let hi = self.parse_hex4()?;
                            if (0xD800..=0xDBFF).contains(&hi) {
                                // High surrogate: must be followed by \uDC00..DFFF.
                                if self.peek() != Some(b'\\') {
                                    return Err(JsonError::InvalidUtf8);
                                }
                                self.pos += 1;
                                if self.peek() != Some(b'u') {
                                    return Err(JsonError::InvalidUtf8);
                                }
                                self.pos += 1;
                                let lo = self.parse_hex4()?;
                                if !(0xDC00..=0xDFFF).contains(&lo) {
                                    return Err(JsonError::InvalidUtf8);
                                }
                                let cp =
                                    0x10000 + (((hi - 0xD800) as u32) << 10) + (lo - 0xDC00) as u32;
                                s.push(char::from_u32(cp).ok_or(JsonError::InvalidUtf8)?);
                            } else if (0xDC00..=0xDFFF).contains(&hi) {
                                // Lone low surrogate.
                                return Err(JsonError::InvalidUtf8);
                            } else {
                                s.push(char::from_u32(hi as u32).ok_or(JsonError::InvalidUtf8)?);
                            }
                        }
                        _ => {
                            return Err(JsonError::InvalidJson(format!(
                                "invalid escape '\\{}'",
                                esc as char
                            )))
                        }
                    }
                }
                // Control characters must be escaped in JSON strings.
                0x00..=0x1F => {
                    return Err(JsonError::InvalidJson(
                        "unescaped control character in string".into(),
                    ))
                }
                // A raw byte >= 0x20: consume one UTF-8 code point. The input is
                // a &str, so byte boundaries are valid; find the char length.
                _ => {
                    let start = self.pos;
                    let len = utf8_char_len(c);
                    let end = start + len;
                    if end > self.bytes.len() {
                        return Err(JsonError::InvalidJson("truncated UTF-8".into()));
                    }
                    // Safe: input originated from a &str, so this is valid UTF-8.
                    s.push_str(
                        std::str::from_utf8(&self.bytes[start..end]).map_err(|_| {
                            JsonError::InvalidJson("invalid UTF-8 in string".into())
                        })?,
                    );
                    self.pos = end;
                }
            }
        }
    }

    fn parse_hex4(&mut self) -> Result<u16, JsonError> {
        let slice = self
            .bytes
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| JsonError::InvalidJson("truncated \\u escape".into()))?;
        let mut v: u16 = 0;
        for &b in slice {
            let d = (b as char)
                .to_digit(16)
                .ok_or_else(|| JsonError::InvalidJson("invalid \\u hex digit".into()))?;
            v = v * 16 + d as u16;
        }
        self.pos += 4;
        Ok(v)
    }

    fn parse_number(&mut self) -> Result<Node, JsonError> {
        let start = self.pos;
        let mut is_float = false;

        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // Integer part: '0' alone, or [1-9][0-9]* (no leading zeros).
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                // A digit immediately after a leading zero is invalid ("01").
                if matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    return Err(JsonError::InvalidJson("leading zero in number".into()));
                }
            }
            Some(c) if c.is_ascii_digit() => {
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
            _ => return Err(JsonError::InvalidJson("invalid number".into())),
        }
        // Fraction.
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                return Err(JsonError::InvalidJson("expected digit after '.'".into()));
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        // Exponent.
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                return Err(JsonError::InvalidJson("expected digit in exponent".into()));
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }

        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| JsonError::InvalidJson("invalid number".into()))?;

        if !is_float {
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Node::Int(i));
            }
            // Integer too large for i64: fall through to f64.
        }
        let f = text
            .parse::<f64>()
            .map_err(|_| JsonError::InvalidJson("invalid number".into()))?;
        if !f.is_finite() {
            return Err(JsonError::NumberOutOfRange);
        }
        Ok(Node::Float(f))
    }
}

/// Length in bytes of the UTF-8 code point whose lead byte is `b`.
fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// Build a canonical [`Node::Object`] from raw parsed pairs: sort bytewise by
/// key and drop duplicate keys keeping the last occurrence.
fn canonical_object(mut pairs: Vec<(String, Node)>) -> Node {
    // Stable sort keeps the original (insertion) order among equal keys, so the
    // last-inserted duplicate is the last element of each equal-key run.
    pairs.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut deduped: Vec<(String, Node)> = Vec::with_capacity(pairs.len());
    for pair in pairs {
        if let Some(last) = deduped.last_mut() {
            if last.0 == pair.0 {
                *last = pair; // last-wins
                continue;
            }
        }
        deduped.push(pair);
    }
    Node::Object(deduped)
}

fn encode_node(node: &Node, out: &mut Vec<u8>) {
    let start = out.len();
    match node {
        Node::Null => out.push(TAG_NULL),
        Node::Bool(false) => out.push(TAG_FALSE),
        Node::Bool(true) => out.push(TAG_TRUE),
        Node::Int(i) => {
            out.push(TAG_INT);
            out.extend_from_slice(&i.to_le_bytes());
        }
        Node::Float(f) => {
            out.push(TAG_FLOAT);
            out.extend_from_slice(&f.to_le_bytes());
        }
        Node::Str(s) => {
            out.push(TAG_STRING);
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Node::Array(elems) => {
            out.push(TAG_ARRAY);
            let count = elems.len();
            out.extend_from_slice(&(count as u32).to_le_bytes());
            let offtable_pos = out.len();
            out.resize(offtable_pos + 4 * (count + 1), 0);
            let mut offs = Vec::with_capacity(count + 1);
            for e in elems {
                offs.push((out.len() - start) as u32);
                encode_node(e, out);
            }
            offs.push((out.len() - start) as u32); // end sentinel
            for (i, off) in offs.iter().enumerate() {
                let p = offtable_pos + i * 4;
                out[p..p + 4].copy_from_slice(&off.to_le_bytes());
            }
        }
        Node::Object(pairs) => {
            out.push(TAG_OBJECT);
            let count = pairs.len();
            out.extend_from_slice(&(count as u32).to_le_bytes());
            let pairtable_pos = out.len();
            // count (key_off, val_off) pairs + one end u32.
            out.resize(pairtable_pos + count * 8 + 4, 0);
            let mut key_offs = Vec::with_capacity(count);
            for (k, _) in pairs {
                key_offs.push((out.len() - start) as u32);
                out.extend_from_slice(&(k.len() as u32).to_le_bytes());
                out.extend_from_slice(k.as_bytes());
            }
            let mut val_offs = Vec::with_capacity(count);
            for (_, v) in pairs {
                val_offs.push((out.len() - start) as u32);
                encode_node(v, out);
            }
            let end = (out.len() - start) as u32;
            for i in 0..count {
                let p = pairtable_pos + i * 8;
                out[p..p + 4].copy_from_slice(&key_offs[i].to_le_bytes());
                out[p + 4..p + 8].copy_from_slice(&val_offs[i].to_le_bytes());
            }
            let ep = pairtable_pos + count * 8;
            out[ep..ep + 4].copy_from_slice(&end.to_le_bytes());
        }
    }
}

// ─── little helpers over raw PJ1 bytes ───────────────────────────────────────

#[inline]
fn read_u32(buf: &[u8], at: usize) -> Option<u32> {
    buf.get(at..at + 4)
        .map(|s| u32::from_le_bytes(s.try_into().expect("4-byte slice")))
}

#[inline]
fn read_i64(buf: &[u8], at: usize) -> Option<i64> {
    buf.get(at..at + 8)
        .map(|s| i64::from_le_bytes(s.try_into().expect("8-byte slice")))
}

#[inline]
fn read_f64(buf: &[u8], at: usize) -> Option<f64> {
    buf.get(at..at + 8)
        .map(|s| f64::from_le_bytes(s.try_into().expect("8-byte slice")))
}

// ─── canonical PJ1 -> text ───────────────────────────────────────────────────

/// Render canonical PJ1 bytes as canonical JSON text. Round-trips through
/// [`parse_json_text`] to identical bytes.
pub fn pj1_to_text(doc: &[u8]) -> Result<String, JsonError> {
    let mut out = String::new();
    let end = write_text(doc, 0, &mut out)?;
    if end != doc.len() {
        return Err(JsonError::MalformedPj1("trailing bytes".into()));
    }
    Ok(out)
}

/// Write the node beginning at `start` as JSON text; returns the node's end
/// offset (exclusive).
fn write_text(buf: &[u8], start: usize, out: &mut String) -> Result<usize, JsonError> {
    let tag = *buf.get(start).ok_or(JsonError::Truncated)?;
    match tag {
        TAG_NULL => {
            out.push_str("null");
            Ok(start + 1)
        }
        TAG_FALSE => {
            out.push_str("false");
            Ok(start + 1)
        }
        TAG_TRUE => {
            out.push_str("true");
            Ok(start + 1)
        }
        TAG_INT => {
            let v = read_i64(buf, start + 1).ok_or(JsonError::Truncated)?;
            out.push_str(&v.to_string());
            Ok(start + 9)
        }
        TAG_FLOAT => {
            let v = read_f64(buf, start + 1).ok_or(JsonError::Truncated)?;
            // Canonical PJ1 never stores a non-finite float (parse rejects
            // NaN/Infinity); guard so a corrupt blob errors rather than
            // rendering the non-JSON literal `NaN`/`inf`.
            if !v.is_finite() {
                return Err(JsonError::MalformedPj1("non-finite float".into()));
            }
            out.push_str(&render_float(v));
            Ok(start + 9)
        }
        TAG_STRING => {
            let (s, end) = read_str(buf, start)?;
            write_json_string(s, out);
            Ok(end)
        }
        TAG_ARRAY => {
            let count = read_u32(buf, start + 1).ok_or(JsonError::Truncated)? as usize;
            let offtable = start + 5;
            out.push('[');
            for i in 0..count {
                if i > 0 {
                    out.push(',');
                }
                let off = read_u32(buf, offtable + i * 4).ok_or(JsonError::Truncated)? as usize;
                write_text(buf, start + off, out)?;
            }
            out.push(']');
            let end_off = read_u32(buf, offtable + count * 4).ok_or(JsonError::Truncated)? as usize;
            Ok(start + end_off)
        }
        TAG_OBJECT => {
            let count = read_u32(buf, start + 1).ok_or(JsonError::Truncated)? as usize;
            let pairtable = start + 5;
            out.push('{');
            for i in 0..count {
                if i > 0 {
                    out.push(',');
                }
                let key_off =
                    read_u32(buf, pairtable + i * 8).ok_or(JsonError::Truncated)? as usize;
                let val_off =
                    read_u32(buf, pairtable + i * 8 + 4).ok_or(JsonError::Truncated)? as usize;
                let (key, _) = read_str_len(buf, start + key_off)?;
                write_json_string(key, out);
                out.push(':');
                write_text(buf, start + val_off, out)?;
            }
            out.push('}');
            let end_off =
                read_u32(buf, pairtable + count * 8).ok_or(JsonError::Truncated)? as usize;
            Ok(start + end_off)
        }
        8..=15 => Err(JsonError::ReservedTag(tag)),
        _ => Err(JsonError::ReservedTag(tag)),
    }
}

/// Canonical shortest float text with a guaranteed fractional/exponent marker,
/// so the value re-parses as a float (never as an int).
fn render_float(v: f64) -> String {
    let s = format!("{v}");
    if s.bytes().any(|b| b == b'.' || b == b'e' || b == b'E') {
        s
    } else {
        format!("{s}.0")
    }
}

/// Read a PJ1 string node's `&str` payload and the node's end offset.
fn read_str(buf: &[u8], start: usize) -> Result<(&str, usize), JsonError> {
    read_str_len(buf, start + 1)
}

/// Read a length-prefixed UTF-8 blob at `at` (`len u32` + bytes); returns the
/// string and the offset just past it. Used for both string nodes (after the
/// tag) and object keys.
fn read_str_len(buf: &[u8], at: usize) -> Result<(&str, usize), JsonError> {
    let len = read_u32(buf, at).ok_or(JsonError::Truncated)? as usize;
    let data_start = at + 4;
    let slice = buf
        .get(data_start..data_start + len)
        .ok_or(JsonError::Truncated)?;
    let s = std::str::from_utf8(slice)
        .map_err(|_| JsonError::MalformedPj1("non-UTF-8 string".into()))?;
    Ok((s, data_start + len))
}

/// Append `s` to `out` as a JSON string literal with the minimal required
/// escaping.
fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ─── zero-alloc path extraction ──────────────────────────────────────────────

/// Extract the child node addressed by `seg` from the top-level node of `doc`,
/// returning a borrowed sub-slice that is itself a valid standalone PJ1
/// document (offsets are node-relative, so no re-basing is needed).
///
/// Returns `None` when the segment does not apply (key missing, index out of
/// range, or the node is not an object/array). Zero allocation; object lookups
/// are a binary search over the sorted key directory.
pub fn pj1_get<'a>(doc: &'a [u8], seg: &PathSeg) -> Option<&'a [u8]> {
    let tag = *doc.first()?;
    match (tag, seg) {
        (TAG_ARRAY, PathSeg::Index(idx)) => {
            let count = read_u32(doc, 1)? as usize;
            let idx = *idx as usize;
            if idx >= count {
                return None;
            }
            let offtable = 5;
            let o0 = read_u32(doc, offtable + idx * 4)? as usize;
            let o1 = read_u32(doc, offtable + (idx + 1) * 4)? as usize;
            doc.get(o0..o1)
        }
        (TAG_OBJECT, PathSeg::Key(key)) => {
            let count = read_u32(doc, 1)? as usize;
            let pairtable = 5;
            let target = key.as_bytes();
            let (mut lo, mut hi) = (0usize, count);
            while lo < hi {
                let mid = (lo + hi) / 2;
                let key_off = read_u32(doc, pairtable + mid * 8)? as usize;
                let (k, _) = read_str_len(doc, key_off).ok()?;
                match k.as_bytes().cmp(target) {
                    Ordering::Less => lo = mid + 1,
                    Ordering::Greater => hi = mid,
                    Ordering::Equal => {
                        let val_off = read_u32(doc, pairtable + mid * 8 + 4)? as usize;
                        let next = if mid + 1 < count {
                            read_u32(doc, pairtable + (mid + 1) * 8 + 4)? as usize
                        } else {
                            read_u32(doc, pairtable + count * 8)? as usize // end
                        };
                        return doc.get(val_off..next);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

// ─── total order over raw PJ1 bytes ──────────────────────────────────────────

/// Rank of a node in the type ladder (int and float share the "number" rank so
/// they can compare numerically): null < false < true < number < string <
/// array < object.
fn type_rank(tag: u8) -> u8 {
    match tag {
        TAG_NULL => 0,
        TAG_FALSE => 1,
        TAG_TRUE => 2,
        TAG_INT | TAG_FLOAT => 3,
        TAG_STRING => 4,
        TAG_ARRAY => 5,
        TAG_OBJECT => 6,
        _ => 7,
    }
}

/// Total order over two canonical PJ1 documents (design 4.2). Numbers compare
/// numerically across the int/float split; strings bytewise; arrays
/// lexicographically; objects by their sorted `(key, value)` pairs.
pub fn pj1_cmp(a: &[u8], b: &[u8]) -> Ordering {
    cmp_node(a, 0, b, 0).0
}

/// Compare the nodes at `(a, ai)` and `(b, bi)`; returns the ordering plus the
/// two node end offsets so array/object walks can advance.
fn cmp_node(a: &[u8], ai: usize, b: &[u8], bi: usize) -> (Ordering, usize, usize) {
    let ta = a.get(ai).copied().unwrap_or(255);
    let tb = b.get(bi).copied().unwrap_or(255);
    let (ra, rb) = (type_rank(ta), type_rank(tb));
    if ra != rb {
        // Different type ranks: order by rank; end offsets are not needed by a
        // caller once a decision is reached, but we still return best-effort
        // ends so a parent walk stays in sync on the equal branches only.
        return (ra.cmp(&rb), node_end(a, ai), node_end(b, bi));
    }
    match ra {
        3 => {
            // Numbers: compare numerically across int/float.
            let na = read_number(a, ai);
            let nb = read_number(b, bi);
            (cmp_numeric(na, nb), ai + 9, bi + 9)
        }
        4 => {
            let (sa, ea) = read_str(a, ai).unwrap_or(("", ai + 1));
            let (sb, eb) = read_str(b, bi).unwrap_or(("", bi + 1));
            (sa.as_bytes().cmp(sb.as_bytes()), ea, eb)
        }
        5 => (cmp_array(a, ai, b, bi), node_end(a, ai), node_end(b, bi)),
        6 => (cmp_object(a, ai, b, bi), node_end(a, ai), node_end(b, bi)),
        // Scalars null/false/true carry no payload: equal within the rank.
        _ => (Ordering::Equal, ai + 1, bi + 1),
    }
}

/// A number extracted for comparison: exactly one of int or float.
enum Num {
    Int(i64),
    Float(f64),
}

fn read_number(buf: &[u8], at: usize) -> Num {
    match buf.get(at).copied() {
        Some(TAG_INT) => Num::Int(read_i64(buf, at + 1).unwrap_or(0)),
        _ => Num::Float(read_f64(buf, at + 1).unwrap_or(0.0)),
    }
}

/// Numeric comparison mirroring `Value::cmp`: int/int by i64, float/float by
/// `total_cmp`, and cross-type by promoting the int to f64.
fn cmp_numeric(a: Num, b: Num) -> Ordering {
    match (a, b) {
        (Num::Int(x), Num::Int(y)) => x.cmp(&y),
        (Num::Float(x), Num::Float(y)) => x.total_cmp(&y),
        // Cross-type comparisons promote to f64 and, when numerically tied,
        // tie-break by tag (int before float). Without the tie-break,
        // pj1_cmp(1, 1.0) == Equal while their canonical bytes differ, which
        // breaks the Ord/Eq contract on Value::Json: order/min/max would call
        // documents equal that group/distinct/= call distinct. The tie-break
        // makes pj1_cmp a true total order under byte identity: Equal iff
        // canonical bytes are equal. Lexicographic (promoted f64, tag, exact
        // value) is transitive; the f64 promotion precision cliff at 2^53 is
        // documented and shared with engine-wide Value::cmp.
        (Num::Int(x), Num::Float(y)) => (x as f64).total_cmp(&y).then(Ordering::Less),
        (Num::Float(x), Num::Int(y)) => x.total_cmp(&(y as f64)).then(Ordering::Greater),
    }
}

fn cmp_array(a: &[u8], ai: usize, b: &[u8], bi: usize) -> Ordering {
    let ca = read_u32(a, ai + 1).unwrap_or(0) as usize;
    let cb = read_u32(b, bi + 1).unwrap_or(0) as usize;
    let ta = ai + 5;
    let tb = bi + 5;
    for i in 0..ca.min(cb) {
        let oa = ai + read_u32(a, ta + i * 4).unwrap_or(0) as usize;
        let ob = bi + read_u32(b, tb + i * 4).unwrap_or(0) as usize;
        let (ord, _, _) = cmp_node(a, oa, b, ob);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    ca.cmp(&cb)
}

fn cmp_object(a: &[u8], ai: usize, b: &[u8], bi: usize) -> Ordering {
    let ca = read_u32(a, ai + 1).unwrap_or(0) as usize;
    let cb = read_u32(b, bi + 1).unwrap_or(0) as usize;
    let pa = ai + 5;
    let pb = bi + 5;
    for i in 0..ca.min(cb) {
        // Keys first (sorted, so pairwise comparison is well defined).
        let ka_off = ai + read_u32(a, pa + i * 8).unwrap_or(0) as usize;
        let kb_off = bi + read_u32(b, pb + i * 8).unwrap_or(0) as usize;
        let (ka, _) = read_str_len(a, ka_off).unwrap_or(("", 0));
        let (kb, _) = read_str_len(b, kb_off).unwrap_or(("", 0));
        match ka.as_bytes().cmp(kb.as_bytes()) {
            Ordering::Equal => {}
            other => return other,
        }
        let va_off = ai + read_u32(a, pa + i * 8 + 4).unwrap_or(0) as usize;
        let vb_off = bi + read_u32(b, pb + i * 8 + 4).unwrap_or(0) as usize;
        let (ord, _, _) = cmp_node(a, va_off, b, vb_off);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    ca.cmp(&cb)
}

/// Best-effort end offset of the node at `start` (used only on equal-rank walks
/// where the caller does not actually consume it after a decision).
fn node_end(buf: &[u8], start: usize) -> usize {
    match buf.get(start).copied() {
        Some(TAG_NULL) | Some(TAG_FALSE) | Some(TAG_TRUE) => start + 1,
        Some(TAG_INT) | Some(TAG_FLOAT) => start + 9,
        Some(TAG_STRING) => read_str(buf, start).map(|(_, e)| e).unwrap_or(start + 1),
        Some(TAG_ARRAY) => {
            let count = read_u32(buf, start + 1).unwrap_or(0) as usize;
            start + read_u32(buf, start + 5 + count * 4).unwrap_or(0) as usize
        }
        Some(TAG_OBJECT) => {
            let count = read_u32(buf, start + 1).unwrap_or(0) as usize;
            start + read_u32(buf, start + 5 + count * 8).unwrap_or(0) as usize
        }
        _ => start + 1,
    }
}

// ─── structural validation of untrusted bytes ────────────────────────────────

/// Validate that `doc` is a single well-formed canonical PJ1 document. This is
/// the guard for bytes of unknown provenance (e.g. a corrupted page or a fuzz
/// input): it never panics and never reads out of bounds, rejecting anything
/// malformed with a typed error instead.
pub fn pj1_validate(doc: &[u8]) -> Result<(), JsonError> {
    let end = validate_node(doc, 0, 0)?;
    if end != doc.len() {
        return Err(JsonError::MalformedPj1("trailing bytes".into()));
    }
    Ok(())
}

fn validate_node(buf: &[u8], start: usize, depth: usize) -> Result<usize, JsonError> {
    if depth > MAX_DEPTH {
        return Err(JsonError::TooDeep);
    }
    let tag = *buf.get(start).ok_or(JsonError::Truncated)?;
    match tag {
        TAG_NULL | TAG_FALSE | TAG_TRUE => Ok(start + 1),
        TAG_INT => {
            buf.get(start + 1..start + 9).ok_or(JsonError::Truncated)?;
            Ok(start + 9)
        }
        TAG_FLOAT => {
            let v = read_f64(buf, start + 1).ok_or(JsonError::Truncated)?;
            // Non-finite floats are non-canonical (see write_text); reject them
            // so "validated => renderable => re-parseable" holds.
            if !v.is_finite() {
                return Err(JsonError::MalformedPj1("non-finite float".into()));
            }
            Ok(start + 9)
        }
        TAG_STRING => {
            let (_, end) = read_str_checked(buf, start + 1)?;
            Ok(end)
        }
        TAG_ARRAY => validate_array(buf, start, depth),
        TAG_OBJECT => validate_object(buf, start, depth),
        8..=15 => Err(JsonError::ReservedTag(tag)),
        _ => Err(JsonError::ReservedTag(tag)),
    }
}

fn validate_array(buf: &[u8], start: usize, depth: usize) -> Result<usize, JsonError> {
    let count = read_u32(buf, start + 1).ok_or(JsonError::Truncated)? as usize;
    let offtable = start + 5;
    // Offset table must be present: count+1 u32s.
    let table_end = offtable
        .checked_add(4usize.checked_mul(count + 1).ok_or(overflow())?)
        .ok_or(overflow())?;
    buf.get(offtable..table_end).ok_or(JsonError::Truncated)?;

    let header_size = 5 + 4 * (count + 1);
    let first = read_u32(buf, offtable).ok_or(JsonError::Truncated)? as usize;
    if first != header_size {
        return Err(JsonError::MalformedPj1(
            "array first offset != header".into(),
        ));
    }
    let mut prev = first;
    for i in 0..count {
        let o0 = read_u32(buf, offtable + i * 4).ok_or(JsonError::Truncated)? as usize;
        let o1 = read_u32(buf, offtable + (i + 1) * 4).ok_or(JsonError::Truncated)? as usize;
        if o0 != prev || o1 < o0 {
            return Err(JsonError::MalformedPj1(
                "array offsets not monotonic".into(),
            ));
        }
        let child_end = validate_node(buf, start + o0, depth + 1)?;
        if child_end != start + o1 {
            return Err(JsonError::MalformedPj1(
                "array element length mismatch".into(),
            ));
        }
        prev = o1;
    }
    let total = read_u32(buf, offtable + count * 4).ok_or(JsonError::Truncated)? as usize;
    if total != prev {
        return Err(JsonError::MalformedPj1("array end offset mismatch".into()));
    }
    Ok(start + total)
}

fn validate_object(buf: &[u8], start: usize, depth: usize) -> Result<usize, JsonError> {
    let count = read_u32(buf, start + 1).ok_or(JsonError::Truncated)? as usize;
    let pairtable = start + 5;
    let pairs_bytes = 8usize.checked_mul(count).ok_or(overflow())?;
    let table_end = pairtable
        .checked_add(pairs_bytes)
        .and_then(|v| v.checked_add(4))
        .ok_or(overflow())?;
    buf.get(pairtable..table_end).ok_or(JsonError::Truncated)?;

    let header_size = 5 + count * 8 + 4;
    // Keys occupy a contiguous region right after the header, sorted and
    // strictly increasing (canonical dedup). Validate them in order.
    let mut expected_key_off = header_size;
    let mut prev_key: Option<&[u8]> = None;
    for i in 0..count {
        let key_off = read_u32(buf, pairtable + i * 8).ok_or(JsonError::Truncated)? as usize;
        if key_off != expected_key_off {
            return Err(JsonError::MalformedPj1("object key offset mismatch".into()));
        }
        let (k, kend) = read_str_checked(buf, start + key_off)?;
        if let Some(pk) = prev_key {
            if k <= pk {
                return Err(JsonError::MalformedPj1(
                    "object keys not strictly sorted".into(),
                ));
            }
        }
        prev_key = Some(k);
        expected_key_off = kend - start;
    }
    // Values follow the key region; val_off strictly increasing, each a node.
    let mut expected_val_off = expected_key_off;
    for i in 0..count {
        let val_off = read_u32(buf, pairtable + i * 8 + 4).ok_or(JsonError::Truncated)? as usize;
        if val_off != expected_val_off {
            return Err(JsonError::MalformedPj1(
                "object value offset mismatch".into(),
            ));
        }
        let vend = validate_node(buf, start + val_off, depth + 1)?;
        expected_val_off = vend - start;
    }
    let end = read_u32(buf, pairtable + count * 8).ok_or(JsonError::Truncated)? as usize;
    if end != expected_val_off {
        return Err(JsonError::MalformedPj1("object end offset mismatch".into()));
    }
    Ok(start + end)
}

/// Bounds-checked read of a length-prefixed UTF-8 blob at `at`; returns the
/// `&[u8]` payload and the offset just past it. Used by validation.
fn read_str_checked(buf: &[u8], at: usize) -> Result<(&[u8], usize), JsonError> {
    let len = read_u32(buf, at).ok_or(JsonError::Truncated)? as usize;
    let data_start = at + 4;
    let end = data_start.checked_add(len).ok_or(overflow())?;
    let slice = buf.get(data_start..end).ok_or(JsonError::Truncated)?;
    // Keys and string payloads must be valid UTF-8 to be canonical.
    std::str::from_utf8(slice).map_err(|_| JsonError::MalformedPj1("non-UTF-8 string".into()))?;
    Ok((slice, end))
}

fn overflow() -> JsonError {
    JsonError::MalformedPj1("length overflow".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(text: &str) -> String {
        let bytes = parse_json_text(text).expect("parse");
        pj1_validate(&bytes).expect("validate");
        pj1_to_text(&bytes).expect("to_text")
    }

    #[test]
    fn scalars_roundtrip() {
        assert_eq!(roundtrip("null"), "null");
        assert_eq!(roundtrip("true"), "true");
        assert_eq!(roundtrip("false"), "false");
        assert_eq!(roundtrip("0"), "0");
        assert_eq!(roundtrip("-42"), "-42");
        assert_eq!(roundtrip("\"hi\""), "\"hi\"");
    }

    #[test]
    fn int_vs_float_preserved() {
        // 1 stays an int, 1.0 stays a float, and both re-render distinctly.
        assert_eq!(roundtrip("1"), "1");
        assert_eq!(roundtrip("1.0"), "1.0");
        assert_eq!(roundtrip("100"), "100");
        assert_eq!(roundtrip("100.0"), "100.0");
        assert_eq!(roundtrip("1e2"), "100.0"); // exponent => float
        assert_eq!(roundtrip("-0"), "0"); // -0 integer canonicalizes to 0
        assert_eq!(roundtrip("-0.0"), "-0.0"); // -0.0 float keeps its sign
        assert_eq!(roundtrip("1.5"), "1.5");
    }

    #[test]
    fn big_integer_becomes_float() {
        // Beyond i64 range: parses as f64 and renders with a float marker.
        let t = roundtrip("99999999999999999999999999");
        assert!(t.contains('.') || t.contains('e'), "got {t}");
    }

    #[test]
    fn object_keys_sorted_and_deduped() {
        // Insertion order not preserved; keys come out sorted bytewise.
        assert_eq!(roundtrip("{\"b\":1,\"a\":2}"), "{\"a\":2,\"b\":1}");
        // Duplicate keys: last wins.
        assert_eq!(roundtrip("{\"a\":1,\"a\":2}"), "{\"a\":2}");
        assert_eq!(roundtrip("{}"), "{}");
    }

    #[test]
    fn equal_documents_equal_bytes() {
        // Same logical document, different key order and duplicate => same bytes.
        let a = parse_json_text("{\"x\":1,\"y\":[1,2,3]}").unwrap();
        let b = parse_json_text("{\"y\":[1,2,3],\"x\":1}").unwrap();
        assert_eq!(a, b);
        let c = parse_json_text("{\"x\":9,\"x\":1,\"y\":[1,2,3]}").unwrap();
        assert_eq!(a, c);
    }

    #[test]
    fn nested_roundtrip() {
        let t = "{\"a\":[1,{\"b\":null,\"c\":\"x\"},true],\"z\":1.5}";
        // Keys already sorted here; nested object keys sorted too.
        assert_eq!(roundtrip(t), t);
    }

    #[test]
    fn rejects_bad_json() {
        assert!(parse_json_text("01").is_err()); // leading zero
        assert!(parse_json_text("1.").is_err());
        assert!(parse_json_text("").is_err());
        assert!(parse_json_text("nul").is_err());
        assert!(parse_json_text("{\"a\":1,}").is_err()); // trailing comma
        assert!(parse_json_text("[1,2").is_err()); // unterminated
        assert!(parse_json_text("1 2").is_err()); // trailing garbage
        assert!(parse_json_text("NaN").is_err());
        assert!(parse_json_text("Infinity").is_err());
        assert_eq!(parse_json_text("1e400"), Err(JsonError::NumberOutOfRange));
    }

    #[test]
    fn rejects_lone_surrogate() {
        assert_eq!(parse_json_text("\"\\uD800\""), Err(JsonError::InvalidUtf8));
        assert_eq!(parse_json_text("\"\\uDC00\""), Err(JsonError::InvalidUtf8));
        // Valid surrogate pair (emoji) accepted.
        assert!(parse_json_text("\"\\uD83D\\uDE00\"").is_ok());
    }

    #[test]
    fn depth_cap_enforced() {
        let deep = format!("{}1{}", "[".repeat(200), "]".repeat(200));
        assert_eq!(parse_json_text(&deep), Err(JsonError::TooDeep));
        // Just at the cap is fine.
        let ok = format!("{}1{}", "[".repeat(120), "]".repeat(120));
        assert!(parse_json_text(&ok).is_ok());
    }

    #[test]
    fn get_walks() {
        let doc = parse_json_text("{\"a\":{\"b\":[10,20,30]},\"n\":null}").unwrap();
        let a = pj1_get(&doc, &PathSeg::Key("a")).unwrap();
        let b = pj1_get(a, &PathSeg::Key("b")).unwrap();
        let e1 = pj1_get(b, &PathSeg::Index(1)).unwrap();
        assert_eq!(pj1_to_text(e1).unwrap(), "20");
        // Miss + out of range.
        assert!(pj1_get(&doc, &PathSeg::Key("missing")).is_none());
        assert!(pj1_get(b, &PathSeg::Index(9)).is_none());
        // Wrong segment kind for node type.
        assert!(pj1_get(&doc, &PathSeg::Index(0)).is_none());
        assert!(pj1_get(b, &PathSeg::Key("x")).is_none());
    }

    #[test]
    fn get_weird_keys() {
        let doc = parse_json_text("{\"\":1,\"a b\":2,\"\\u00e9\":3}").unwrap();
        assert_eq!(
            pj1_to_text(pj1_get(&doc, &PathSeg::Key("")).unwrap()).unwrap(),
            "1"
        );
        assert_eq!(
            pj1_to_text(pj1_get(&doc, &PathSeg::Key("a b")).unwrap()).unwrap(),
            "2"
        );
        assert_eq!(
            pj1_to_text(pj1_get(&doc, &PathSeg::Key("é")).unwrap()).unwrap(),
            "3"
        );
    }

    #[test]
    fn validate_rejects_non_finite_float() {
        // A float node whose 8 payload bytes decode to NaN is non-canonical
        // (the parser never emits one). Both validate and to_text must reject
        // it rather than render the non-JSON literal `NaN`. Regression for a
        // fuzz-found crash: [4, 7, 0xFF x7].
        let nan = [TAG_FLOAT, 7, 255, 255, 255, 255, 255, 255, 255];
        assert!(pj1_validate(&nan).is_err());
        assert!(pj1_to_text(&nan).is_err());
        // Infinity too.
        let mut inf = vec![TAG_FLOAT];
        inf.extend_from_slice(&f64::INFINITY.to_le_bytes());
        assert!(pj1_validate(&inf).is_err());
        assert!(pj1_to_text(&inf).is_err());
    }

    #[test]
    fn total_order_type_ladder() {
        let vals = [
            "null",
            "false",
            "true",
            "1",
            "1.5",
            "2",
            "\"a\"",
            "\"b\"",
            "[1]",
            "[2]",
            "{\"a\":1}",
        ];
        let encoded: Vec<Vec<u8>> = vals.iter().map(|v| parse_json_text(v).unwrap()).collect();
        // null < false < true < numbers < strings < arrays < objects.
        for i in 0..encoded.len() {
            for j in 0..encoded.len() {
                let ord = pj1_cmp(&encoded[i], &encoded[j]);
                assert_eq!(ord, i.cmp(&j).then(ord), "index {i} vs {j}");
            }
        }
    }

    #[test]
    fn total_order_numeric_across_types() {
        let a = parse_json_text("1").unwrap();
        let b = parse_json_text("1.0").unwrap();
        // Numerically tied across int/float, so the tag tie-break decides:
        // int before float. Equal is reserved for byte-equal documents so the
        // Ord/Eq contract on Value::Json holds.
        assert_eq!(pj1_cmp(&a, &b), Ordering::Less);
        assert_eq!(pj1_cmp(&b, &a), Ordering::Greater);
        let c = parse_json_text("2").unwrap();
        assert_eq!(pj1_cmp(&b, &c), Ordering::Less);
        // Byte-equal documents are the only Equal.
        assert_eq!(pj1_cmp(&a, &a), Ordering::Equal);
    }
}
