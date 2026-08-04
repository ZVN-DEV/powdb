//! The dialect-independent vocabulary the oracle compares in.
//!
//! Everything three engines produce is funnelled into [`OVal`] / [`ResultSet`]
//! before a single comparison happens, so the comparison code never has to
//! know which engine a row came from.

use std::cmp::Ordering;
use std::fmt;

/// A PowDB column type. Covers every storable [`powdb_storage::types::TypeId`];
/// the ninth `TypeId`, `Empty`, is not a column type but a per-value state, so
/// it is modelled as [`Lit::Null`] / [`OVal::Null`] and reached by making every
/// column except `id` nullable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    Int,
    Float,
    Bool,
    Str,
    DateTime,
    Uuid,
    Bytes,
    Json,
}

impl ColType {
    /// PowQL `type T { .. }` spelling.
    pub fn powql(self) -> &'static str {
        match self {
            ColType::Int => "int",
            ColType::Float => "float",
            ColType::Bool => "bool",
            ColType::Str => "str",
            ColType::DateTime => "datetime",
            ColType::Uuid => "uuid",
            ColType::Bytes => "bytes",
            ColType::Json => "json",
        }
    }

    /// SQLite `CREATE TABLE` spelling. See `normalize.rs` for why each of
    /// these is a faithful carrier for the PowDB type.
    pub fn sqlite(self) -> &'static str {
        match self {
            ColType::Int | ColType::Bool | ColType::DateTime => "INTEGER",
            ColType::Float => "REAL",
            ColType::Str | ColType::Json => "TEXT",
            ColType::Uuid | ColType::Bytes => "BLOB",
        }
    }

    /// Whether values of this type have a total order that SQLite and PowDB
    /// agree on. `Json` does not: PowDB orders by the PJ1 total order
    /// (null < false < true < number < string < array < object) while SQLite
    /// sees the canonical text and orders it bytewise. The oracle therefore
    /// never generates an ORDER BY / `<` / `>` over a json column; that is a
    /// declared coverage gap, not a suppressed divergence.
    pub fn orderable_against_sqlite(self) -> bool {
        !matches!(self, ColType::Json)
    }
}

/// One column of the oracle's fixture table.
#[derive(Debug, Clone, Copy)]
pub struct Column {
    pub name: &'static str,
    pub ty: ColType,
    /// `id` is the required, dense, unique tiebreak key. Every other column is
    /// nullable so `Empty` is reachable everywhere.
    pub required: bool,
}

/// The fixture schema. One table with every storable type, so a single
/// generated query shape can be instantiated against all of them.
///
/// There are three `str` columns on purpose, because SQLite can only
/// adjudicate LIKE over a restricted corpus and each restriction has to be
/// isolated rather than averaged together:
///
/// - `s` ([`LIKE_COLUMN`]) holds LIKE-safe text: no embedded NUL and no `%`
///   or `_` characters. The LIKE shapes that must agree with SQLite run here.
/// - `sm` ([`LIKE_META_COLUMN`]) holds text containing literal `%` and `_`.
///   Isolated because a pattern `%` that lands on a literal `%` in the text is
///   where PowDB's matcher currently goes wrong; keeping it in its own column
///   and its own shape stops that one entry from excusing every other LIKE
///   difference.
/// - `sx` holds the whole corpus including embedded NUL. SQLite's
///   `patternCompare` walks a NUL-terminated buffer, so `'a\0b' LIKE '_'` is
///   true in SQLite and false in PowDB, and PowDB is the one that is right;
///   no LIKE shape touches `sx`, while ordering, indexing, equality, grouping
///   and distinct all still exercise it in full.
pub const COLUMNS: [Column; 11] = [
    Column {
        name: "id",
        ty: ColType::Int,
        required: true,
    },
    Column {
        name: "i",
        ty: ColType::Int,
        required: false,
    },
    Column {
        name: "f",
        ty: ColType::Float,
        required: false,
    },
    Column {
        name: "b",
        ty: ColType::Bool,
        required: false,
    },
    Column {
        name: "s",
        ty: ColType::Str,
        required: false,
    },
    Column {
        name: "sm",
        ty: ColType::Str,
        required: false,
    },
    Column {
        name: "sx",
        ty: ColType::Str,
        required: false,
    },
    Column {
        name: "d",
        ty: ColType::DateTime,
        required: false,
    },
    Column {
        name: "u",
        ty: ColType::Uuid,
        required: false,
    },
    Column {
        name: "y",
        ty: ColType::Bytes,
        required: false,
    },
    Column {
        name: "j",
        ty: ColType::Json,
        required: false,
    },
];

