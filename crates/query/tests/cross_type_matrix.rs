//! Exhaustive cross-type comparison matrix, checked in as expected output.
//!
//! Comparison between a column of one type and a literal of another is decided
//! by coercion rules that live in several places at once: the parser's literal
//! typing, `coerce_value`, the compiled byte-level leaves, and the generic
//! `Value::cmp`. There is no single place to read them off, so a change to any
//! one of them can quietly change what a query means for a type nobody wrote a
//! test for.
//!
//! This test enumerates every (column type, operator, literal) triple against a
//! two-row fixture and writes the outcome to
//! `crates/query/tests/expected/cross_type_matrix.txt`. The file is compared
//! byte for byte, so a coercion change is a reviewable diff in a pull request
//! rather than an invisible behavior shift.
//!
//! Regenerate deliberately:
//!
//! ```text
//! UPDATE_EXPECT=1 cargo test -p powdb-query --test cross_type_matrix
//! ```
//!
//! Every cell is evaluated twice, once with the executor fast paths on and once
//! with them forced off. A cell where the two disagree is written as
//! `DIVERGED(fast=…, generic=…)`, and the set of divergent cells is compared
//! against `KNOWN_PATH_DIVERGENCES` below. Regenerating the snapshot is not
//! enough to accept a new one: the list has to be edited too.
//!
//! ## The index axis
//!
//! Every cell is ALSO evaluated with a plain B-tree and with a unique index on
//! the column under test, at each path setting, and required to give the same
//! answer as the unindexed run. An index is an access path, never a semantic.
//!
//! This axis exists because without it the file could not see the defect class
//! it is nominally about. Cross-type coercion has two halves: what a comparison
//! MEANS, which the cells below pin, and what byte lane an index probe built
//! from the same literal addresses, which nothing here could reach while the
//! fixture carried no index on any tested column. The two halves went out of
//! step (`.price < 3` against an indexed float column answered 0 where the scan
//! answered 2) and this snapshot stayed green throughout.
//!
//! A cell where an indexed run disagrees with the unindexed one is written as
//! `INDEX_DIVERGED(...)` and pinned in `KNOWN_INDEX_DIVERGENCES`, on the same
//! terms as the path divergences: a new one is a new bug, and fixing an old one
//! fails the test until the list shrinks.

#![cfg(feature = "testing")]

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

const UUID_TEXT: &str = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";

/// One column of the fixture: the PowQL declaration and the value the populated
/// row holds. Every populated value denotes "one" in its own type where the
/// type can express it, so a coercion that succeeds has an obvious answer and a
/// coercion that refuses has an obvious message.
struct Column {
    name: &'static str,
    declaration: &'static str,
    value: &'static str,
}

const COLUMNS: &[Column] = &[
    Column {
        name: "c_int",
        declaration: "c_int: int",
        value: "1",
    },
    Column {
        name: "c_float",
        declaration: "c_float: float",
        value: "1.0",
    },
    Column {
        name: "c_str",
        declaration: "c_str: str",
        value: "\"1\"",
    },
    Column {
        name: "c_bool",
        declaration: "c_bool: bool",
        value: "true",
    },
    Column {
        name: "c_dt",
        declaration: "c_dt: datetime",
        value: "1",
    },
    Column {
        name: "c_uuid",
        declaration: "c_uuid: uuid",
        value: "\"UUID\"",
    },
    Column {
        name: "c_bytes",
        declaration: "c_bytes: bytes",
        value: "\"\\\\x01\"",
    },
    Column {
        name: "c_json",
        declaration: "c_json: json",
        value: "\"1\"",
    },
];

/// One literal, with a stable label so the snapshot stays readable.
struct Literal {
    label: &'static str,
    text: &'static str,
}

