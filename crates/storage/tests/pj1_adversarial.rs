//! Adversarial PJ1 battery (design 2026-07-13, sections 4.2 and 9).
//!
//! This file is the hostile complement to `pj1_codec.rs`. Where that suite
//! proves the happy-path invariants (idempotent canonicalization, a serde_json
//! differential, model-based total order), this one attacks:
//!
//!   A. canonical-bytes collisions (key order, duplicates, unicode ordering),
//!   B. the total order at its numeric cliffs (int/float, signed zero, 2^53),
//!   C. `pj1_get` on degenerate keys/indices,
//!   D. `pj1_validate` on hand-crafted malformed bytes (must never panic),
//!   E. size and depth boundaries.
//!
//! The load-bearing rule for every malformed-bytes case: `pj1_validate` must
//! return a typed error and must NEVER panic or read out of bounds. Several
//! cases hand-build bytes the parser can never emit, so they exercise the
//! decoder's own bounds checks rather than the encoder.

use powdb_storage::pj1::{parse_json_text, pj1_cmp, pj1_get, pj1_to_text, pj1_validate, PathSeg};
use std::cmp::Ordering;

// PJ1 node tags (private in the crate; re-declared here for hand-crafting).
const TAG_NULL: u8 = 0;
const TAG_INT: u8 = 3;
const TAG_FLOAT: u8 = 4;
const TAG_ARRAY: u8 = 6;
const TAG_OBJECT: u8 = 7;

fn enc(text: &str) -> Vec<u8> {
    parse_json_text(text).unwrap_or_else(|e| panic!("parse `{text}`: {e}"))
}

/// Run `pj1_validate` under a panic guard: the promise is "typed error, never
/// panic". Returns whether it accepted the bytes.
fn validate_no_panic(bytes: &[u8]) -> bool {
    let r = std::panic::catch_unwind(|| pj1_validate(bytes));
    let res = r.unwrap_or_else(|_| panic!("pj1_validate PANICKED on {bytes:?}"));
    res.is_ok()
}

/// Lay out an object node from pairs in the GIVEN order (no sorting, no dedup),
/// so tests can craft the non-canonical key directories the encoder never
/// produces. `values` are pre-encoded PJ1 nodes. Offsets are node-relative.
fn build_object_raw(pairs: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let count = pairs.len();
    let header = 5 + count * 8 + 4;
    let mut key_offs = Vec::with_capacity(count);
    let mut key_region = Vec::new();
    for (k, _) in pairs {
        key_offs.push(header + key_region.len());
        key_region.extend_from_slice(&(k.len() as u32).to_le_bytes());
        key_region.extend_from_slice(k.as_bytes());
    }
    let val_start = header + key_region.len();
    let mut val_offs = Vec::with_capacity(count);
    let mut val_region = Vec::new();
    for (_, v) in pairs {
        val_offs.push(val_start + val_region.len());
        val_region.extend_from_slice(v);
    }
    let end = val_start + val_region.len();

    let mut out = Vec::with_capacity(end);
    out.push(TAG_OBJECT);
    out.extend_from_slice(&(count as u32).to_le_bytes());
    for i in 0..count {
        out.extend_from_slice(&(key_offs[i] as u32).to_le_bytes());
        out.extend_from_slice(&(val_offs[i] as u32).to_le_bytes());
    }
    out.extend_from_slice(&(end as u32).to_le_bytes());
    out.extend_from_slice(&key_region);
    out.extend_from_slice(&val_region);
    out
}

// ─── A. canonical-bytes attacks ──────────────────────────────────────────────

