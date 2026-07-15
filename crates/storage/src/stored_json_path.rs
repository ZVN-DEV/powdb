//! Versioned, table-local identity for persisted JSON expression paths.
//!
//! Catalog/index ownership is deliberately outside this module. This type is
//! only the stable byte/text contract those layers can store once expression
//! indexes are wired: the table is already known by the owning catalog entry,
//! so identity starts at a column and preserves key-vs-index structure.

/// Version tag at the start of every [`StoredJsonPathV1::canonical_bytes`].
pub const STORED_JSON_PATH_VERSION_V1: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StoredJsonPathSegmentV1 {
    Key(String),
    Index(u32),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StoredJsonPathV1 {
    pub column: String,
    pub segments: Vec<StoredJsonPathSegmentV1>,
}

impl StoredJsonPathV1 {
    pub fn new(column: impl Into<String>, segments: Vec<StoredJsonPathSegmentV1>) -> Self {
        Self {
            column: column.into(),
            segments,
        }
    }

    /// Stable binary identity. Lengths are little-endian u32 values; callers
    /// must treat this format as versioned data rather than Rust serialization.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(STORED_JSON_PATH_VERSION_V1);
        push_str(&mut out, &self.column);
        push_u32(&mut out, self.segments.len() as u32);
        for segment in &self.segments {
            match segment {
                StoredJsonPathSegmentV1::Key(key) => {
                    out.push(1);
                    push_str(&mut out, key);
                }
                StoredJsonPathSegmentV1::Index(index) => {
                    out.push(2);
                    push_u32(&mut out, *index);
                }
            }
        }
        out
    }

    /// Stable diagnostic form. Keys are always quoted, making the equivalence
    /// of source spellings (`->author` and `->"author"`) explicit while keeping
    /// object key `"0"` distinct from array index `0`.
    pub fn canonical_text(&self) -> String {
        let mut out = format!("v1:.{}", self.column);
        for segment in &self.segments {
            match segment {
                StoredJsonPathSegmentV1::Key(key) => {
                    out.push_str("->\"");
                    push_escaped(&mut out, key);
                    out.push('"');
                }
                StoredJsonPathSegmentV1::Index(index) => {
                    out.push_str("->");
                    out.push_str(&index.to_string());
                }
            }
        }
        out
    }
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_str(out: &mut Vec<u8>, value: &str) {
    push_u32(out, value.len() as u32);
    out.extend_from_slice(value.as_bytes());
}

fn push_escaped(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c <= '\u{1f}' => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_identity_distinguishes_key_from_index() {
        let key = StoredJsonPathV1::new("data", vec![StoredJsonPathSegmentV1::Key("0".into())]);
        let index = StoredJsonPathV1::new("data", vec![StoredJsonPathSegmentV1::Index(0)]);
        assert_ne!(key.canonical_bytes(), index.canonical_bytes());
        assert_eq!(key.canonical_text(), "v1:.data->\"0\"");
        assert_eq!(index.canonical_text(), "v1:.data->0");
    }

    #[test]
    fn canonical_unicode_and_escaping_golden() {
        let path = StoredJsonPathV1::new(
            "payload",
            vec![StoredJsonPathSegmentV1::Key("café\n\"x\\y".into())],
        );
        assert_eq!(path.canonical_text(), "v1:.payload->\"café\\n\\\"x\\\\y\"");
        assert_eq!(
            path.canonical_bytes(),
            vec![
                1, 7, 0, 0, 0, b'p', b'a', b'y', b'l', b'o', b'a', b'd', 1, 0, 0, 0, 1, 10, 0, 0,
                0, b'c', b'a', b'f', 0xc3, 0xa9, b'\n', b'"', b'x', b'\\', b'y',
            ]
        );
    }
}
