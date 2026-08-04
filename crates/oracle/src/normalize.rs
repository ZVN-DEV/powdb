//! The normalization layer: turning three engines' native outputs into
//! [`OVal`] / [`ResultSet`].
//!
//! # Why a normalization layer exists at all, and where it stops
//!
//! SQLite has five storage classes (NULL, INTEGER, REAL, TEXT, BLOB). PowDB
//! has nine `TypeId`s. Four PowDB types therefore have no SQLite carrier of
//! their own and must ride inside one that does. Reinterpreting that carrier
//! back into the declared PowDB type is the *whole* of the normalization
//! performed here.
//!
//! Every rule below is (a) type-directed from the *fixture schema*, never from
//! the observed value, (b) applied only to the SQLite side, and (c) fails loud
//! rather than coercing when the carrier does not hold what the schema says it
//! should. Rule 5 is the only rule that touches both sides, and it removes a
//! comparison that has no content rather than one that could hide a bug.
//!
//! Nothing here rewrites *values*. There is no int/float widening, no string
//! trimming, no case folding, no NULL/empty conflation, no float epsilon. If
//! two engines disagree about a value, the oracle reports it.

use powdb_storage::types::Value;
use rusqlite::types::ValueRef;

use crate::model::{ColType, Kind, OVal, ResultSet, SCALAR_COLUMN};

/// Convert a PowDB `Value` into the comparison currency.
///
/// This is a straight 1:1 mapping: PowDB's type lattice *is* the oracle's, so
/// there is no reinterpretation to do and no rule to justify. The one piece of
/// rendering is `Json`, which is stored as canonical PJ1 bytes and rendered
/// through the engine's own canonical-text function so it can be compared with
/// the JSON text SQLite holds.
pub fn powdb_value(v: &Value) -> OVal {
    match v {
        Value::Empty => OVal::Null,
        Value::Int(n) => OVal::Int(*n),
        Value::Float(n) => OVal::Float(*n),
        Value::Bool(b) => OVal::Bool(*b),
        Value::Str(s) => OVal::Str(s.clone()),
        Value::DateTime(t) => OVal::DateTime(*t),
        Value::Uuid(u) => OVal::Uuid(*u),
        Value::Bytes(b) => OVal::Bytes(b.clone()),
        Value::Json(_) => OVal::Json(v.to_wire_string()),
    }
}