const LITERALS: &[Literal] = &[
    Literal {
        label: "int_0",
        text: "0",
    },
    Literal {
        label: "int_1",
        text: "1",
    },
    Literal {
        label: "int_2",
        text: "2",
    },
    Literal {
        label: "int_neg",
        text: "-1",
    },
    Literal {
        label: "float_1_0",
        text: "1.0",
    },
    Literal {
        label: "float_1_5",
        text: "1.5",
    },
    Literal {
        label: "str_1",
        text: "\"1\"",
    },
    Literal {
        label: "str_a",
        text: "\"a\"",
    },
    Literal {
        label: "str_empty",
        text: "\"\"",
    },
    Literal {
        label: "bool_true",
        text: "true",
    },
    Literal {
        label: "bool_false",
        text: "false",
    },
    Literal {
        label: "null",
        text: "null",
    },
    Literal {
        label: "uuid_lit",
        text: "uuid(\"UUID\")",
    },
    Literal {
        label: "bytes_lit",
        text: "bytes(\"\\\\x01\")",
    },
];

const OPERATORS: &[&str] = &["=", "!=", "<", "<=", ">", ">="];

fn schema() -> String {
    let columns: Vec<&str> = COLUMNS.iter().map(|column| column.declaration).collect();
    format!(
        "type X {{ required unique id: int, {} }}",
        columns.join(", ")
    )
}

/// Two rows: one populated in every column, one entirely null. The null row
/// makes the documented two-valued NULL rule visible per type (it must never
/// match, including under `!=`), and keeps every count in `{0, 1}` so the
/// snapshot stays legible.
fn populated_row() -> String {
    let assignments: Vec<String> = COLUMNS
        .iter()
        .map(|column| {
            format!(
                "{} := {}",
                column.name,
                column.value.replace("UUID", UUID_TEXT)
            )
        })
        .collect();
    format!("insert X {{ id := 1, {} }}", assignments.join(", "))
}

/// The index state a fixture carries on every column it can.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Index {
    /// No index anywhere: the reference every other state is compared against.
    None,
    Btree,
    Unique,
}

impl Index {
    /// The `alter` verb, or `None` for the unindexed reference.
    fn verb(self) -> Option<&'static str> {
        match self {
            Index::None => None,
            Index::Btree => Some("index"),
            Index::Unique => Some("unique"),
        }
    }
}

/// The two indexed states, each compared against [`Index::None`].
const INDEXED: [Index; 2] = [Index::Btree, Index::Unique];

fn build(force_generic: bool) -> (Engine, tempfile::TempDir) {
    let (engine, dir, _) = build_indexed(force_generic, Index::None);
    (engine, dir)
}

/// Build the fixture and try to index every column under test.
///
/// Not every type can carry one, so the columns that accepted the DDL are
/// returned rather than assumed: an axis that silently stopped applying to a
/// column would otherwise look exactly like an axis that applies and finds
/// nothing. [`the_index_axis_reaches_the_columns_it_claims_to`] pins that set.
fn build_indexed(force_generic: bool, index: Index) -> (Engine, tempfile::TempDir, Vec<String>) {
    let dir = tempfile::tempdir().expect("temp dir for the matrix engine");
    let mut engine = Engine::new(dir.path()).expect("engine opens over a fresh temp dir");
    for statement in [
        schema(),
        populated_row(),
        "insert X { id := 2 }".to_string(),
    ] {
        engine
            .execute_powql(&statement)
            .unwrap_or_else(|err| panic!("fixture statement `{statement}` failed: {err}"));
    }
    let mut indexed = Vec::new();
    if let Some(verb) = index.verb() {
        for column in COLUMNS {
            let statement = format!("alter X add {verb} .{}", column.name);
            if engine.execute_powql(&statement).is_ok() {
                indexed.push(column.name.to_string());
            }
        }
    }
    engine.set_force_generic_path(force_generic);
    (engine, dir, indexed)
}

/// The outcome of one cell, rendered for the snapshot.
fn cell(engine: &mut Engine, query: &str) -> String {
    match engine.execute_powql(query) {
        Ok(QueryResult::Scalar(Value::Int(n))) => n.to_string(),
        Ok(other) => format!("UNEXPECTED({other:?})"),
        // Only the error's kind and wording matter here, and they are already
        // short; keeping the whole message means a reworded error is a visible
        // diff instead of a silent one.
        Err(err) => format!("ERR({err})"),
    }
}

/// Everything one full sweep produced: the snapshot text, the fast/generic
/// path divergences, and the index divergences.
struct Sweep {
    text: String,
    path_divergences: Vec<String>,
    index_divergences: Vec<String>,
}