/// The string column the ordinary LIKE shapes use. See [`COLUMNS`].
pub const LIKE_COLUMN: &str = "s";

/// The string column holding literal LIKE metacharacters. See [`COLUMNS`].
pub const LIKE_META_COLUMN: &str = "sm";

/// The table name every generated query targets.
pub const TABLE: &str = "T";

/// Look up a column by name. Returns `None` for an unknown name, which is a
/// harness bug at every call site.
pub fn column(name: &str) -> Option<&'static Column> {
    COLUMNS.iter().find(|c| c.name == name)
}

/// A literal value, in the generator's own representation.
///
/// The generator holds the *intended* value. Each engine then receives it in
/// whatever form that engine can accept exactly: PowDB gets literal text
/// (there is no other way to write a PowQL literal), SQLite gets a bound
/// parameter. If PowDB's literal syntax cannot carry the value losslessly the
/// oracle sees it as a divergence, which is the point.
#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    DateTime(i64),
    Uuid([u8; 16]),
    Bytes(Vec<u8>),
    /// Canonical JSON *text*. The generator only ever emits already-canonical
    /// text (keys sorted bytewise, no insignificant whitespace) so that PowDB's
    /// canonical rendering and SQLite's verbatim text are directly comparable.
    Json(String),
}

impl Lit {
    pub fn is_null(&self) -> bool {
        matches!(self, Lit::Null)
    }

    /// PowQL literal text, or `None` when PowQL has no lossless spelling.
    ///
    /// `None` is never silently skipped: fixture construction turns it into a
    /// hard error so a value can never quietly vanish from coverage.
    pub fn powql(&self) -> Option<String> {
        Some(match self {
            // PowQL has no null literal. Nullness is expressed by omitting the
            // column on insert and by `is null` in predicates.
            Lit::Null => return None,
            Lit::Int(v) | Lit::DateTime(v) => v.to_string(),
            Lit::Float(v) => plain_decimal(*v)?,
            Lit::Bool(v) => v.to_string(),
            Lit::Str(s) => powdb_quoted(s, '"'),
            Lit::Uuid(u) => format!("uuid({})", powdb_quoted(&hyphenated(u), '"')),
            Lit::Bytes(b) => format!("bytes({})", powdb_quoted(&backslash_hex(b), '"')),
            Lit::Json(t) => powdb_quoted(t, '"'),
        })
    }

    /// PowDB-SQL literal text. Same escape rules as PowQL (the SQL frontend
    /// shares the backslash-escape convention), single-quoted.
    pub fn powdb_sql(&self) -> Option<String> {
        Some(match self {
            Lit::Null => "NULL".to_string(),
            Lit::Int(v) | Lit::DateTime(v) => v.to_string(),
            Lit::Float(v) => plain_decimal(*v)?,
            Lit::Bool(v) => v.to_string(),
            Lit::Str(s) => powdb_quoted(s, '\''),
            Lit::Uuid(u) => format!("uuid({})", powdb_quoted(&hyphenated(u), '\'')),
            Lit::Bytes(b) => format!("bytes({})", powdb_quoted(&backslash_hex(b), '\'')),
            Lit::Json(t) => powdb_quoted(t, '\''),
        })
    }

    /// The bound-parameter form handed to SQLite. Exact by construction: no
    /// text parsing is involved on this side at all.
    pub fn sqlite_param(&self) -> rusqlite::types::Value {
        use rusqlite::types::Value as S;
        match self {
            Lit::Null => S::Null,
            Lit::Int(v) | Lit::DateTime(v) => S::Integer(*v),
            Lit::Float(v) => S::Real(*v),
            Lit::Bool(v) => S::Integer(i64::from(*v)),
            Lit::Str(s) => S::Text(s.clone()),
            Lit::Uuid(u) => S::Blob(u.to_vec()),
            Lit::Bytes(b) => S::Blob(b.clone()),
            Lit::Json(t) => S::Text(t.clone()),
        }
    }
}

