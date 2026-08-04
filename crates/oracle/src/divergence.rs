//! The known-divergence ledger.
//!
//! A ledger of accepted differences is one bad day away from becoming a
//! suppression list, so this one is deliberately hostile to rot:
//!
//! 1. Every entry must name a `reason` and an `owner` of the form
//!    `path/to/file.rs:LINE`. The path must exist in the repository and the
//!    line must be inside it. A divergence nobody owns cannot be recorded.
//!    An entry may also name an `anchor`: text that must appear on that line.
//!    A bare line number only rots silently (it stays in range while the code
//!    moves out from under it), so an anchor is what makes the owner keep
//!    naming the same code rather than the same coordinate.
//! 2. Every entry must match at least one divergence in the run. An entry that
//!    no longer matches anything is a hard failure, not a comment: either the
//!    behaviour changed and the entry must go, or the shape it names was
//!    renamed and the entry is now excusing nothing while looking like it
//!    excuses something.
//! 3. Every divergence must match an entry. An unexplained divergence fails.
//! 4. The file format is parsed strictly: an unknown key, a missing key, a
//!    duplicate id or an unparsable line is an error. There is no lenient
//!    path that could swallow a typo and silently widen an entry.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// Which two engines disagreed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pair {
    /// PowQL against PowDB's own SQL frontend. Both are PowDB: a difference
    /// here is always an engine bug, never a dialect opinion.
    PowqlVsPowdbSql,
    /// PowQL against SQLite. The differential proper.
    PowqlVsSqlite,
}

impl Pair {
    pub fn key(self) -> &'static str {
        match self {
            Pair::PowqlVsPowdbSql => "powql_vs_powdb_sql",
            Pair::PowqlVsSqlite => "powql_vs_sqlite",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "powql_vs_powdb_sql" => Some(Pair::PowqlVsPowdbSql),
            "powql_vs_sqlite" => Some(Pair::PowqlVsSqlite),
            _ => None,
        }
    }
}

impl fmt::Display for Pair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

/// One observed difference.
#[derive(Debug, Clone)]
pub struct Divergence {
    pub fixture: &'static str,
    pub shape: &'static str,
    pub pair: Pair,
    pub query: String,
    pub detail: String,
}

impl fmt::Display for Divergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}/{}/{}] {}\n    query: {}",
            self.fixture, self.shape, self.pair, self.detail, self.query
        )
    }
}

/// One recorded, accepted divergence.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub shape: String,
    pub pair: Pair,
    /// Optional: restrict the entry to a single fixture. Omitted means the
    /// entry applies to every fixture, which is correct for a semantic rule
    /// but wrong for a data-dependent artefact, so shapes that need it say so.
    pub fixture: Option<String>,
    pub reason: String,
    pub owner: String,
    /// Optional: text that must appear on the `owner` line.
    ///
    /// `owner` alone rots silently. A line number stays *valid* while the code
    /// around it moves, so an anchor that used to name the LIKE matcher can end
    /// up naming an unrelated `BinOp::And` arm with every check still green.
    /// An anchor turns that into a hard failure and, because the check reports
    /// where the text actually went, into a one-line fix.
    pub anchor: Option<String>,
}

impl Entry {
    fn matches(&self, d: &Divergence) -> bool {
        self.shape == d.shape
            && self.pair == d.pair
            && self.fixture.as_ref().is_none_or(|f| f == d.fixture)
    }
}