fn render() -> Sweep {
    let (mut fast, _fast_dir) = build(false);
    let (mut generic, _generic_dir) = build(true);
    // One engine per (index kind, path setting). Each indexed engine is
    // compared against the unindexed engine at the SAME path setting, so an
    // index is never asked to reproduce an answer the unindexed run does not
    // give either.
    let mut indexed: Vec<(Index, bool, Engine, tempfile::TempDir)> = Vec::new();
    for index in INDEXED {
        for force_generic in [false, true] {
            let (engine, dir, _) = build_indexed(force_generic, index);
            indexed.push((index, force_generic, engine, dir));
        }
    }

    let mut out = String::new();
    out.push_str(
        "# PowDB cross-type comparison matrix.\n\
         #\n\
         # Generated by crates/query/tests/cross_type_matrix.rs. Do not hand-edit:\n\
         #   UPDATE_EXPECT=1 cargo test -p powdb-query --test cross_type_matrix\n\
         #\n\
         # Fixture: one row with every column populated (each holding \"one\" in its\n\
         # own type) and one row that is null in every column. Each cell is the row\n\
         # count returned by `count(X filter .<column> <op> <literal>)`, so 1 means\n\
         # the populated row matched, 0 means nothing matched, and ERR(...) means the\n\
         # comparison was refused. The null row must never match, including under\n\
         # `!=`: that is PowDB's documented two-valued NULL rule.\n\
         #\n\
         # Every cell is evaluated with the executor fast paths on and again with\n\
         # them forced off. They must agree; a disagreement is written as\n\
         # DIVERGED(...) and fails the test.\n\
         #\n\
         # Every cell is also evaluated with a plain and with a unique index on\n\
         # the column, at each path setting. An index must never change the\n\
         # answer; one that does is written as INDEX_DIVERGED(...).\n\
         #\n\
         # column   op   literal        result\n",
    );

    let mut divergences = Vec::new();
    let mut index_divergences = Vec::new();
    for column in COLUMNS {
        out.push('\n');
        for operator in OPERATORS {
            for literal in LITERALS {
                let query = format!(
                    "count(X filter .{} {operator} {})",
                    column.name,
                    literal.text.replace("UUID", UUID_TEXT)
                );
                let fast_cell = cell(&mut fast, &query);
                let generic_cell = cell(&mut generic, &query);
                let mut rendered = if fast_cell == generic_cell {
                    fast_cell.clone()
                } else {
                    divergences.push(format!(
                        "{} {operator} {}: fast={fast_cell} generic={generic_cell}",
                        column.name, literal.label
                    ));
                    format!("DIVERGED(fast={fast_cell}, generic={generic_cell})")
                };
                for (index, force_generic, engine, _) in indexed.iter_mut() {
                    let indexed_cell = cell(engine, &query);
                    let reference = if *force_generic {
                        &generic_cell
                    } else {
                        &fast_cell
                    };
                    if &indexed_cell != reference {
                        let paths = if *force_generic { "generic" } else { "fast" };
                        index_divergences.push(format!(
                            "{} {operator} {} [{index:?}/{paths}]: none={reference} \
                             index={indexed_cell}",
                            column.name, literal.label
                        ));
                        rendered.push_str(&format!(
                            " INDEX_DIVERGED({index:?}/{paths}: none={reference}, \
                             index={indexed_cell})"
                        ));
                    }
                }
                out.push_str(&format!(
                    "{:<9} {:<4} {:<14} {}\n",
                    column.name, operator, literal.label, rendered
                ));
            }
        }
    }
    Sweep {
        text: out,
        path_divergences: divergences,
        index_divergences,
    }
}

fn expected_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("expected")
        .join("cross_type_matrix.txt")
}