#[test]
fn a_key_order_and_duplicates_collapse_to_equal_bytes() {
    // Differently ordered, and a shadowed duplicate, all canonicalize identical.
    let base = enc(r#"{"a":1,"b":2,"c":3}"#);
    assert_eq!(enc(r#"{"c":3,"a":1,"b":2}"#), base, "key order irrelevant");
    assert_eq!(enc(r#"{"b":2,"c":3,"a":1}"#), base, "another permutation");
    assert_eq!(
        enc(r#"{"a":9,"b":2,"c":3,"a":1}"#),
        base,
        "duplicate key is last-wins and vanishes from the bytes"
    );
}

#[test]
fn a_nested_duplicate_keys_collapse() {
    // Duplicates inside a nested object also collapse last-wins.
    let want = enc(r#"{"o":{"k":2},"z":[1,2]}"#);
    let got = enc(r#"{"z":[1,2],"o":{"k":9,"k":2}}"#);
    assert_eq!(got, want);
}

#[test]
fn a_unicode_keys_sort_bytewise_which_equals_codepoint_order() {
    // PJ1 sorts keys by their UTF-8 bytes. For valid UTF-8 that is IDENTICAL to
    // Unicode code-point order, but it DIVERGES from a naive UTF-16 code-unit
    // sort: U+FFFF (ef bf bf) precedes U+10000 (f0 90 80 80) bytewise and by
    // code point, whereas UTF-16 would place U+10000 (surrogate D800 DC00)
    // first. This test pins the bytewise/code-point behaviour.
    let doc = enc("{\"\u{10000}\":2,\"\u{FFFF}\":1}");
    assert_eq!(
        pj1_to_text(&doc).unwrap(),
        "{\"\u{FFFF}\":1,\"\u{10000}\":2}",
        "U+FFFF sorts before U+10000 (code-point / UTF-8 order, not UTF-16)"
    );

    // ASCII vs multi-byte: every non-ASCII lead byte is >= 0xC0 > any ASCII
    // byte, so an ASCII key always precedes a non-ASCII one, both bytewise and
    // by code point. "z" (0x7A) before "é" (0xC3 0xA9).
    let doc2 = enc("{\"é\":2,\"z\":1}");
    assert_eq!(pj1_to_text(&doc2).unwrap(), "{\"z\":1,\"é\":2}");
}

// ─── B. total-order attacks (pj1_cmp) ────────────────────────────────────────

#[test]
fn b_int_and_float_compare_numerically_but_signed_zero_splits() {
    // 1 == 1.0 numerically across the int/float split.
    assert_eq!(pj1_cmp(&enc("1"), &enc("1.0")), Ordering::Equal);
    // int 0 == float 0.0.
    assert_eq!(pj1_cmp(&enc("0"), &enc("0.0")), Ordering::Equal);
    // But -0.0 and 0.0 are BOTH floats compared with total_cmp, which
    // distinguishes the sign bit: -0.0 < 0.0. (Deliberate: pj1_cmp mirrors
    // Value::cmp, whose float arm is total_cmp. It is a total order, not a
    // numeric-equality relation, which is exactly what index/group keys need.)
    assert_eq!(pj1_cmp(&enc("-0.0"), &enc("0.0")), Ordering::Less);
    // int 0 promotes to +0.0, so it is GREATER than float -0.0.
    assert_eq!(pj1_cmp(&enc("0"), &enc("-0.0")), Ordering::Greater);
    // Consistency: the -0 integer literal canonicalizes to 0, so it equals 0.0.
    assert_eq!(pj1_cmp(&enc("-0"), &enc("0.0")), Ordering::Equal);
}

#[test]
fn b_precision_cliff_around_two_pow_53() {
    // 2^53 and 2^53+1 both fit i64. Promoting the int to f64 rounds 2^53+1 down
    // to 2^53, so the int compares EQUAL to the float 2^53. This is the
    // documented "i64 as f64 loses precision above 2^53 but stays monotonic"
    // behaviour; pin it so a future change to the numeric ladder is caught.
    let int_hi = enc("9007199254740993"); // 2^53 + 1
    let float_53 = enc("9007199254740992.0"); // 2^53 exactly
    assert_eq!(
        pj1_cmp(&int_hi, &float_53),
        Ordering::Equal,
        "2^53+1 (i64) rounds to 2^53 (f64) on promotion"
    );
    // Well below the cliff the comparison is exact.
    assert_eq!(pj1_cmp(&enc("3"), &enc("2.0")), Ordering::Greater);
    assert_eq!(pj1_cmp(&enc("2"), &enc("2.5")), Ordering::Less);
}

#[test]
fn b_i64_extremes_versus_floats() {
    // i64::MAX / i64::MIN as literals, compared to nearby floats. i64::MAX
    // promotes to 2^63; 1e19 > 2^63 so it sorts greater; 1e18 < 2^63 lesser.
    let max = enc("9223372036854775807");
    let min = enc("-9223372036854775808");
    assert_eq!(pj1_cmp(&max, &enc("1e19")), Ordering::Less);
    assert_eq!(pj1_cmp(&max, &enc("1e18")), Ordering::Greater);
    assert_eq!(pj1_cmp(&min, &enc("-1e19")), Ordering::Greater);
    assert_eq!(pj1_cmp(&min, &max), Ordering::Less);
}

#[test]
fn b_empty_string_array_object_ordering() {
    // Type ladder: string(4) < array(5) < object(6), regardless of emptiness.
    let s = enc(r#""""#);
    let a = enc("[]");
    let o = enc("{}");
    assert_eq!(pj1_cmp(&s, &a), Ordering::Less);
    assert_eq!(pj1_cmp(&a, &o), Ordering::Less);
    assert_eq!(pj1_cmp(&s, &o), Ordering::Less);
    // And each is equal to itself.
    for v in [&s, &a, &o] {
        assert_eq!(pj1_cmp(v, v), Ordering::Equal);
    }
    // Empty string < any non-empty string.
    assert_eq!(pj1_cmp(&s, &enc(r#""a""#)), Ordering::Less);
    // Empty array < any non-empty array (length tiebreak on equal prefix).
    assert_eq!(pj1_cmp(&a, &enc("[1]")), Ordering::Less);
}

#[test]
fn b_deep_equal_prefix_arrays_break_on_length() {
    // Equal element-wise up to the shorter length => the shorter sorts first.
    assert_eq!(pj1_cmp(&enc("[1,2,3]"), &enc("[1,2,3,4]")), Ordering::Less);
    assert_eq!(
        pj1_cmp(&enc("[1,2,3,4]"), &enc("[1,2,3]")),
        Ordering::Greater
    );
    assert_eq!(pj1_cmp(&enc("[1,2,3]"), &enc("[1,2,3]")), Ordering::Equal);
    // First differing element wins over length.
    assert_eq!(
        pj1_cmp(&enc("[1,2,9]"), &enc("[1,2,3,4]")),
        Ordering::Greater
    );
    // Nested arrays compare lexicographically at each level.
    assert_eq!(
        pj1_cmp(&enc("[[1,2],[3]]"), &enc("[[1,2],[3,0]]")),
        Ordering::Less
    );
}

// ─── C. pj1_get attacks ──────────────────────────────────────────────────────

#[test]
fn c_key_that_is_a_prefix_of_another() {
    let doc = enc(r#"{"a":1,"ab":2,"abc":3}"#);
    assert_eq!(
        pj1_to_text(pj1_get(&doc, &PathSeg::Key("a")).unwrap()).unwrap(),
        "1"
    );
    assert_eq!(
        pj1_to_text(pj1_get(&doc, &PathSeg::Key("ab")).unwrap()).unwrap(),
        "2"
    );
    assert_eq!(
        pj1_to_text(pj1_get(&doc, &PathSeg::Key("abc")).unwrap()).unwrap(),
        "3"
    );
    // A key that is a prefix-plus-more of a real key, but absent, misses.
    assert!(pj1_get(&doc, &PathSeg::Key("abcd")).is_none());
    assert!(pj1_get(&doc, &PathSeg::Key("b")).is_none());
}

#[test]
fn c_empty_and_number_looking_keys() {
    let doc = enc(r#"{"":10,"0":20,"1":30}"#);
    // Empty-string key resolves.
    assert_eq!(
        pj1_to_text(pj1_get(&doc, &PathSeg::Key("")).unwrap()).unwrap(),
        "10"
    );
    // A key that LOOKS like a number is still a string key, not an index.
    assert_eq!(
        pj1_to_text(pj1_get(&doc, &PathSeg::Key("0")).unwrap()).unwrap(),
        "20"
    );
    // Index access on an object is a category mismatch -> None (not key "0").
    assert!(pj1_get(&doc, &PathSeg::Index(0)).is_none());
}

#[test]
fn c_index_edges_and_paths_into_scalars() {
    let empty = enc("[]");
    assert!(
        pj1_get(&empty, &PathSeg::Index(0)).is_none(),
        "index 0 of []"
    );
    let three = enc("[10,20,30]");
    assert_eq!(
        pj1_to_text(pj1_get(&three, &PathSeg::Index(0)).unwrap()).unwrap(),
        "10"
    );
    assert_eq!(
        pj1_to_text(pj1_get(&three, &PathSeg::Index(2)).unwrap()).unwrap(),
        "30"
    );
    assert!(
        pj1_get(&three, &PathSeg::Index(3)).is_none(),
        "past the end"
    );
    assert!(
        pj1_get(&three, &PathSeg::Index(u32::MAX)).is_none(),
        "u32::MAX index"
    );
    // Key access on an array is a category mismatch.
    assert!(pj1_get(&three, &PathSeg::Key("0")).is_none());
    // Any segment into a scalar node yields None (never a panic).
    for scalar in ["5", "5.5", "true", "null", r#""hi""#] {
        let d = enc(scalar);
        assert!(
            pj1_get(&d, &PathSeg::Key("x")).is_none(),
            "key into {scalar}"
        );
        assert!(
            pj1_get(&d, &PathSeg::Index(0)).is_none(),
            "index into {scalar}"
        );
    }
}

// ─── D. malformed-bytes attacks (pj1_validate must never panic) ───────────────

#[test]
fn d_every_prefix_of_a_valid_doc_is_clean() {
    // Build a rich valid document, then validate EVERY prefix. Each must either
    // validate (and then be renderable — "validated => renderable") or return a
    // typed error. None may panic or read out of bounds.
    let doc = enc(r#"{"arr":[1,2.5,"three",null,true],"nested":{"k":[{"deep":42}]},"z":"éx"}"#);
    assert!(validate_no_panic(&doc), "the full doc validates");
    for i in 0..doc.len() {
        let prefix = &doc[..i];
        let accepted = validate_no_panic(prefix);
        if accepted {
            // The invariant that makes validation useful downstream: anything
            // that validates must render without error.
            assert!(
                pj1_to_text(prefix).is_ok(),
                "validated prefix len {i} failed to render"
            );
        }
    }
}

#[test]
fn d_reserved_and_undefined_top_tags_rejected() {
    // Tags 8..=15 are reserved; anything > 7 must be rejected on decode.
    for tag in 8u8..=15 {
        assert!(!validate_no_panic(&[tag]), "reserved tag {tag} accepted");
    }
    for tag in [16u8, 100, 200, 254, 255] {
        assert!(!validate_no_panic(&[tag]), "undefined tag {tag} accepted");
    }
    // Empty buffer is truncated, not a panic.
    assert!(!validate_no_panic(&[]));
}

#[test]
fn d_truncated_scalar_payloads() {
    // Int/float tags with fewer than 8 payload bytes: Truncated, no panic.
    for len in 0..8 {
        let mut int_bytes = vec![TAG_INT];
        int_bytes.resize(1 + len, 0);
        assert!(
            !validate_no_panic(&int_bytes),
            "int with {len} payload bytes"
        );
        let mut float_bytes = vec![TAG_FLOAT];
        float_bytes.resize(1 + len, 0);
        assert!(
            !validate_no_panic(&float_bytes),
            "float with {len} payload bytes"
        );
    }
}

#[test]
fn d_non_finite_float_bytes_rejected() {
    // A well-formed FLOAT node whose payload decodes to NaN / +Inf / -Inf is
    // non-canonical: validate and render must both reject it rather than emit
    // the non-JSON literal `NaN`/`inf`.
    for bits in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut b = vec![TAG_FLOAT];
        b.extend_from_slice(&bits.to_le_bytes());
        assert!(!validate_no_panic(&b), "non-finite float {bits} accepted");
        assert!(pj1_to_text(&b).is_err(), "non-finite float {bits} rendered");
    }
}

#[test]
fn d_array_offsets_out_of_bounds_and_non_monotonic() {
    // count=2 but the offset table / elements are missing entirely: Truncated.
    let mut trunc = vec![TAG_ARRAY];
    trunc.extend_from_slice(&2u32.to_le_bytes());
    assert!(!validate_no_panic(&trunc));

    // A count so large that (count+1)*4 overflows or exceeds the buffer.
    let mut huge = vec![TAG_ARRAY];
    huge.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(!validate_no_panic(&huge), "u32::MAX element count accepted");

    // Take a real 3-element array and corrupt an interior offset so the element
    // spans go backwards (non-monotonic). Must be rejected, not walked.
    let mut arr = enc("[1,2,3]");
    // Header is [tag][count u32][off0..off3 u32]. off1 sits at byte 9. Rewrite
    // it to a value smaller than off0 so o0 != prev / o1 < o0 trips.
    arr[9..13].copy_from_slice(&0u32.to_le_bytes());
    assert!(
        !validate_no_panic(&arr),
        "non-monotonic array offsets accepted"
    );
}

#[test]
fn d_object_unsorted_or_duplicate_keys_are_rejected() {
    // THE load-bearing decode guard: `pj1_get` binary-searches the key
    // directory, so an object whose keys are NOT strictly increasing would make
    // lookups silently miss. `pj1_validate` must reject such bytes. If this test
    // ever fails (validator accepts unsorted keys), that is a HIGH-severity find
    // because persisted-but-unsorted objects become silently unqueryable.
    let one = enc("1");
    let two = enc("2");

    // Keys laid out in DESCENDING order ("b" before "a").
    let unsorted = build_object_raw(&[("b", two.clone()), ("a", one.clone())]);
    assert!(
        !validate_no_panic(&unsorted),
        "validator accepted a descending key directory"
    );

    // Duplicate keys in the directory (not deduped).
    let dup = build_object_raw(&[("a", one.clone()), ("a", two.clone())]);
    assert!(
        !validate_no_panic(&dup),
        "validator accepted a duplicated key directory"
    );

    // Sanity: the SAME pairs laid out sorted and unique DO validate, proving the
    // rejection above is about order, not about build_object_raw itself.
    let sorted = build_object_raw(&[("a", one), ("b", two)]);
    assert!(
        validate_no_panic(&sorted),
        "sorted equivalent must validate"
    );
    assert_eq!(pj1_to_text(&sorted).unwrap(), r#"{"a":1,"b":2}"#);
}

#[test]
fn d_object_key_offset_pointing_out_of_bounds() {
    // Valid object, then corrupt the first key_off to point past the buffer.
    let mut obj = enc(r#"{"a":1}"#);
    // Pairtable starts at byte 5: [key_off u32][val_off u32][end u32]. Overwrite
    // key_off with a wild value.
    obj[5..9].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    assert!(!validate_no_panic(&obj), "wild key offset accepted");
}

#[test]
fn d_trailing_bytes_after_a_complete_node() {
    // A complete node followed by junk must be rejected (`end != doc.len()`),
    // never silently truncated to the first node.
    let mut doc = enc("null");
    doc.push(0xAB);
    assert!(
        !validate_no_panic(&doc),
        "trailing byte after null accepted"
    );

    let mut arr = enc("[1,2]");
    arr.extend_from_slice(&[TAG_NULL, TAG_NULL]);
    assert!(
        !validate_no_panic(&arr),
        "trailing bytes after array accepted"
    );
}

// ─── E. boundary sizes and depth ─────────────────────────────────────────────

#[test]
fn e_string_values_around_the_4070_byte_inline_edge() {
    // 4070 bytes is the heap inline/overflow boundary (row.rs / page.rs). PJ1
    // itself has no such limit, but string length is a u32 and these are the
    // sizes the storage layer will hand it, so exercise the codec right at the
    // edge. Each round-trips to a string of exactly the input length.
    for n in [4069usize, 4070, 4071] {
        let text = format!("\"{}\"", "x".repeat(n));
        let bytes = enc(&text);
        assert!(validate_no_panic(&bytes), "len {n} failed to validate");
        assert_eq!(pj1_to_text(&bytes).unwrap(), text, "len {n} round-trip");
    }
}

#[test]
fn e_multi_megabyte_document_validates_and_orders() {
    // A document well above any page size (a few MB) to exercise large u32
    // offset arithmetic in validate/cmp/get. The real 64 MiB ceiling
    // (MAX_VALUE_SIZE) is enforced at the storage/catalog layer, not in pj1, so
    // this stays in the low-MB range to keep CI fast (noted per the task).
    let n = 2_000_000usize;
    let text = format!("[\"{}\",\"tail\"]", "y".repeat(n));
    let big = enc(&text);
    assert!(big.len() > n, "encoded at least the payload size");
    assert!(validate_no_panic(&big), "multi-MB doc failed to validate");
    // Path extraction still lands the right element out of the big buffer.
    let e0 = pj1_get(&big, &PathSeg::Index(0)).unwrap();
    match pj1_to_text(e0) {
        Ok(s) => assert_eq!(s.len(), n + 2, "big element reassembles (+2 quotes)"),
        Err(e) => panic!("render of big element failed: {e}"),
    }
    let e1 = pj1_get(&big, &PathSeg::Index(1)).unwrap();
    assert_eq!(pj1_to_text(e1).unwrap(), r#""tail""#);
    // A larger doc sorts after a smaller one with the same first element.
    let small = enc(r#"["yy","tail"]"#);
    assert_eq!(pj1_cmp(&small, &big), Ordering::Less);
}

#[test]
fn e_depth_exactly_128_accepted_129_rejected() {
    // parse_json_text caps nesting at MAX_DEPTH = 128.
    let ok = format!("{}1{}", "[".repeat(128), "]".repeat(128));
    let bytes = parse_json_text(&ok).expect("depth 128 must parse");
    assert!(validate_no_panic(&bytes), "depth-128 doc must validate");
    // Reaching the innermost value confirms all 128 levels are walkable.
    let mut cur: &[u8] = &bytes;
    for _ in 0..128 {
        cur = pj1_get(cur, &PathSeg::Index(0)).expect("descend one array level");
    }
    assert_eq!(pj1_to_text(cur).unwrap(), "1");

    let too_deep = format!("{}1{}", "[".repeat(129), "]".repeat(129));
    assert!(
        parse_json_text(&too_deep).is_err(),
        "depth 129 must be rejected by the parser"
    );
}
