//! Full result-set comparison.
//!
//! Nothing here compares counts. Comparing counts is precisely the weakness
//! that let a datetime bug live through a green suite: two engines can return
//! the same number of wrong rows all day. Every column name, every value and
//! every value's *type* is compared, and row order is compared whenever the
//! query fixes one.

use crate::model::{OVal, Outcome, ResultSet};

/// The first difference found between two outcomes, or `None` if they agree.
///
/// Only the first difference is reported: a single wrong row usually cascades,
/// and one precise line is more useful than a hundred derived ones.
pub fn diff(left: &Outcome, right: &Outcome, ordered: bool) -> Option<String> {
    match (left, right) {
        (Outcome::Error(a), Outcome::Error(b)) => {
            // Two engines refusing the same query agree about the answer even
            // though their words differ. Error *text* is engine-specific and
            // is not part of the contract the oracle checks.
            let _ = (a, b);
            None
        }
        (Outcome::Error(a), Outcome::Rows(rs)) => Some(format!(
            "left errored ({a}) while right returned {} row(s)",
            rs.rows.len()
        )),
        (Outcome::Rows(rs), Outcome::Error(b)) => Some(format!(
            "right errored ({b}) while left returned {} row(s)",
            rs.rows.len()
        )),
        (Outcome::Rows(a), Outcome::Rows(b)) => diff_rows(a, b, ordered),
    }
}

fn diff_rows(a: &ResultSet, b: &ResultSet, ordered: bool) -> Option<String> {
    if a.columns != b.columns {
        return Some(format!(
            "column names differ: {:?} vs {:?}",
            a.columns, b.columns
        ));
    }
    if a.rows.len() != b.rows.len() {
        // Naming the rows that are actually missing is what makes a count
        // mismatch triageable. A bare count would be exactly the kind of
        // report that gets hand-waved.
        return Some(format!(
            "row count differs: {} vs {}{}",
            a.rows.len(),
            b.rows.len(),
            exclusive_examples(&a.rows, &b.rows)
        ));
    }

    // For an unordered query the engines are free to choose any order, so both
    // sides are canonicalised with the oracle's own total order first. This is
    // a multiset comparison, not a set comparison: duplicates still have to
    // match one for one.
    let (left, right);
    let (la, lb): (&[Vec<OVal>], &[Vec<OVal>]) = if ordered {
        (&a.rows, &b.rows)
    } else {
        left = sorted(&a.rows);
        right = sorted(&b.rows);
        (&left, &right)
    };

    for (idx, (ra, rb)) in la.iter().zip(lb.iter()).enumerate() {
        if ra.len() != rb.len() {
            return Some(format!(
                "row {idx} width differs: {} vs {}",
                ra.len(),
                rb.len()
            ));
        }
        for (col, (va, vb)) in ra.iter().zip(rb.iter()).enumerate() {
            // Tag first, so an int that happens to equal a bool, or a str that
            // happens to spell the same characters as a json document, is
            // still a difference.
            if va.tag() != vb.tag() || va != vb {
                let name = a.columns.get(col).map_or("?", String::as_str);
                let where_ = if ordered { "at row" } else { "at sorted row" };
                return Some(format!("{where_} {idx}, column `{name}`: {va} vs {vb}"));
            }
        }
    }
    None
}

fn sorted(rows: &[Vec<OVal>]) -> Vec<Vec<OVal>> {
    let mut out = rows.to_vec();
    out.sort();
    out
}

/// Up to three rows each side has that the other does not, rendered inline.
fn exclusive_examples(a: &[Vec<OVal>], b: &[Vec<OVal>]) -> String {
    let only = |from: &[Vec<OVal>], other: &[Vec<OVal>]| -> Vec<String> {
        let mut pool: Vec<&Vec<OVal>> = other.iter().collect();
        let mut out = Vec::new();
        for row in from {
            match pool.iter().position(|r| *r == row) {
                Some(idx) => {
                    pool.swap_remove(idx);
                }
                None => out.push(render(row)),
            }
        }
        out
    };
    let left = only(a, b);
    let right = only(b, a);
    let mut parts = String::new();
    if !left.is_empty() {
        parts.push_str(&format!("; only left: [{}]", head(&left)));
    }
    if !right.is_empty() {
        parts.push_str(&format!("; only right: [{}]", head(&right)));
    }
    parts
}

fn head(items: &[String]) -> String {
    let shown = items.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    if items.len() > 3 {
        format!("{shown}, +{} more", items.len() - 3)
    } else {
        shown
    }
}

