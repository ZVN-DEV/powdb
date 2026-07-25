//! Redaction of query literals for logging.
//!
//! Query text carries user data: `User filter .email = "ada@example.com"` puts
//! a real address in the log line, and a delete or update carries whatever the
//! application was operating on. Logs are shipped, retained, and read by people
//! who are not authorized to read the table, so the literal values must not
//! appear there.
//!
//! What operators actually need from a query log is the query *shape*: which
//! table, which operators, which columns, and a stable identifier that ties
//! repeated executions together. The engine already has that notion:
//! [`powdb_query::canonicalize::canonicalize`] hashes the token stream with
//! literal values replaced by placeholders, which is exactly the plan-cache
//! key. We log that hash alongside a literal-free rendering of the text, so a
//! logged query can be matched to a plan-cache entry and to other executions
//! of the same shape without ever printing a value.

/// Longest redacted query rendered into a log line. Long enough to see the
/// whole shape of a realistic query, short enough that a hostile client cannot
/// use the log as an amplification channel.
const MAX_LOGGED_QUERY_LEN: usize = 512;

/// Replace every literal value in `query` with a placeholder.
///
/// String literals become `"?"` / `'?'` (the quote style is preserved so the
/// shape still reads as PowQL) and numeric literals become `?`. Identifiers,
/// keywords, operators, and punctuation are kept verbatim. An unterminated
/// string redacts to the end of the input: when in doubt, drop the bytes.
///
/// This is deliberately a conservative lexical pass rather than a reuse of the
/// query lexer: it must never fail, must never allocate unboundedly, and must
/// stay correct for input that does not parse at all (which is exactly the
/// input a failure log is most likely to carry).
pub fn redact_query_literals(query: &str) -> String {
    let mut out = String::with_capacity(query.len().min(MAX_LOGGED_QUERY_LEN) + 8);
    let bytes = query.as_bytes();
    let mut i = 0;
    let mut prev_byte: Option<u8> = None;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' || b == b'\'' {
            out.push(b as char);
            out.push('?');
            out.push(b as char);
            i = skip_string(bytes, i);
            prev_byte = Some(b);
            continue;
        }
        if b.is_ascii_digit() && starts_a_number(prev_byte) {
            out.push('?');
            i = skip_number(bytes, i);
            prev_byte = Some(b')');
            continue;
        }
        // Multi-byte UTF-8 inside an identifier: copy the whole character so
        // the output stays valid UTF-8.
        let char_len = utf8_len(b);
        out.push_str(&query[i..i + char_len]);
        // Whitespace separates tokens: `limit 10` is a literal, `col2` is
        // part of an identifier.
        prev_byte = Some(if b.is_ascii_whitespace() { b' ' } else { b });
        i += char_len;
    }

    truncate_for_log(out)
}

/// A digit begins a literal unless it continues an identifier (`col2`) or a
/// field path (`.field2`). `prev` is the byte before the digit, with runs of
/// whitespace collapsed to a single space by the caller.
fn starts_a_number(prev: Option<u8>) -> bool {
    match prev {
        None => true,
        Some(p) => !(p.is_ascii_alphanumeric() || p == b'_' || p == b'.'),
    }
}

/// Index just past the string literal that starts at `open` (a quote byte).
/// Handles backslash escapes; an unterminated string consumes the rest.
fn skip_string(bytes: &[u8], open: usize) -> usize {
    let quote = bytes[open];
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b if b == quote => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// Index just past the numeric literal starting at `start`.
fn skip_number(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
        // A trailing `.` is a field access, not part of the number.
        if bytes[i] == b'.' && !bytes.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
            break;
        }
        i += 1;
    }
    i
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

fn truncate_for_log(mut s: String) -> String {
    if s.len() <= MAX_LOGGED_QUERY_LEN {
        return s;
    }
    let mut cut = MAX_LOGGED_QUERY_LEN;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push_str("...");
    s
}

/// Stable identifier for a query's *shape*, reusing the plan cache's
/// canonicalization so a logged query can be tied to its cached plan. `None`
/// when the text does not lex (in which case there is no shape to name).
pub fn query_shape_hash(query: &str) -> Option<u64> {
    powdb_query::canonicalize::canonicalize(query)
        .ok()
        .map(|(hash, _literals)| hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_literals_are_replaced_with_a_placeholder() {
        assert_eq!(
            redact_query_literals(r#"User filter .email = "ada@example.com""#),
            r#"User filter .email = "?""#
        );
        assert_eq!(
            redact_query_literals("User filter .name = 'Ada Lovelace'"),
            "User filter .name = '?'"
        );
    }

    #[test]
    fn numeric_literals_are_replaced_but_identifiers_are_kept() {
        assert_eq!(
            redact_query_literals("User filter .salary > 125000 limit 10"),
            "User filter .salary > ? limit ?"
        );
        assert_eq!(
            redact_query_literals("Table2 filter .col2 = 7"),
            "Table2 filter .col2 = ?"
        );
        assert_eq!(
            redact_query_literals("User filter .score = 3.5"),
            "User filter .score = ?"
        );
    }

    #[test]
    fn no_literal_value_survives_redaction() {
        let secret = "4111111111111111";
        let redacted = redact_query_literals(&format!(
            r#"Card filter .pan = "{secret}" or .pan = '{secret}' or .id = {secret}"#
        ));
        assert!(
            !redacted.contains(secret),
            "literal leaked into the log line: {redacted}"
        );
    }

    #[test]
    fn escaped_quotes_do_not_end_the_string_early() {
        let redacted = redact_query_literals(r#"User filter .bio = "say \"hi\" to ada" limit 1"#);
        assert_eq!(redacted, r#"User filter .bio = "?" limit ?"#);
        assert!(!redacted.contains("ada"));
    }

    #[test]
    fn unterminated_string_redacts_to_the_end() {
        let redacted = redact_query_literals(r#"User filter .email = "ada@example.com"#);
        assert!(!redacted.contains("ada"), "redacted: {redacted}");
    }

    #[test]
    fn long_queries_are_truncated() {
        let long = "User filter .name = x and .other = y ".repeat(64);
        let redacted = redact_query_literals(&long);
        assert!(redacted.len() <= MAX_LOGGED_QUERY_LEN + 3);
        assert!(redacted.ends_with("..."));
    }

    #[test]
    fn non_ascii_identifiers_stay_valid_utf8() {
        let redacted = redact_query_literals("Café filter .naïve = \"secret\"");
        assert_eq!(redacted, "Café filter .naïve = \"?\"");
    }

    #[test]
    fn shape_hash_matches_across_differing_literals() {
        let a = query_shape_hash("User filter .id = 1").expect("lexes");
        let b = query_shape_hash("User filter .id = 999").expect("lexes");
        assert_eq!(a, b, "the same shape must hash the same");
        let c = query_shape_hash("Order filter .id = 1").expect("lexes");
        assert_ne!(a, c, "a different shape must hash differently");
    }
}