/// PowDB string-literal quoting, shared by PowQL (`"`) and PowDB-SQL (`'`).
///
/// Both lexers use the same rule: a backslash escapes the next character, with
/// `\n`/`\t` recognised and every other `\X` collapsing to `X`. So a literal
/// backslash must be doubled and the quote character must be escaped.
fn powdb_quoted(s: &str, quote: char) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Canonical hyphenated UUID text, the form `uuid("...")` parses.
pub fn hyphenated(u: &[u8; 16]) -> String {
    let mut s = String::with_capacity(36);
    for (i, byte) in u.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// `\xHEX` text, the form `bytes("...")` parses.
fn backslash_hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("\\x");
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// Render an `f64` as a plain decimal that PowQL's lexer accepts and that
/// parses back to the *same bits*.
///
/// PowQL float literals have no exponent form, so `{:?}` (which produces
/// `1.7976931348623157e308` for `f64::MAX`) is unusable. We try increasing
/// fixed precisions and return the first that round-trips bit-exactly.
/// Non-finite values return `None`: PowQL cannot express them at all.
pub fn plain_decimal(v: f64) -> Option<String> {
    if !v.is_finite() {
        return None;
    }
    for precision in [1usize, 17, 20, 40, 80, 320, 1100] {
        let text = format!("{v:.precision$}");
        if text.parse::<f64>().map(f64::to_bits) == Ok(v.to_bits()) {
            return Some(text);
        }
    }
    None
}

/// What the oracle knows about one output column of one query.
///
/// This is what makes the SQLite normalization principled rather than a guess:
/// a `Col` output is a bare reference to a declared PowDB column, so SQLite's
/// carrier type is reinterpreted as that declared type. An `Expr` output is
/// computed, so SQLite's own dynamic type is taken at face value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Col(ColType),
    Expr,
}

/// A dialect-independent value. The comparison currency.
#[derive(Debug, Clone)]
pub enum OVal {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    DateTime(i64),
    Uuid([u8; 16]),
    Bytes(Vec<u8>),
    /// Canonical JSON text.
    Json(String),
}

impl OVal {
    /// A short type tag, compared alongside the value so that `Int(1)` never
    /// silently passes for `Bool(true)` or `Float(1.0)`.
    pub fn tag(&self) -> &'static str {
        match self {
            OVal::Null => "null",
            OVal::Int(_) => "int",
            OVal::Float(_) => "float",
            OVal::Bool(_) => "bool",
            OVal::Str(_) => "str",
            OVal::DateTime(_) => "datetime",
            OVal::Uuid(_) => "uuid",
            OVal::Bytes(_) => "bytes",
            OVal::Json(_) => "json",
        }
    }

    fn rank(&self) -> u8 {
        match self {
            OVal::Null => 0,
            OVal::Int(_) => 1,
            OVal::Float(_) => 2,
            OVal::Bool(_) => 3,
            OVal::Str(_) => 4,
            OVal::DateTime(_) => 5,
            OVal::Uuid(_) => 6,
            OVal::Bytes(_) => 7,
            OVal::Json(_) => 8,
        }
    }
}

impl PartialEq for OVal {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for OVal {}

impl PartialOrd for OVal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A total order over `OVal` used *only* to canonicalise unordered result sets
/// before comparing them. It is deliberately type-tag-first and never mixes
/// types, so it can never make two differently-typed values compare equal.
/// Floats use `total_cmp`, so `-0.0` and `0.0` are distinct and NaN has a
/// stable position.
impl Ord for OVal {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (OVal::Null, OVal::Null) => Ordering::Equal,
            (OVal::Int(a), OVal::Int(b)) => a.cmp(b),
            (OVal::Float(a), OVal::Float(b)) => a.total_cmp(b),
            (OVal::Bool(a), OVal::Bool(b)) => a.cmp(b),
            (OVal::Str(a), OVal::Str(b)) => a.cmp(b),
            (OVal::DateTime(a), OVal::DateTime(b)) => a.cmp(b),
            (OVal::Uuid(a), OVal::Uuid(b)) => a.cmp(b),
            (OVal::Bytes(a), OVal::Bytes(b)) => a.cmp(b),
            (OVal::Json(a), OVal::Json(b)) => a.cmp(b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl fmt::Display for OVal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OVal::Null => write!(f, "null"),
            OVal::Int(v) => write!(f, "int:{v}"),
            OVal::Float(v) => write!(f, "float:{v:?}"),
            OVal::Bool(v) => write!(f, "bool:{v}"),
            OVal::Str(v) => write!(f, "str:{v:?}"),
            OVal::DateTime(v) => write!(f, "datetime:{v}"),
            OVal::Uuid(v) => write!(f, "uuid:{}", hyphenated(v)),
            OVal::Bytes(v) => write!(f, "bytes:{v:02x?}"),
            OVal::Json(v) => write!(f, "json:{v}"),
        }
    }
}