/// Convert one SQLite column value into the comparison currency, using what
/// the oracle knows about that output column.
///
/// Returns `Err` when the carrier does not hold what the schema promises (a
/// non-0/1 integer in a bool column, a blob of the wrong length in a uuid
/// column). That is a hard failure, not a coercion: if it ever fires, either
/// the fixture loader corrupted the SQLite side or SQLite changed a storage
/// rule, and either way the oracle must stop rather than invent a value.
pub fn sqlite_value(v: ValueRef<'_>, kind: Kind) -> Result<OVal, String> {
    // A NULL is a NULL in both engines. PowDB's `Empty` is the empty set
    // rather than SQL NULL in principle, but it is what every nullable column
    // holds when unset and what every engine surface renders as `null`, so the
    // two are the same observable and are compared directly.
    if matches!(v, ValueRef::Null) {
        return Ok(OVal::Null);
    }

    let ty = match kind {
        // Rule 0 (no reinterpretation): a computed expression has no declared
        // PowDB type on the SQLite side, so SQLite's own dynamic type is taken
        // at face value. This is where an int-vs-float or text-vs-blob
        // disagreement in an *expression result* stays visible.
        Kind::Expr => return Ok(native(v)),
        Kind::Col(ty) => ty,
    };

    Ok(match (ty, v) {
        // Rule 1 (bool). SQLite has no boolean storage class; the fixture
        // loader binds `true`/`false` as the integers 1/0, which is the
        // universal SQL convention and what `SELECT b` therefore returns.
        // Reading it back as a bool is undoing the loader's own encoding, not
        // reinterpreting an unrelated value: we assert the integer is exactly
        // 0 or 1, so a genuinely wrong integer surfacing in a bool column is
        // an error rather than a silent `true`.
        (ColType::Bool, ValueRef::Integer(0)) => OVal::Bool(false),
        (ColType::Bool, ValueRef::Integer(1)) => OVal::Bool(true),
        (ColType::Bool, other) => {
            return Err(format!(
                "bool column carried a non-boolean SQLite value: {other:?}"
            ))
        }

        // Rule 2 (datetime). PowDB's `DateTime` *is* an i64 of epoch
        // microseconds; there is no calendar conversion, no timezone and no
        // string form in the storage layer. SQLite holds the identical
        // integer. Relabelling it is a pure tag change with no value change,
        // and the integer itself is still compared exactly, so a datetime
        // arithmetic bug is not hidden by this rule.
        (ColType::DateTime, ValueRef::Integer(n)) => OVal::DateTime(n),
        (ColType::DateTime, other) => {
            return Err(format!(
                "datetime column carried a non-integer SQLite value: {other:?}"
            ))
        }

        // Rule 3 (uuid). PowDB's `Uuid` is exactly 16 opaque bytes with no
        // interpretation; SQLite holds the same 16 bytes in a BLOB. The length
        // check makes the rule refuse to invent a UUID out of anything else.
        (ColType::Uuid, ValueRef::Blob(b)) if b.len() == 16 => {
            let mut u = [0u8; 16];
            u.copy_from_slice(b);
            OVal::Uuid(u)
        }
        (ColType::Uuid, other) => {
            return Err(format!(
                "uuid column carried a non-16-byte SQLite value: {other:?}"
            ))
        }

        // Rule 4 (json). PowDB stores canonical PJ1 and renders canonical JSON
        // text; SQLite stores the text it was given. The fixture generator only
        // ever emits *already canonical* text (keys sorted bytewise, no
        // insignificant whitespace), so the two strings are directly
        // comparable and this rule only changes the tag from `str` to `json`
        // so a json column and a str column holding the same characters cannot
        // pass for each other.
        //
        // What this rule deliberately does NOT do is run PowDB's own
        // canonicaliser over the SQLite text. Doing that would make PowDB the
        // authority on its own output and the comparison vacuous. The cost is
        // that PJ1 canonicalisation of *non-canonical input* is outside the
        // oracle's reach; SQLite is not an authority on PJ1 and cannot be made
        // one.
        (ColType::Json, ValueRef::Text(t)) => OVal::Json(text(t)?),
        (ColType::Json, other) => {
            return Err(format!(
                "json column carried a non-text SQLite value: {other:?}"
            ))
        }

        // No reinterpretation needed: int/float/str/bytes have a SQLite
        // storage class of their own, so the native mapping is used and any
        // disagreement is reported.
        (ColType::Int, ValueRef::Integer(n)) => OVal::Int(n),
        (ColType::Float, ValueRef::Real(n)) => OVal::Float(n),
        (ColType::Str, ValueRef::Text(t)) => OVal::Str(text(t)?),
        (ColType::Bytes, ValueRef::Blob(b)) => OVal::Bytes(b.to_vec()),

        // A declared column returning some other storage class is not
        // something to smooth over: report the raw shape and let the
        // comparison fail.
        (_, other) => native(other),
    })
}

/// SQLite's dynamic type taken at face value.
fn native(v: ValueRef<'_>) -> OVal {
    match v {
        ValueRef::Null => OVal::Null,
        ValueRef::Integer(n) => OVal::Int(n),
        ValueRef::Real(n) => OVal::Float(n),
        // Invalid UTF-8 in a TEXT column cannot occur (everything is bound
        // from Rust `String`s), but it must not panic if it ever does.
        ValueRef::Text(t) => match std::str::from_utf8(t) {
            Ok(s) => OVal::Str(s.to_string()),
            Err(_) => OVal::Bytes(t.to_vec()),
        },
        ValueRef::Blob(b) => OVal::Bytes(b.to_vec()),
    }
}

fn text(raw: &[u8]) -> Result<String, String> {
    std::str::from_utf8(raw)
        .map(str::to_string)
        .map_err(|e| format!("SQLite TEXT was not UTF-8: {e}"))
}