#[test]
fn cross_type_comparison_matrix_matches_the_checked_in_snapshot() {
    let sweep = render();
    let rendered = sweep.text;
    let divergences = sweep.path_divergences;
    let path = expected_path();

    if std::env::var_os("UPDATE_EXPECT").is_some() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("expected/ directory is creatable");
        }
        std::fs::write(&path, &rendered).expect("snapshot is writable");
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "cannot read {}: {err}. Regenerate with \
             `UPDATE_EXPECT=1 cargo test -p powdb-query --test cross_type_matrix`",
            path.display()
        )
    });

    if expected != rendered {
        let diff = first_difference(&expected, &rendered);
        panic!(
            "the cross-type comparison matrix changed. If the change is intended, \
             regenerate it with `UPDATE_EXPECT=1 cargo test -p powdb-query --test \
             cross_type_matrix` and review the diff.\n{diff}"
        );
    }

    // The snapshot already records each divergence, but a snapshot can be
    // regenerated in one command. Accepting a path divergence has to cost a
    // second, deliberate edit, and removing one has to be noticed too, so the
    // observed set is compared against an explicit list rather than merely
    // being allowed to exist.
    let mut observed = divergences;
    observed.sort();
    let mut known: Vec<String> = KNOWN_PATH_DIVERGENCES
        .iter()
        .map(|s| s.to_string())
        .collect();
    known.sort();
    assert_eq!(
        observed, known,
        "the set of fast-path/generic coercion divergences changed. A new entry is a \
         new bug; a missing entry means one was fixed and KNOWN_PATH_DIVERGENCES should \
         shrink."
    );

    // Same discipline for the index axis. Kept a separate list from the path
    // divergences because the two have different causes and different owners:
    // a path divergence is two evaluators disagreeing about what a comparison
    // means, an index divergence is a probe addressing different keys than the
    // scan reads.
    let mut observed_index = sweep.index_divergences;
    observed_index.sort();
    observed_index.dedup();
    let mut known_index: Vec<String> = KNOWN_INDEX_DIVERGENCES
        .iter()
        .map(|s| s.to_string())
        .collect();
    known_index.sort();
    assert_eq!(
        observed_index, known_index,
        "the set of cells where an index changes the answer changed. A new entry is a \
         new bug; a missing entry means one was fixed and KNOWN_INDEX_DIVERGENCES \
         should shrink."
    );
}

/// Cells where an index gives a different answer from the sequential scan over
/// the same rows.
///
/// An index is an access path, never a semantic, so an entry here is a live
/// defect rather than a policy.
///
/// Empty, and it has to stay empty: the list exists so that an index that
/// starts changing an answer fails the build rather than being absorbed into a
/// regenerated snapshot.
///
/// It held one entry until the generic evaluator was taught cross-type numeric
/// comparison. `c_float` holds `1.0`, and `.c_float = 1` was true under the
/// compiled float leaf and false under the generic evaluator; a B-tree stores
/// one key per row, so a probe necessarily reproduced one of those two answers
/// and not the other. It reproduced the compiled one, so the cell only showed
/// up in the forced-generic column, where the reference it is compared against
/// was the OTHER answer. With the two evaluators agreeing there is no second
/// answer for the probe to disagree with.
///
/// The float-literal-against-an-int-column cells were never here. Those are the
/// ones an index used to get wrong on its own (`.price < 3` against an indexed
/// float column answered 0 where the scan answered 2), and this file could not
/// see them at all until the index axis existed. They run in every cell above
/// and agree.
const KNOWN_INDEX_DIVERGENCES: &[&str] = &[];

/// The index axis has to actually reach the columns it claims to.
///
/// `alter X add index .<col>` is refused for some column types, and a refusal
/// is indistinguishable from a clean run in the sweep above: both produce an
/// engine whose answers match the unindexed one. Pinning the accepted set is
/// what separates "the index axis found nothing" from "the index axis was not
/// applied", which is the difference between coverage and the appearance of it.
#[test]
fn the_index_axis_reaches_the_columns_it_claims_to() {
    for index in INDEXED {
        let (_engine, _dir, indexed) = build_indexed(false, index);
        let all: Vec<String> = COLUMNS
            .iter()
            .map(|column| column.name.to_string())
            .collect();
        assert_eq!(
            indexed, all,
            "the {index:?} axis only reached {indexed:?} of {all:?}. If a type \
             genuinely cannot be indexed, list it here deliberately; do not let the \
             axis quietly skip it."
        );
    }
}