/// Parse the ledger. Strict: any deviation from the expected shape is an
/// error, so a mistyped key silently widening an entry is impossible.
pub fn parse(text: &str) -> Result<Vec<Entry>, String> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut current: Option<BTreeMap<String, String>> = None;

    let finish = |current: Option<BTreeMap<String, String>>,
                  entries: &mut Vec<Entry>|
     -> Result<(), String> {
        let Some(fields) = current else { return Ok(()) };
        let get = |k: &str| -> Result<String, String> {
            fields
                .get(k)
                .cloned()
                .ok_or_else(|| format!("entry is missing `{k}`"))
        };
        let id = get("id")?;
        let pair_text = get("pair")?;
        let pair = Pair::parse(&pair_text)
            .ok_or_else(|| format!("entry `{id}` has unknown pair `{pair_text}`"))?;
        let entry = Entry {
            shape: get("shape")?,
            pair,
            fixture: fields.get("fixture").cloned(),
            reason: get("reason")?,
            owner: get("owner")?,
            anchor: fields.get("anchor").cloned(),
            id,
        };
        if entry.reason.trim().is_empty() {
            return Err(format!("entry `{}` has an empty reason", entry.id));
        }
        if entries.iter().any(|e| e.id == entry.id) {
            return Err(format!("duplicate entry id `{}`", entry.id));
        }
        // At most one entry per (shape, pair, fixture). Adjudication takes the
        // first match, so a second entry on the same key could never match and
        // would fail the staleness check anyway; rejecting it here says why.
        // It also forces shapes to stay narrow: two accepted reasons for one
        // shape means the shape has to be split, not the ledger widened.
        if let Some(clash) = entries
            .iter()
            .find(|e| e.shape == entry.shape && e.pair == entry.pair && e.fixture == entry.fixture)
        {
            return Err(format!(
                "entries `{}` and `{}` both claim shape `{}` / pair `{}` / fixture {:?}; \
                 split the shape instead of stacking entries",
                clash.id, entry.id, entry.shape, entry.pair, entry.fixture
            ));
        }
        entries.push(entry);
        Ok(())
    };

    const KNOWN_KEYS: [&str; 7] = [
        "id", "shape", "pair", "fixture", "reason", "owner", "anchor",
    ];

    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[divergence]]" {
            finish(current.take(), &mut entries)?;
            current = Some(BTreeMap::new());
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!(
                "line {}: expected `key = \"value\"`, got `{line}`",
                lineno + 1
            ));
        };
        let key = key.trim().to_string();
        if !KNOWN_KEYS.contains(&key.as_str()) {
            return Err(format!("line {}: unknown key `{key}`", lineno + 1));
        }
        let value = unquote(value.trim())
            .ok_or_else(|| format!("line {}: value must be a quoted string", lineno + 1))?;
        let Some(fields) = current.as_mut() else {
            return Err(format!(
                "line {}: `{key}` appears before any [[divergence]]",
                lineno + 1
            ));
        };
        if fields.insert(key.clone(), value).is_some() {
            return Err(format!(
                "line {}: duplicate key `{key}` in one entry",
                lineno + 1
            ));
        }
    }
    finish(current.take(), &mut entries)?;
    Ok(entries)
}

fn unquote(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(chars.next()?);
        } else if c == '"' {
            // An unescaped quote inside the value means the line was not a
            // single string.
            return None;
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// The repository root, derived from this crate's manifest directory.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Check that every entry's `owner` names a real `path:line` in the repo.
///
/// This is what stops `owner` from decaying into decoration: if the code that
/// owns a divergence is deleted or moved, the ledger stops loading.
pub fn check_owners(entries: &[Entry], root: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    for entry in entries {
        let Some((path, line_text)) = entry.owner.rsplit_once(':') else {
            problems.push(format!(
                "entry `{}`: owner `{}` is not `path:line`",
                entry.id, entry.owner
            ));
            continue;
        };
        let Ok(line) = line_text.parse::<usize>() else {
            problems.push(format!(
                "entry `{}`: owner line `{line_text}` is not a number",
                entry.id
            ));
            continue;
        };
        let full = root.join(path);
        let Ok(contents) = std::fs::read_to_string(&full) else {
            problems.push(format!(
                "entry `{}`: owner file `{path}` does not exist",
                entry.id
            ));
            continue;
        };
        let total = contents.lines().count();
        if line == 0 || line > total {
            problems.push(format!(
                "entry `{}`: owner line {line} is outside `{path}` ({total} lines)",
                entry.id
            ));
            continue;
        }
        let Some(anchor) = entry.anchor.as_deref() else {
            continue;
        };
        // `line` is 1-based and in range, so this index exists.
        if contents
            .lines()
            .nth(line - 1)
            .is_some_and(|l| l.contains(anchor))
        {
            continue;
        }
        // Naming where the text went turns a rotted anchor into a one-line fix
        // instead of a hunt.
        let moved_to: Vec<usize> = contents
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains(anchor))
            .map(|(i, _)| i + 1)
            .take(4)
            .collect();
        let hint = if moved_to.is_empty() {
            format!("and it appears nowhere in `{path}`")
        } else {
            format!("it is now at line(s) {moved_to:?}")
        };
        problems.push(format!(
            "entry `{}`: owner `{path}:{line}` does not contain anchor `{anchor}`; {hint}",
            entry.id
        ));
    }
    problems
}