/// A fully normalized result set: column names plus every row, in the order
/// the engine returned them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<OVal>>,
}

/// The synthetic column name used for a scalar (single-value) result.
///
/// PowQL's scalar aggregate result carries no column name at all: the engine
/// returns `QueryResult::Scalar(v)`, not a named single-column row. There is
/// therefore nothing on the PowDB side to compare a SQLite column label
/// against, so both sides are given the same synthetic name. Values and types
/// are still compared in full; only the *name* comparison is vacuous here, and
/// only for scalar results.
pub const SCALAR_COLUMN: &str = "_scalar";

/// What one engine did with one query.
#[derive(Debug, Clone)]
pub enum Outcome {
    Rows(ResultSet),
    /// The engine rejected the query. An error on one side and rows on the
    /// other is a divergence, never a skip.
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_decimal_round_trips_extremes() {
        for v in [
            0.0f64,
            -0.0,
            1.0,
            -1.5,
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
            f64::EPSILON,
            std::f64::consts::PI,
        ] {
            let text = plain_decimal(v).unwrap_or_else(|| panic!("no plain decimal for {v:?}"));
            assert!(
                !text.contains('e') && !text.contains('E'),
                "exponent leaked into {text}"
            );
            let back: f64 = text.parse().expect("parse back");
            assert_eq!(back.to_bits(), v.to_bits(), "round trip failed for {v:?}");
        }
    }

    #[test]
    fn plain_decimal_refuses_non_finite() {
        assert_eq!(plain_decimal(f64::NAN), None);
        assert_eq!(plain_decimal(f64::INFINITY), None);
        assert_eq!(plain_decimal(f64::NEG_INFINITY), None);
    }

    #[test]
    fn powdb_quoting_escapes_backslash_and_quote() {
        assert_eq!(powdb_quoted("a\\b", '"'), "\"a\\\\b\"");
        assert_eq!(powdb_quoted("a\"b", '"'), "\"a\\\"b\"");
        assert_eq!(powdb_quoted("a'b", '\''), "'a\\'b'");
        assert_eq!(powdb_quoted("a\nb", '"'), "\"a\\nb\"");
        // A quote of the *other* flavour is left alone.
        assert_eq!(powdb_quoted("a'b", '"'), "\"a'b\"");
    }

    #[test]
    fn uuid_and_bytes_literals_are_well_formed() {
        let u = [
            0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44,
            0x00, 0x00,
        ];
        assert_eq!(hyphenated(&u), "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(
            Lit::Uuid(u).powql().expect("uuid literal"),
            "uuid(\"550e8400-e29b-41d4-a716-446655440000\")"
        );
        assert_eq!(
            Lit::Bytes(vec![0x00, 0xff, 0x10])
                .powql()
                .expect("bytes literal"),
            "bytes(\"\\\\x00ff10\")"
        );
        assert_eq!(
            Lit::Bytes(vec![]).powql().expect("empty bytes"),
            "bytes(\"\\\\x\")"
        );
    }

    #[test]
    fn oval_order_never_equates_different_types() {
        assert_ne!(OVal::Int(1), OVal::Bool(true));
        assert_ne!(OVal::Int(1), OVal::Float(1.0));
        assert_ne!(OVal::Int(0), OVal::Null);
        assert_ne!(OVal::Str("x".into()), OVal::Json("x".into()));
        assert_ne!(OVal::Float(0.0), OVal::Float(-0.0));
        assert_eq!(OVal::Float(1.5), OVal::Float(1.5));
    }
}
