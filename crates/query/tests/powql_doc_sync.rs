//! docs/POWQL.md's "Complete keyword list" claims it is "derived from the
//! lexer's keyword table". This gate makes the claim true by construction:
//! the documented list must equal `POWQL_KEYWORDS` word for word. When it was
//! added, the doc was missing `json_text`, `json_type`, and `raw` — three
//! reserved words a user could trip over with no documentation trail.
//!
//! Same pattern as crates/server/tests/errors_doc_sync.rs (docs/errors.md vs
//! ErrorClass): the doc is data, the code is authority, CI holds them equal.

use powdb_query::lexer::POWQL_KEYWORDS;

#[test]
fn the_documented_keyword_list_equals_the_lexer_table() {
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/POWQL.md"))
        .expect("docs/POWQL.md must exist in the workspace");

    let anchor = "### Complete keyword list";
    let start = doc.find(anchor).expect("keyword-list section must exist");
    let block_start = doc[start..].find("```").expect("code block must open") + start + 3;
    let block_end = doc[block_start..]
        .find("```")
        .expect("code block must close")
        + block_start;

    // No dedup: a word listed twice in the doc is also a doc bug and must fail.
    let mut documented: Vec<&str> = doc[block_start..block_end]
        .split([',', '\n', ' '])
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .collect();
    documented.sort_unstable();

    let mut expected: Vec<&str> = POWQL_KEYWORDS.to_vec();
    expected.sort_unstable();

    let missing: Vec<_> = expected
        .iter()
        .filter(|w| !documented.contains(w))
        .collect();
    let extra: Vec<_> = documented
        .iter()
        .filter(|w| !expected.contains(w))
        .collect();
    assert_eq!(
        documented, expected,
        "docs/POWQL.md keyword list disagrees with lexer POWQL_KEYWORDS \
         (missing from doc: {missing:?}; documented but not reserved: {extra:?})"
    );
}