/// The verdict of one oracle run.
pub struct Verdict {
    pub unexplained: Vec<Divergence>,
    /// Ids of entries that matched nothing in this run: the ledger has rotted.
    pub stale_entries: Vec<String>,
    /// (entry id, number of divergences it covered).
    pub matched: Vec<(String, usize)>,
}

impl Verdict {
    pub fn is_clean(&self) -> bool {
        self.unexplained.is_empty() && self.stale_entries.is_empty()
    }
}

/// Split observed divergences into explained and unexplained, and find
/// entries that no longer match anything.
pub fn adjudicate(divergences: &[Divergence], entries: &[Entry]) -> Verdict {
    let mut hits: Vec<usize> = vec![0; entries.len()];
    let mut unexplained = Vec::new();

    for d in divergences {
        match entries.iter().position(|e| e.matches(d)) {
            Some(idx) => hits[idx] += 1,
            None => unexplained.push(d.clone()),
        }
    }

    let stale_entries = entries
        .iter()
        .zip(&hits)
        .filter(|(_, n)| **n == 0)
        .map(|(e, _)| e.id.clone())
        .collect();
    let matched = entries
        .iter()
        .zip(&hits)
        .filter(|(_, n)| **n > 0)
        .map(|(e, n)| (e.id.clone(), *n))
        .collect();

    Verdict {
        unexplained,
        stale_entries,
        matched,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn div(fixture: &'static str, shape: &'static str, pair: Pair) -> Divergence {
        Divergence {
            fixture,
            shape,
            pair,
            query: "q".into(),
            detail: "d".into(),
        }
    }

    const SAMPLE: &str = r#"
# a comment
[[divergence]]
id = "one"
shape = "filter_not"
pair = "powql_vs_sqlite"
reason = "two-valued logic"
owner = "docs/SQL.md:78"
"#;

    #[test]
    fn parses_a_well_formed_entry() {
        let entries = parse(SAMPLE).expect("parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "one");
        assert_eq!(entries[0].pair, Pair::PowqlVsSqlite);
        assert_eq!(entries[0].fixture, None);
    }

    #[test]
    fn rejects_a_missing_required_key() {
        let text = SAMPLE.replace("reason = \"two-valued logic\"\n", "");
        assert!(parse(&text).is_err(), "a missing reason must not parse");
        let text = SAMPLE.replace("owner = \"docs/SQL.md:78\"\n", "");
        assert!(parse(&text).is_err(), "a missing owner must not parse");
    }

    #[test]
    fn rejects_an_unknown_key() {
        let text = format!("{SAMPLE}note = \"sneaky\"\n");
        assert!(parse(&text).is_err());
    }

    #[test]
    fn rejects_an_empty_reason() {
        let text = SAMPLE.replace("two-valued logic", "   ");
        assert!(parse(&text).is_err());
    }

    #[test]
    fn rejects_a_duplicate_id() {
        let text = format!("{SAMPLE}{SAMPLE}");
        assert!(parse(&text).is_err());
    }

    #[test]
    fn rejects_an_unknown_pair() {
        let text = SAMPLE.replace("powql_vs_sqlite", "powql_vs_postgres");
        assert!(parse(&text).is_err());
    }

    #[test]
    fn rejects_a_key_before_any_entry_header() {
        assert!(parse("id = \"x\"\n").is_err());
    }

    #[test]
    fn an_unmatched_divergence_is_unexplained() {
        let entries = parse(SAMPLE).expect("parse");
        let v = adjudicate(
            &[div("boundary", "filter_cmp", Pair::PowqlVsSqlite)],
            &entries,
        );
        assert_eq!(v.unexplained.len(), 1);
        assert!(!v.is_clean());
    }

    #[test]
    fn an_entry_that_matches_nothing_is_stale() {
        let entries = parse(SAMPLE).expect("parse");
        let v = adjudicate(&[], &entries);
        assert_eq!(v.stale_entries, vec!["one".to_string()]);
        assert!(!v.is_clean());
    }

    #[test]
    fn an_entry_only_covers_its_own_shape_and_pair() {
        let entries = parse(SAMPLE).expect("parse");
        // Right shape, wrong pair.
        let v = adjudicate(
            &[div("boundary", "filter_not", Pair::PowqlVsPowdbSql)],
            &entries,
        );
        assert_eq!(
            v.unexplained.len(),
            1,
            "an entry must not cover the other pair"
        );
        // Right shape and pair.
        let v = adjudicate(
            &[div("boundary", "filter_not", Pair::PowqlVsSqlite)],
            &entries,
        );
        assert!(v.is_clean());
        assert_eq!(v.matched, vec![("one".to_string(), 1)]);
    }

    #[test]
    fn a_fixture_scoped_entry_does_not_cover_other_fixtures() {
        let text = SAMPLE.replace("reason =", "fixture = \"boundary\"\nreason =");
        let entries = parse(&text).expect("parse");
        let v = adjudicate(
            &[div("single", "filter_not", Pair::PowqlVsSqlite)],
            &entries,
        );
        assert_eq!(v.unexplained.len(), 1);
    }

    #[test]
    fn owner_must_point_at_a_real_line() {
        let root = repo_root();
        let good = parse(SAMPLE).expect("parse");
        assert!(
            check_owners(&good, &root).is_empty(),
            "docs/SQL.md:78 should exist"
        );

        let missing_file =
            parse(&SAMPLE.replace("docs/SQL.md:78", "docs/NOPE.md:1")).expect("parse");
        assert_eq!(check_owners(&missing_file, &root).len(), 1);

        let bad_line =
            parse(&SAMPLE.replace("docs/SQL.md:78", "docs/SQL.md:999999")).expect("parse");
        assert_eq!(check_owners(&bad_line, &root).len(), 1);

        let no_line = parse(&SAMPLE.replace("docs/SQL.md:78", "docs/SQL.md")).expect("parse");
        assert_eq!(check_owners(&no_line, &root).len(), 1);
    }

    /// The anchor is what stops an owner from rotting into a valid line number
    /// that names the wrong code. Both directions are checked: an anchor that
    /// is really there must pass, or the mechanism is only noise.
    ///
    /// The owned file is written into a temp root rather than pointed at real
    /// source, so the test asserts the behaviour of the check and cannot itself
    /// rot the way the two LIKE entries did.
    #[test]
    fn an_anchor_must_be_present_on_the_owner_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(
            root.join("owned.rs"),
            "fn first() {}\nlet decides_the_behaviour = true;\nfn third() {}\n",
        )
        .expect("write");

        let entries = |owner: &str, anchor: &str| {
            let text = SAMPLE
                .replace("docs/SQL.md:78", owner)
                .replace("reason =", &format!("anchor = \"{anchor}\"\nreason ="));
            parse(&text).expect("parse")
        };

        let good = entries("owned.rs:2", "decides_the_behaviour");
        assert!(
            check_owners(&good, root).is_empty(),
            "an anchor that is really on the line must pass"
        );

        // Still a valid line number, still the right file, wrong code: exactly
        // the rot a line-only check cannot see.
        let moved = entries("owned.rs:3", "decides_the_behaviour");
        let problems = check_owners(&moved, root);
        assert_eq!(problems.len(), 1, "a drifted anchor must be reported");
        assert!(
            problems[0].contains("line(s) [2]"),
            "the report must name where the text went: {}",
            problems[0]
        );

        let gone = entries("owned.rs:2", "deleted_long_ago");
        let problems = check_owners(&gone, root);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("appears nowhere"), "{}", problems[0]);
    }

    #[test]
    fn an_entry_without_an_anchor_is_still_accepted() {
        let entries = parse(SAMPLE).expect("parse");
        assert_eq!(entries[0].anchor, None);
        assert!(check_owners(&entries, &repo_root()).is_empty());
    }
}
