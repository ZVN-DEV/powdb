//! PJ1 codec correctness: the spec demands these before the first persisted
//! byte (design 2026-07-13, section 9). Four families:
//!
//! 1. Canonicalization idempotency: text -> PJ1 -> text -> PJ1 is a fixpoint,
//!    and logically equal documents (key order / duplicates) have equal bytes.
//! 2. Differential parse acceptance/rejection vs serde_json (a dev-only oracle).
//! 3. Model-based total order: `pj1_cmp` is reflexive, antisymmetric, and
//!    transitive over a generated corpus, and consistent with the type ladder.
//! 4. Zero-alloc path walk (`pj1_get`) hits, misses, and edge keys.

use powdb_storage::pj1::{parse_json_text, pj1_cmp, pj1_get, pj1_to_text, pj1_validate, PathSeg};
use proptest::prelude::*;
use std::cmp::Ordering;

// ─── a small recursive JSON-text generator ───────────────────────────────────

/// Generate syntactically valid JSON text (bounded depth) to feed the codec.
fn json_text() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        Just("null".to_string()),
        Just("true".to_string()),
        Just("false".to_string()),
        any::<i32>().prop_map(|n| n.to_string()),
        any::<i32>().prop_map(|n| format!("{n}.5")),
        "[a-zA-Z0-9 _\\-]{0,12}".prop_map(|s| format!("\"{s}\"")),
    ];
    leaf.prop_recursive(4, 40, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..5)
                .prop_map(|elems| format!("[{}]", elems.join(","))),
            prop::collection::vec(("[a-z]{1,6}", inner), 0..5).prop_map(|pairs| {
                let body: Vec<String> = pairs
                    .into_iter()
                    .enumerate()
                    .map(|(i, (k, v))| format!("\"k{i}{k}\":{v}"))
                    .collect();
                format!("{{{}}}", body.join(","))
            }),
        ]
    })
}

proptest! {
    /// text -> PJ1 -> text -> PJ1 gives identical bytes (canonicalization is a
    /// fixpoint), and every intermediate document validates.
    #[test]
    fn canonicalization_is_idempotent(t in json_text()) {
        let pj1_a = parse_json_text(&t).expect("valid generated JSON parses");
        pj1_validate(&pj1_a).expect("encoder output validates");
        let text_a = pj1_to_text(&pj1_a).expect("renders");
        let pj1_b = parse_json_text(&text_a).expect("re-parse of canonical text");
        prop_assert_eq!(&pj1_a, &pj1_b, "text={} canon={}", t, text_a);
        let text_b = pj1_to_text(&pj1_b).expect("renders");
        prop_assert_eq!(text_a, text_b);
    }

    /// Parse acceptance and rejection agree with serde_json on generated text.
    #[test]
    fn differential_accept_matches_serde(t in json_text()) {
        let ours = parse_json_text(&t).is_ok();
        let theirs = serde_json::from_str::<serde_json::Value>(&t).is_ok();
        prop_assert_eq!(ours, theirs, "disagreement on {}", t);
    }

    /// `pj1_cmp` never panics and is antisymmetric on generated pairs.
    #[test]
    fn cmp_is_antisymmetric(a in json_text(), b in json_text()) {
        let ea = parse_json_text(&a).unwrap();
        let eb = parse_json_text(&b).unwrap();
        let ab = pj1_cmp(&ea, &eb);
        let ba = pj1_cmp(&eb, &ea);
        prop_assert_eq!(ab, ba.reverse());
    }
}

/// A fixed corpus for the differential edge cases the spec calls out by name.
#[test]
fn differential_edge_cases() {
    // (input, expected-accept). Both parsers must agree on each.
    let cases: &[(&str, bool)] = &[
        ("{}", true),
        ("[]", true),
        ("{\"a\":1,\"a\":2}", true), // duplicate keys accepted (last wins)
        ("\"\\u00e9\"", true),       // unicode escape
        ("\"\\uD83D\\uDE00\"", true), // valid surrogate pair
        ("\"\\uD800\"", false),      // lone high surrogate
        ("\"\\uDC00\"", false),      // lone low surrogate
        ("-0.0", true),
        ("-0", true),
        ("1e400", false),  // overflows f64 -> rejected
        ("01", false),     // leading zero
        ("1.", false),     // no digit after point
        (".5", false),     // no integer part
        ("+1", false),     // leading plus
        ("1 2", false),    // trailing garbage
        ("[1,2,]", false), // trailing comma
        ("{\"a\":}", false),
        ("nul", false),
        ("NaN", false),
        ("Infinity", false),
        ("  42  ", true), // surrounding whitespace ok
    ];
    for (input, expect_ok) in cases {
        let ours = parse_json_text(input).is_ok();
        let theirs = serde_json::from_str::<serde_json::Value>(input).is_ok();
        assert_eq!(
            ours, *expect_ok,
            "our parser disagreed with expectation on {input:?}"
        );
        // serde_json also rejects 1e400? It parses to f64 inf and... serde
        // accepts it as a float. So only assert serde parity where they can
        // agree; 1e400 is the documented divergence (we reject inf).
        if *input != "1e400" {
            assert_eq!(
                ours, theirs,
                "our parser disagreed with serde_json on {input:?}"
            );
        }
    }
}