fn render(row: &[OVal]) -> String {
    format!(
        "({})",
        row.iter()
            .map(OVal::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rs(columns: &[&str], rows: Vec<Vec<OVal>>) -> Outcome {
        Outcome::Rows(ResultSet {
            columns: columns.iter().map(|s| (*s).to_string()).collect(),
            rows,
        })
    }

    #[test]
    fn identical_results_agree() {
        let a = rs(&["id"], vec![vec![OVal::Int(1)], vec![OVal::Int(2)]]);
        let b = rs(&["id"], vec![vec![OVal::Int(1)], vec![OVal::Int(2)]]);
        assert_eq!(diff(&a, &b, true), None);
    }

    #[test]
    fn column_names_are_compared() {
        let a = rs(&["id"], vec![vec![OVal::Int(1)]]);
        let b = rs(&["ID"], vec![vec![OVal::Int(1)]]);
        assert!(diff(&a, &b, false)
            .expect("diff")
            .contains("column names differ"));
    }

    #[test]
    fn row_order_is_compared_only_when_the_query_fixes_one() {
        let a = rs(&["id"], vec![vec![OVal::Int(1)], vec![OVal::Int(2)]]);
        let b = rs(&["id"], vec![vec![OVal::Int(2)], vec![OVal::Int(1)]]);
        assert!(
            diff(&a, &b, true).is_some(),
            "ordered comparison must see the swap"
        );
        assert_eq!(diff(&a, &b, false), None, "unordered comparison must not");
    }

    #[test]
    fn a_row_count_mismatch_names_the_missing_rows() {
        let a = rs(&["id"], vec![vec![OVal::Int(1)], vec![OVal::Int(2)]]);
        let b = rs(&["id"], vec![vec![OVal::Int(1)]]);
        let detail = diff(&a, &b, false).expect("diff");
        assert!(
            detail.contains("only left: [(int:2)]"),
            "unexpected: {detail}"
        );
        let detail = diff(&b, &a, false).expect("diff");
        assert!(
            detail.contains("only right: [(int:2)]"),
            "unexpected: {detail}"
        );
    }

    #[test]
    fn duplicates_must_match_one_for_one() {
        let a = rs(&["id"], vec![vec![OVal::Int(1)], vec![OVal::Int(1)]]);
        let b = rs(&["id"], vec![vec![OVal::Int(1)], vec![OVal::Int(2)]]);
        assert!(diff(&a, &b, false).is_some());
    }

    #[test]
    fn types_are_compared_not_just_values() {
        let a = rs(&["v"], vec![vec![OVal::Int(1)]]);
        let b = rs(&["v"], vec![vec![OVal::Bool(true)]]);
        assert!(
            diff(&a, &b, false).is_some(),
            "int 1 must not pass for bool true"
        );

        let c = rs(&["v"], vec![vec![OVal::Int(1)]]);
        let d = rs(&["v"], vec![vec![OVal::Float(1.0)]]);
        assert!(
            diff(&c, &d, false).is_some(),
            "int 1 must not pass for float 1.0"
        );

        let e = rs(&["v"], vec![vec![OVal::Str("{}".into())]]);
        let f = rs(&["v"], vec![vec![OVal::Json("{}".into())]]);
        assert!(diff(&e, &f, false).is_some(), "str must not pass for json");

        let g = rs(&["v"], vec![vec![OVal::Int(0)]]);
        let h = rs(&["v"], vec![vec![OVal::DateTime(0)]]);
        assert!(
            diff(&g, &h, false).is_some(),
            "int must not pass for datetime"
        );
    }

    #[test]
    fn negative_zero_is_not_zero() {
        let a = rs(&["v"], vec![vec![OVal::Float(0.0)]]);
        let b = rs(&["v"], vec![vec![OVal::Float(-0.0)]]);
        assert!(diff(&a, &b, false).is_some());
    }

    #[test]
    fn null_is_not_a_value() {
        let a = rs(&["v"], vec![vec![OVal::Null]]);
        let b = rs(&["v"], vec![vec![OVal::Int(0)]]);
        assert!(diff(&a, &b, false).is_some());
        let c = rs(&["v"], vec![vec![OVal::Null]]);
        let d = rs(&["v"], vec![vec![OVal::Str(String::new())]]);
        assert!(diff(&c, &d, false).is_some());
    }

    #[test]
    fn an_error_against_rows_is_a_divergence() {
        let a = Outcome::Error("boom".into());
        let b = rs(&["id"], vec![vec![OVal::Int(1)]]);
        assert!(diff(&a, &b, false).is_some());
        assert!(diff(&b, &a, false).is_some());
    }

    #[test]
    fn two_errors_agree_regardless_of_wording() {
        let a = Outcome::Error("integer overflow".into());
        let b = Outcome::Error("cannot compute sum: the integer total overflows int64".into());
        assert_eq!(diff(&a, &b, false), None);
    }

    #[test]
    fn row_counts_alone_never_decide_agreement() {
        // Same count, entirely different rows: this is the case a count-only
        // comparison passes and this one must not.
        let a = rs(&["id"], vec![vec![OVal::Int(1)], vec![OVal::Int(2)]]);
        let b = rs(&["id"], vec![vec![OVal::Int(3)], vec![OVal::Int(4)]]);
        assert!(diff(&a, &b, false).is_some());
    }
}