/// Cells where the compiled byte-level predicate and the generic evaluator
/// disagree. Every entry is a live defect, not a policy: the answer to the
/// query would depend on whether a fast path happened to match its plan shape.
///
/// Empty. It held the two `c_float` / `int_1` cells: `c_float` holds `1.0`, the
/// compiled float leaf coerced the int literal `1` to `1.0` and reported
/// equality, and the generic evaluator compared `Value::Float(1.0)` with
/// `Value::Int(1)` across enum variants and reported inequality. The generic
/// side was also self-contradictory about it, answering true to both
/// `.c_float <= 1` and `.c_float >= 1`. The generic comparison operators now
/// compare numeric pairs numerically (`eval::cross_type_numeric_cmp`), which
/// closes both at once; see
/// `int_and_float_comparison_is_a_consistent_total_order`.
const KNOWN_PATH_DIVERGENCES: &[&str] = &[];

/// The six comparison operators over a mixed int/float pair have to describe
/// ONE order.
///
/// This pinned the contradiction before it was fixed: for an `int` column
/// holding `1` and the float literal `1.0`, PowDB answered `false` to `=`,
/// `false` to `<`, `false` to `>`, and `true` to both `<=` and `>=`. No total
/// order permits that, since if neither value is less than the other and each
/// is "less than or equal" to the other, they are equal. `=` and `!=` went
/// through `Value::PartialEq`, which is strict per variant because it has to
/// agree with `Value::Hash`, while the four ordered operators went through
/// `Value::Ord`, which promotes across int and float.
///
/// Now the comparison operators all read the same order
/// (`eval::cross_type_numeric_cmp`), so the two values are simply equal. Both
/// physical paths agree, so this is a semantic property rather than a path
/// divergence, and it is asserted explicitly here so a change shows up as a
/// named failure instead of as one line inside a 700-line snapshot.
///
/// Both orientations run: the compiled leaves flip the operator when the
/// literal is on the left, and a flip that dropped a case would be invisible
/// from one side alone.
#[test]
fn int_and_float_comparison_is_a_consistent_total_order() {
    let (mut engine, _dir) = build(false);
    // `.c_int` holds 1, so against 1.0 the row matches =, <= and >= and no
    // other operator: exactly the table "these two values are equal".
    let column_first: Vec<String> = ["=", "!=", "<", "<=", ">", ">="]
        .iter()
        .map(|operator| {
            cell(
                &mut engine,
                &format!("count(X filter .c_int {operator} 1.0)"),
            )
        })
        .collect();
    assert_eq!(
        column_first,
        vec!["1", "0", "0", "1", "0", "1"],
        "an int column and an equal float literal must compare equal under `=` and \
         under both non-strict inequalities, and unequal under nothing"
    );

    // The mirrored spelling, and the mirrored types: a float column against an
    // equal int literal.
    let literal_first: Vec<String> = ["=", "!=", "<", "<=", ">", ">="]
        .iter()
        .map(|operator| {
            cell(
                &mut engine,
                &format!("count(X filter 1.0 {operator} .c_int)"),
            )
        })
        .collect();
    assert_eq!(literal_first, column_first, "the order must be symmetric");

    let float_column: Vec<String> = ["=", "!=", "<", "<=", ">", ">="]
        .iter()
        .map(|operator| {
            cell(
                &mut engine,
                &format!("count(X filter .c_float {operator} 1)"),
            )
        })
        .collect();
    assert_eq!(
        float_column, column_first,
        "a float column against an equal int literal must answer the same table as \
         an int column against an equal float literal"
    );
}

/// A minimal line-level report, so a failure names the cells that moved instead
/// of dumping several hundred identical lines.
fn first_difference(expected: &str, actual: &str) -> String {
    let mut report = String::new();
    let expected_lines: Vec<&str> = expected.lines().collect();
    let actual_lines: Vec<&str> = actual.lines().collect();
    let mut shown = 0;
    for index in 0..expected_lines.len().max(actual_lines.len()) {
        let before = expected_lines.get(index).copied().unwrap_or("<missing>");
        let after = actual_lines.get(index).copied().unwrap_or("<missing>");
        if before != after {
            report.push_str(&format!("line {}:\n  -{before}\n  +{after}\n", index + 1));
            shown += 1;
            if shown == 20 {
                report.push_str("  ... further differences suppressed\n");
                break;
            }
        }
    }
    report
}