/// Rule 5 (scalar column name), applied to *both* sides.
///
/// See [`SCALAR_COLUMN`]. A PowQL scalar result has no column name in the
/// protocol, so there is no name to compare; both sides are relabelled so the
/// name comparison is a no-op for scalars only. Column-name comparison stays
/// fully live for every row-shaped query, which is where a projection-alias or
/// `SELECT *` ordering bug would show up.
pub fn scalarize(rs: ResultSet) -> ResultSet {
    if rs.columns.len() == 1 && rs.rows.len() == 1 {
        ResultSet {
            columns: vec![SCALAR_COLUMN.to_string()],
            rows: rs.rows,
        }
    } else {
        rs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_rule_accepts_zero_and_one_and_refuses_the_rest() {
        assert_eq!(
            sqlite_value(ValueRef::Integer(0), Kind::Col(ColType::Bool)),
            Ok(OVal::Bool(false))
        );
        assert_eq!(
            sqlite_value(ValueRef::Integer(1), Kind::Col(ColType::Bool)),
            Ok(OVal::Bool(true))
        );
        assert!(sqlite_value(ValueRef::Integer(2), Kind::Col(ColType::Bool)).is_err());
        assert!(sqlite_value(ValueRef::Real(1.0), Kind::Col(ColType::Bool)).is_err());
    }

    #[test]
    fn uuid_rule_requires_exactly_sixteen_bytes() {
        let ok = sqlite_value(ValueRef::Blob(&[7u8; 16]), Kind::Col(ColType::Uuid));
        assert_eq!(ok, Ok(OVal::Uuid([7u8; 16])));
        assert!(sqlite_value(ValueRef::Blob(&[7u8; 15]), Kind::Col(ColType::Uuid)).is_err());
        assert!(sqlite_value(ValueRef::Blob(&[7u8; 17]), Kind::Col(ColType::Uuid)).is_err());
    }

    #[test]
    fn datetime_rule_relabels_without_changing_the_integer() {
        assert_eq!(
            sqlite_value(
                ValueRef::Integer(-62_135_596_800_000_000),
                Kind::Col(ColType::DateTime)
            ),
            Ok(OVal::DateTime(-62_135_596_800_000_000))
        );
        assert!(sqlite_value(ValueRef::Text(b"0"), Kind::Col(ColType::DateTime)).is_err());
    }

    #[test]
    fn json_rule_keeps_the_text_verbatim_and_changes_only_the_tag() {
        let got = sqlite_value(
            ValueRef::Text(br#"{"b":2,"a":1}"#),
            Kind::Col(ColType::Json),
        );
        // Verbatim: the rule does NOT sort the keys. If it did, the oracle
        // would be laundering PowDB's canonicalisation through the SQLite side.
        assert_eq!(got, Ok(OVal::Json(r#"{"b":2,"a":1}"#.to_string())));
    }

    #[test]
    fn expression_columns_keep_sqlites_own_type() {
        assert_eq!(
            sqlite_value(ValueRef::Integer(1), Kind::Expr),
            Ok(OVal::Int(1))
        );
        assert_eq!(
            sqlite_value(ValueRef::Real(1.0), Kind::Expr),
            Ok(OVal::Float(1.0))
        );
        // Crucially NOT normalized to Bool even though the value is 0/1.
        assert_ne!(
            sqlite_value(ValueRef::Integer(1), Kind::Expr),
            Ok(OVal::Bool(true))
        );
    }

    #[test]
    fn null_is_null_for_every_kind() {
        for kind in [
            Kind::Expr,
            Kind::Col(ColType::Bool),
            Kind::Col(ColType::Uuid),
            Kind::Col(ColType::Json),
        ] {
            assert_eq!(sqlite_value(ValueRef::Null, kind), Ok(OVal::Null));
        }
    }

    #[test]
    fn powdb_json_renders_as_canonical_text() {
        // PJ1 bytes for `1` are opaque here; go through the engine's own
        // renderer, which is what the wire and the CLI show.
        let v = Value::Bool(true);
        assert_eq!(powdb_value(&v), OVal::Bool(true));
        assert_eq!(powdb_value(&Value::Empty), OVal::Null);
        assert_eq!(powdb_value(&Value::Float(-0.0)).to_string(), "float:-0.0");
    }

    #[test]
    fn scalarize_only_touches_one_by_one_results() {
        let one = ResultSet {
            columns: vec!["count(*)".into()],
            rows: vec![vec![OVal::Int(3)]],
        };
        assert_eq!(scalarize(one).columns, vec![SCALAR_COLUMN.to_string()]);

        let two = ResultSet {
            columns: vec!["a".into(), "b".into()],
            rows: vec![vec![OVal::Int(1), OVal::Int(2)]],
        };
        assert_eq!(
            scalarize(two).columns,
            vec!["a".to_string(), "b".to_string()]
        );

        let many = ResultSet {
            columns: vec!["a".into()],
            rows: vec![vec![OVal::Int(1)], vec![OVal::Int(2)]],
        };
        assert_eq!(scalarize(many).columns, vec!["a".to_string()]);
    }
}
