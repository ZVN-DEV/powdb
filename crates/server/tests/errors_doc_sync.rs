//! `docs/errors.md` and `ErrorClass::from_u8` must describe the same set of
//! wire error classes.
//!
//! The class byte is documented as stable wire contract, and third-party
//! drivers are written against the table in that document rather than against
//! the Rust source. Until now the two were kept in step by convention: nothing
//! failed if a class was added to the code and not the table, or renamed in
//! one place only. This is that gate.
//!
//! The parse is deliberately strict, and every failure mode is exercised by a
//! negative test below. A doc-parsing gate that quietly matches nothing is the
//! classic vacuous check: it passes forever, including on the day the document
//! is deleted.

use std::collections::BTreeMap;
use std::path::PathBuf;

use powdb_server::protocol::ErrorClass;

/// The number of classes this release documents. Hard-coded on purpose: the
/// document calls the numbering stable wire contract, so gaining a class is a
/// deliberate act that should require touching this file, not something that
/// slips through because both sides moved together.
const DOCUMENTED_CLASS_COUNT: usize = 11;

fn errors_doc_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/errors.md")
}

/// `ErrorClass::Internal` renders as `internal`, `LimitExceeded` as
/// `limit_exceeded`. Derived from the variant rather than listed, so renaming
/// a variant without renaming it in the document fails here.
fn documented_name(class: ErrorClass) -> String {
    let debug = format!("{class:?}");
    let mut out = String::with_capacity(debug.len() + 2);
    for (index, ch) in debug.char_indices() {
        if ch.is_ascii_uppercase() {
            if index != 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Every class the code defines, walked from 0 until `from_u8` refuses.
fn classes_in_code() -> BTreeMap<u8, ErrorClass> {
    let mut found = BTreeMap::new();
    for raw in 0u8..=u8::MAX {
        match ErrorClass::from_u8(raw) {
            Some(class) => {
                found.insert(raw, class);
            }
            None => break,
        }
    }
    found
}

/// Extract `(code, name)` for every row of the class-code table in `markdown`.
///
/// Fails loudly rather than returning an empty list: "found nothing" and
/// "found nothing wrong" must not be the same answer.
fn parse_documented_classes(markdown: &str) -> Result<Vec<(u8, String)>, String> {
    let mut lines = markdown.lines();
    lines
        .by_ref()
        .find(|line| line.trim_end() == "## Class codes")
        .ok_or("docs/errors.md has no `## Class codes` section")?;
    // The table is the first thing in that section; stop at the next heading
    // so a later table in the document can never be mistaken for this one.
    let section: Vec<&str> = lines.take_while(|line| !line.starts_with('#')).collect();

    let mut body = section.into_iter();
    body.by_ref()
        .find(|line| line.starts_with("| Code | Name |"))
        .ok_or("the `## Class codes` section has no `| Code | Name |` table")?;
    let separator = body
        .next()
        .ok_or("the class-code table ends after its header row")?;
    if !separator.starts_with("|--") {
        return Err(format!(
            "expected a markdown separator row under the table header, got {separator:?}"
        ));
    }

    let mut rows = Vec::new();
    for line in body {
        if !line.starts_with('|') {
            break;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cells.len() < 2 {
            return Err(format!("class-code row has fewer than two cells: {line:?}"));
        }
        let code: u8 = cells[0]
            .parse()
            .map_err(|_| format!("class-code row does not start with a number: {line:?}"))?;
        let name = cells[1].trim_matches('`').to_string();
        if name.is_empty() || name.contains(' ') {
            return Err(format!("class-code row has no usable class name: {line:?}"));
        }
        rows.push((code, name));
    }

    if rows.is_empty() {
        return Err("the class-code table has no rows".into());
    }
    Ok(rows)
}

/// The whole contract, as a function so the negative tests below can run it
/// against doctored documents.
fn check(markdown: &str) -> Result<usize, String> {
    let documented = parse_documented_classes(markdown)?;
    let in_code = classes_in_code();

    let mut seen: BTreeMap<u8, String> = BTreeMap::new();
    for (code, name) in &documented {
        if let Some(previous) = seen.insert(*code, name.clone()) {
            return Err(format!(
                "class {code} is documented twice, as {previous:?} and {name:?}"
            ));
        }
    }

    for (code, name) in &documented {
        match ErrorClass::from_u8(*code) {
            Some(class) => {
                let expected = documented_name(class);
                if expected != *name {
                    return Err(format!(
                        "class {code} is `{name}` in docs/errors.md but {class:?} \
                         (`{expected}`) in ErrorClass"
                    ));
                }
            }
            None => {
                return Err(format!(
                    "docs/errors.md documents class {code} (`{name}`), which \
                     ErrorClass::from_u8 does not recognize"
                ));
            }
        }
    }

    for (code, class) in &in_code {
        if !seen.contains_key(code) {
            return Err(format!(
                "ErrorClass defines class {code} ({class:?}) with no row in \
                 docs/errors.md"
            ));
        }
    }

    Ok(documented.len())
}

#[test]
fn errors_doc_matches_error_class_exhaustively() {
    let path = errors_doc_path();
    let markdown = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()));

    let count = check(&markdown).unwrap_or_else(|reason| {
        panic!("docs/errors.md and ErrorClass have drifted: {reason}");
    });

    assert_eq!(
        count, DOCUMENTED_CLASS_COUNT,
        "expected exactly {DOCUMENTED_CLASS_COUNT} documented classes; a parse \
         that silently matched a different number is how this gate goes vacuous"
    );
    assert_eq!(
        classes_in_code().len(),
        DOCUMENTED_CLASS_COUNT,
        "ErrorClass::from_u8 accepts a number of classes the document does not"
    );
    assert!(
        ErrorClass::from_u8(DOCUMENTED_CLASS_COUNT as u8).is_none(),
        "class {DOCUMENTED_CLASS_COUNT} exists in code but the document stops \
         at {}",
        DOCUMENTED_CLASS_COUNT - 1
    );
}

// ---------------------------------------------------------------------------
// Negative controls: every way the gate must fail, proven to fail.
// ---------------------------------------------------------------------------

/// The real table, rebuilt from the code so these fixtures cannot drift.
fn synthetic_doc(rows: &[(u8, String)]) -> String {
    let mut out = String::from("# Wire error classes\n\n## Class codes\n\n");
    out.push_str("| Code | Name | Meaning | Typical causes | TS client |\n");
    out.push_str("|------|------|---------|----------------|-----------|\n");
    for (code, name) in rows {
        out.push_str(&format!("| {code} | `{name}` | m | c | t |\n"));
    }
    out.push_str("\nTrailing prose.\n");
    out
}

fn real_rows() -> Vec<(u8, String)> {
    classes_in_code()
        .into_iter()
        .map(|(code, class)| (code, documented_name(class)))
        .collect()
}

/// The fixture builder must itself pass, otherwise every negative test below
/// would "fail correctly" for the wrong reason.
#[test]
fn the_synthetic_fixture_is_a_valid_document() {
    assert_eq!(
        check(&synthetic_doc(&real_rows())),
        Ok(DOCUMENTED_CLASS_COUNT)
    );
}

#[test]
fn a_twelfth_documented_class_fails() {
    let mut rows = real_rows();
    rows.push((11, "quota_exhausted".into()));
    let error = check(&synthetic_doc(&rows)).expect_err("an undefined class must fail");
    assert!(
        error.contains("does not recognize"),
        "unexpected reason: {error}"
    );
}

#[test]
fn a_renamed_class_fails() {
    let mut rows = real_rows();
    rows[4].1 = "size_exceeded".into(); // the TS name, not the wire name
    let error = check(&synthetic_doc(&rows)).expect_err("a renamed class must fail");
    assert!(
        error.contains("limit_exceeded"),
        "unexpected reason: {error}"
    );
}

#[test]
fn a_missing_class_fails() {
    let mut rows = real_rows();
    rows.retain(|(code, _)| *code != 7);
    let error = check(&synthetic_doc(&rows)).expect_err("a dropped class must fail");
    assert!(
        error.contains("no row in docs/errors.md"),
        "unexpected reason: {error}"
    );
}

#[test]
fn a_duplicated_class_fails() {
    let mut rows = real_rows();
    rows.push((3, "timeout".into()));
    let error = check(&synthetic_doc(&rows)).expect_err("a duplicated class must fail");
    assert!(
        error.contains("documented twice"),
        "unexpected reason: {error}"
    );
}

/// The vacuity guard itself: a document with no table, an empty document, and
/// a table with a header but no rows must all be errors, never "nothing found,
/// therefore nothing wrong".
#[test]
fn a_document_the_parser_cannot_read_fails() {
    let cases = [
        ("", "no `## Class codes` section"),
        (
            "# Wire error classes\n\nProse only.\n",
            "no `## Class codes`",
        ),
        (
            "## Class codes\n\nProse but no table.\n",
            "no `| Code | Name |` table",
        ),
        (
            "\n## Class codes\n\n| Code | Name | Meaning |\n|---|---|---|\n\nprose\n",
            "no rows",
        ),
    ];
    for (markdown, expected) in cases {
        let error = check(markdown).expect_err("an unreadable document must fail");
        assert!(
            error.contains(expected),
            "expected a failure mentioning {expected:?}, got {error:?}"
        );
    }
}