/// Deep nesting to the cap: 128 accepted, 129 rejected (both as array and
/// object nesting).
#[test]
fn depth_cap_boundary() {
    let ok = format!("{}1{}", "[".repeat(128), "]".repeat(128));
    assert!(parse_json_text(&ok).is_ok(), "128-deep array must parse");
    let deep = format!("{}1{}", "[".repeat(129), "]".repeat(129));
    assert!(parse_json_text(&deep).is_err(), "129-deep must be rejected");
}

// ─── model-based total-order over a hand-built corpus ────────────────────────

/// Ordered corpus (ascending under the documented total order). Building it
/// ascending lets us assert `pj1_cmp(corpus[i], corpus[j]) == i.cmp(j)` for the
/// strictly ordered entries, plus reflexivity and transitivity everywhere.
fn ordered_corpus() -> Vec<Vec<u8>> {
    let texts = [
        "null",
        "false",
        "true",
        "-100",
        "-1.5",
        "0",
        "1", // 1 and 1.0 are numerically equal; keep only one representative
        "2",
        "3.14",
        "1000000",
        "\"\"",
        "\"a\"",
        "\"ab\"",
        "\"b\"",
        "[]",
        "[1]",
        "[1,2]",
        "[2]",
        "{}",
        "{\"a\":1}",
        "{\"a\":1,\"b\":1}",
        "{\"a\":2}",
        "{\"b\":1}",
    ];
    texts.iter().map(|t| parse_json_text(t).unwrap()).collect()
}

#[test]
fn total_order_is_strictly_ascending() {
    let c = ordered_corpus();
    for i in 0..c.len() {
        for j in 0..c.len() {
            let ord = pj1_cmp(&c[i], &c[j]);
            let expected = i.cmp(&j);
            assert_eq!(
                ord, expected,
                "corpus[{i}] vs corpus[{j}] expected {expected:?} got {ord:?}"
            );
        }
    }
}

#[test]
fn total_order_reflexive_and_transitive() {
    let c = ordered_corpus();
    // Reflexive.
    for d in &c {
        assert_eq!(pj1_cmp(d, d), Ordering::Equal);
    }
    // Transitive: a<=b and b<=c implies a<=c (over the whole corpus).
    for a in &c {
        for b in &c {
            for cc in &c {
                let ab = pj1_cmp(a, b);
                let bc = pj1_cmp(b, cc);
                if ab != Ordering::Greater && bc != Ordering::Greater {
                    assert_ne!(pj1_cmp(a, cc), Ordering::Greater, "transitivity violated");
                }
            }
        }
    }
}

#[test]
fn numeric_equality_across_int_float() {
    // int 1 and float 1.0 are numerically equal even though bytes differ.
    let i = parse_json_text("1").unwrap();
    let f = parse_json_text("1.0").unwrap();
    assert_ne!(i, f, "distinct encodings");
    assert_eq!(pj1_cmp(&i, &f), Ordering::Equal, "numerically equal");
}

// ─── zero-alloc path walk ────────────────────────────────────────────────────

#[test]
fn get_hits_misses_and_nesting() {
    let doc =
        parse_json_text("{\"user\":{\"name\":\"ada\",\"tags\":[\"x\",\"y\",\"z\"]},\"age\":41}")
            .unwrap();
    // Nested chain: doc.user.tags[2]
    let user = pj1_get(&doc, &PathSeg::Key("user")).unwrap();
    let tags = pj1_get(user, &PathSeg::Key("tags")).unwrap();
    let t2 = pj1_get(tags, &PathSeg::Index(2)).unwrap();
    assert_eq!(pj1_to_text(t2).unwrap(), "\"z\"");
    // Every returned slice is itself a valid standalone PJ1 document.
    pj1_validate(user).unwrap();
    pj1_validate(tags).unwrap();
    assert_eq!(
        pj1_to_text(pj1_get(&doc, &PathSeg::Key("age")).unwrap()).unwrap(),
        "41"
    );
    // Misses.
    assert!(pj1_get(&doc, &PathSeg::Key("nope")).is_none());
    assert!(pj1_get(tags, &PathSeg::Index(3)).is_none()); // out of range
    assert!(pj1_get(&doc, &PathSeg::Index(0)).is_none()); // object, not array
    assert!(pj1_get(tags, &PathSeg::Key("x")).is_none()); // array, not object
}

#[test]
fn get_weird_and_empty_keys() {
    let doc = parse_json_text("{\"\":0,\"a b\":1,\"\\u00e9\":2,\"quote\\\"d\":3,\"z\":4}").unwrap();
    assert_eq!(text_at(&doc, ""), "0");
    assert_eq!(text_at(&doc, "a b"), "1");
    assert_eq!(text_at(&doc, "é"), "2");
    assert_eq!(text_at(&doc, "quote\"d"), "3");
    assert_eq!(text_at(&doc, "z"), "4");
}

fn text_at(doc: &[u8], key: &str) -> String {
    pj1_to_text(pj1_get(doc, &PathSeg::Key(key)).unwrap()).unwrap()
}
