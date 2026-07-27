//! v0.21.0 "Correct or Error", lane A: join-scope column resolution and
//! arithmetic that used to answer with a missing value.
//!
//! Two shipped silent-wrong-answer classes are pinned here:
//!
//! 1. An unqualified column inside a join. Validation accepted `.name` because
//!    it inserted the bare half of every `alias.field` scan column into the
//!    known set, justified by a runtime suffix match that did not exist: both
//!    `eval_expr` and the join key extractor matched EXACTLY and fell through
//!    to `Value::Empty`, so `User join Order on ... { .name, .amount }`
//!    returned a row of NULLs while the qualified spelling worked. The sort
//!    keys resolved exact-only too, on both the read-write and the readonly
//!    path, so `order .amount` called a column missing that the same query
//!    projects one clause later.
//!
//!    The guard added for that ambiguity was itself bypassable: it excluded
//!    every rebound name from the ambiguous set across the whole plan, so a
//!    projection alias that reused the name switched the check off for a
//!    SIBLING field of the same projection and `{ name: Cust.name, .name }`
//!    resolved by suffix match again.
//!
//! 2. Arithmetic the evaluator has no arm for. `.ts + 1` on a datetime column,
//!    `.id + "x"`, and `.n / 0` all produced `Value::Empty` instead of a typed
//!    error. `date_add` multiplied and added unchecked, so a large literal
//!    amount aborted the process under overflow checks and wrapped in release.
//!    Of those, only a wrong-typed operand is a TYPE error; a zero divisor and
//!    an overflowing `date_add` amount are well typed and simply have no
//!    answer, so their messages must not claim a type mismatch.
//!
//! What is deliberately NOT pinned here: a per-row computed missing value
//! landing in a `required` column. Refusing it inside `coerce_value` looked
//! like a fix and was a data-corruption regression, because coercion runs per
//! row inside the expression-update write loop and a mid-loop error still
//! commits the rows written before it. `an_update_that_misses_on_one_row_is_not_torn`
//! guards the revert; the real fix is statement-level atomicity.

use powdb_query::executor::Engine;
use powdb_query::result::{QueryError, QueryResult};
use powdb_storage::types::Value;
use std::sync::atomic::{AtomicU64, Ordering};

static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_lanea_{tag}_{}_{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Two tables that share the column name `id`, so the same fixture covers both
/// the resolvable unqualified reference (`.name`, `.amount`) and the ambiguous
/// one (`.id`).
fn join_engine() -> Engine {
    let mut engine = Engine::new(&temp_dir("join")).unwrap();
    engine
        .execute_powql("type User { required id: int, required name: str }")
        .unwrap();
    engine
        .execute_powql(
            "type Order { required id: int, required user_id: int, required amount: int }",
        )
        .unwrap();
    engine
        .execute_powql("insert User { id := 1, name := \"ann\" }")
        .unwrap();
    engine
        .execute_powql("insert User { id := 2, name := \"bob\" }")
        .unwrap();
    engine
        .execute_powql("insert Order { id := 10, user_id := 1, amount := 100 }")
        .unwrap();
    engine
        .execute_powql("insert Order { id := 11, user_id := 2, amount := 200 }")
        .unwrap();
    engine
}

fn rows(engine: &mut Engine, query: &str) -> Vec<Vec<Value>> {
    match engine.execute_powql(query) {
        Ok(QueryResult::Rows { rows, .. }) => rows,
        other => panic!("expected rows from `{query}`, got {other:?}"),
    }
}

fn error(engine: &mut Engine, query: &str) -> QueryError {
    match engine.execute_powql(query) {
        Err(err) => err,
        other => panic!("expected `{query}` to be rejected, got {other:?}"),
    }
}

// ---- A1: join-scope column resolution ----

#[test]
fn unqualified_column_resolves_through_a_join() {
    let mut engine = join_engine();
    let mut result = rows(
        &mut engine,
        "User join Order on User.id = Order.user_id { .name, .amount }",
    );
    result.sort_by_key(|row| format!("{row:?}"));
    assert_eq!(
        result,
        vec![
            vec![Value::Str("ann".into()), Value::Int(100)],
            vec![Value::Str("bob".into()), Value::Int(200)],
        ],
        "an unqualified projection inside a join must read the joined columns, not NULLs"
    );
}

#[test]
fn unqualified_column_resolves_in_a_join_filter() {
    let mut engine = join_engine();
    let result = rows(
        &mut engine,
        "User join Order on User.id = Order.user_id filter .amount > 150 { User.name }",
    );
    assert_eq!(result, vec![vec![Value::Str("bob".into())]]);
}

#[test]
fn qualified_column_still_resolves_through_a_join() {
    let mut engine = join_engine();
    let mut result = rows(
        &mut engine,
        "User join Order on User.id = Order.user_id { User.name, Order.amount }",
    );
    result.sort_by_key(|row| format!("{row:?}"));
    assert_eq!(
        result,
        vec![
            vec![Value::Str("ann".into()), Value::Int(100)],
            vec![Value::Str("bob".into()), Value::Int(200)],
        ]
    );
}

#[test]
fn unqualified_column_two_joined_tables_expose_is_rejected() {
    let mut engine = join_engine();
    // Both User and Order have an `id`, so suffix resolution would silently
    // answer from whichever side the plan put first.
    let message = error(
        &mut engine,
        "User join Order on User.id = Order.user_id { .id }",
    )
    .to_string();
    assert!(
        message.contains("cannot resolve column 'id'") && message.contains("qualify"),
        "ambiguity must be a typed, actionable error, got: {message}"
    );
    // A self-join is the same conflict under two aliases of one table.
    let message = error(
        &mut engine,
        "User as a join User as b on a.id = b.id { .name }",
    )
    .to_string();
    assert!(
        message.contains("cannot resolve column 'name'"),
        "a self-join must reject the bare name too, got: {message}"
    );
}

/// `User` and `Order` both expose `name` here, so the fixture can ask the same
/// ambiguous `.name` with and without a sibling projection alias of that name.
fn shared_name_engine() -> Engine {
    let mut engine = Engine::new(&temp_dir("sharedname")).unwrap();
    engine
        .execute_powql("type Cust { required id: int, required name: str }")
        .unwrap();
    engine
        .execute_powql(
            "type Ord { required id: int, required cust_id: int, required name: str, \
             required amount: int }",
        )
        .unwrap();
    engine
        .execute_powql("insert Cust { id := 1, name := \"ann\" }")
        .unwrap();
    engine
        .execute_powql("insert Ord { id := 10, cust_id := 1, name := \"widget\", amount := 5 }")
        .unwrap();
    engine
}

/// A projection alias must not switch the ambiguity guard off for a DIFFERENT
/// field of the same projection.
///
/// The guard excluded every rebound name from the ambiguous set for the whole
/// plan, so `{ name: Cust.name, .name }` cleared `name` and the bare `.name`
/// beside it fell back to suffix resolution and silently answered "ann" (the
/// first side of the join) while the identical `{ .name }` was correctly
/// refused. A projection alias names a column of the RESULT; it never makes a
/// join-input reference resolvable, so it cannot vouch for one.
#[test]
fn a_projection_alias_does_not_disable_ambiguity_for_a_sibling_field() {
    let mut engine = shared_name_engine();
    let bare = error(
        &mut engine,
        "Cust join Ord on Cust.id = Ord.cust_id { .name }",
    )
    .to_string();
    assert!(
        bare.contains("cannot resolve column 'name'") && bare.contains("qualify"),
        "got: {bare}"
    );
    // Same reference, same plan, plus an alias that happens to reuse the name.
    let aliased = error(
        &mut engine,
        "Cust join Ord on Cust.id = Ord.cust_id { name: Cust.name, .name }",
    )
    .to_string();
    assert_eq!(
        aliased, bare,
        "an alias of the same name must not make the sibling `.name` resolvable"
    );
    // An alias with an unrelated spelling never suppressed it, and must not start.
    let renamed = error(
        &mut engine,
        "Cust join Ord on Cust.id = Ord.cust_id { nm: Cust.name, .name }",
    )
    .to_string();
    assert_eq!(renamed, bare);
    // The qualified spellings the error tells the caller to write still work.
    assert_eq!(
        rows(
            &mut engine,
            "Cust join Ord on Cust.id = Ord.cust_id { name: Cust.name, item: Ord.name }"
        ),
        vec![vec![Value::Str("ann".into()), Value::Str("widget".into())]]
    );
}

/// The exclusion the sibling-field fix narrows still has to hold for the case
/// it was added for: a `group` key binds a real output column, and the grouped
/// resolver reports bare-name ambiguity itself with a message that names both
/// candidates. Validation must keep deferring to that richer error rather than
/// pre-empting it with the generic one.
#[test]
fn an_ambiguous_group_key_still_gets_the_candidate_naming_error() {
    let mut engine = shared_name_engine();
    let message = error(
        &mut engine,
        "Cust as c join Ord as o on c.id = o.cust_id group .name { .name, n: count(*) }",
    )
    .to_string();
    assert!(
        message.contains("ambiguous") && message.contains("c.name") && message.contains("o.name"),
        "the grouped path must still name the candidates, got: {message}"
    );
}

#[test]
fn unqualified_join_key_uses_the_hash_join_path() {
    // The nested-loop fallback is capped at one candidate pair here, so the
    // 2x2 join can only succeed if the bare `.user_id` resolves to an equi-key
    // and takes the hash path (`plan_exec/join.rs::resolve_side_column`).
    let mut engine = join_engine();
    engine.set_nested_loop_pair_limit(1);
    let mut result = rows(
        &mut engine,
        "User join Order on User.id = .user_id { User.name, Order.amount }",
    );
    result.sort_by_key(|row| format!("{row:?}"));
    assert_eq!(
        result,
        vec![
            vec![Value::Str("ann".into()), Value::Int(100)],
            vec![Value::Str("bob".into()), Value::Int(200)],
        ]
    );
}

#[test]
fn unknown_column_in_a_join_is_still_rejected() {
    let mut engine = join_engine();
    let message = error(
        &mut engine,
        "User join Order on User.id = Order.user_id { .nope }",
    )
    .to_string();
    assert!(
        message.contains("column 'nope' not found"),
        "got: {message}"
    );
}

// ---- A2: arithmetic ----

fn arithmetic_engine() -> Engine {
    let mut engine = Engine::new(&temp_dir("arith")).unwrap();
    engine
        .execute_powql(
            "type Ev { required id: int, required ts: datetime, required n: int, zero: int, note: str }",
        )
        .unwrap();
    engine
        .execute_powql(
            "insert Ev { id := 1, ts := 1700000000000000, n := 8, zero := 0, note := \"hi\" }",
        )
        .unwrap();
    engine
}

#[test]
fn arithmetic_on_a_datetime_column_is_rejected() {
    let mut engine = arithmetic_engine();
    for query in [
        "Ev { .ts + 1 }",
        "Ev { 1 + .ts }",
        "Ev { .ts - 86400000000 }",
        "Ev filter .ts * 2 > 0 { .id }",
    ] {
        let message = error(&mut engine, query).to_string();
        assert!(
            message.contains("not defined for datetime") && message.contains("date_add"),
            "`{query}` must name the type and the supported spelling, got: {message}"
        );
    }
}

#[test]
fn arithmetic_on_a_non_numeric_operand_is_rejected() {
    let mut engine = arithmetic_engine();
    for (query, expected) in [
        ("Ev { .id + \"x\" }", "not defined for str"),
        ("Ev { .note + 1 }", "not defined for str"),
        ("Ev { .id * true }", "not defined for bool"),
    ] {
        let message = error(&mut engine, query).to_string();
        assert!(
            message.contains(expected),
            "`{query}` should report `{expected}`, got: {message}"
        );
    }
}

#[test]
fn arithmetic_on_an_unqualified_join_column_is_typed_too() {
    // Now that the unqualified spelling resolves at runtime, validation has to
    // resolve it the same way, or it would type-check nothing inside a join.
    let mut engine = Engine::new(&temp_dir("joinarith")).unwrap();
    engine
        .execute_powql("type Ev { required id: int, required ts: datetime }")
        .unwrap();
    engine
        .execute_powql("type Tag { required id: int, required ev_id: int }")
        .unwrap();
    engine
        .execute_powql("insert Ev { id := 1, ts := 1700000000000000 }")
        .unwrap();
    engine
        .execute_powql("insert Tag { id := 5, ev_id := 1 }")
        .unwrap();
    let message = error(&mut engine, "Ev join Tag on Ev.id = Tag.ev_id { .ts + 1 }").to_string();
    assert!(
        message.contains("not defined for datetime"),
        "got: {message}"
    );
}

#[test]
fn division_by_a_literal_zero_is_rejected() {
    let mut engine = arithmetic_engine();
    // Run the non-zero spelling first: the plan cache keys on the canonical
    // shape and substitutes literals at lookup, so the guard has to run on the
    // substituted plan, not once when the plan is first compiled.
    assert_eq!(
        rows(&mut engine, "Ev { .n / 2 }"),
        vec![vec![Value::Int(4)]]
    );
    for query in ["Ev { .n / 0 }", "Ev filter .n / 0 > 1 { .id }"] {
        // Byte-exact: a zero divisor is well typed and simply has no answer, so
        // the message must not claim a type mismatch. The `cannot` lead also
        // keeps it inside the server's egress allowlist.
        assert_eq!(
            error(&mut engine, query).to_string(),
            "cannot divide by zero: the divisor is the literal 0",
            "query: {query}"
        );
    }
}

#[test]
fn date_add_with_an_overflowing_literal_amount_is_rejected() {
    let mut engine = arithmetic_engine();
    // Unchecked, this multiply aborted the process under overflow checks and
    // wrapped to a bogus timestamp in release. An amount too large for its unit
    // is an overflow, not a type mismatch, so the message says so.
    assert_eq!(
        error(
            &mut engine,
            "Ev { date_add(.ts, 9223372036854775807, \"day\") }",
        )
        .to_string(),
        "cannot compute date_add: amount 9223372036854775807 overflows the representable \
         range in units of 'day'"
    );
}

#[test]
fn date_arithmetic_never_overflows_on_stored_values() {
    let mut engine = Engine::new(&temp_dir("dtovf")).unwrap();
    engine
        .execute_powql("type T { required id: int, required ts: datetime, big: int }")
        .unwrap();
    engine
        .execute_powql(
            "insert T { id := 1, ts := -9223372036854775808, big := -9223372036854775808 }",
        )
        .unwrap();
    engine
        .execute_powql("insert T { id := 2, ts := 9223372036854775807, big := 1 }")
        .unwrap();
    // A per-row amount cannot be typed before execution, so the unit multiply
    // must be checked at runtime rather than panicking.
    assert_eq!(
        rows(
            &mut engine,
            "T filter .id = 1 { date_add(.ts, .big, \"day\") }"
        ),
        vec![vec![Value::Empty]]
    );
    // date_diff subtracts two stored timestamps a full i64 range apart.
    assert_eq!(
        rows(
            &mut engine,
            "T { date_diff(9223372036854775807, .ts, \"us\") }"
        ),
        vec![vec![Value::Empty], vec![Value::Int(0)]]
    );
    // abs has no representable result for i64::MIN.
    assert_eq!(
        rows(&mut engine, "T { abs(.big) }"),
        vec![vec![Value::Empty], vec![Value::Int(1)]]
    );
}

#[test]
fn numeric_arithmetic_is_unchanged() {
    let mut engine = arithmetic_engine();
    assert_eq!(
        rows(&mut engine, "Ev { .n + 1, .n - 1, .n * 2, .n / 2 }"),
        vec![vec![
            Value::Int(9),
            Value::Int(7),
            Value::Int(16),
            Value::Int(4)
        ]]
    );
    assert_eq!(
        rows(&mut engine, "Ev { .n / .zero }"),
        vec![vec![Value::Empty]],
        "a per-row zero divisor still yields the empty set in a projection"
    );
    assert_eq!(
        rows(
            &mut engine,
            "Ev { date_add(.ts, 1, \"day\"), date_diff(.ts, 0, \"day\") }"
        ),
        vec![vec![Value::DateTime(1700086400000000), Value::Int(19675)]]
    );
}

// ---- A3: statement atomicity and sort-key resolution ----

/// The expression-update loop writes each row as it goes and every non-transactional
/// statement commits when it returns, error or not. A mid-loop `?` therefore leaves a
/// TORN, durably committed update: the rows before the failure keep their new values,
/// the rows after it are never visited. Any per-row rejection added to the write path
/// buys a cleaner error message at the price of corrupting the table, so the write
/// path must not reject per-row.
#[test]
fn an_update_that_misses_on_one_row_is_not_torn() {
    let mut engine = Engine::new(&temp_dir("torn")).unwrap();
    engine
        .execute_powql("type T { required id: int, required n: int, required z: int }")
        .unwrap();
    for (id, n, z) in [(1, 10, 1), (2, 20, 2), (3, 30, 0), (4, 40, 4)] {
        engine
            .execute_powql(&format!("insert T {{ id := {id}, n := {n}, z := {z} }}"))
            .unwrap();
    }
    // Row 3 divides by a per-row zero, which the evaluator answers with the
    // empty set. Every row must still be visited and the statement must not
    // stop halfway through.
    assert!(
        matches!(
            engine.execute_powql("T update { n := .n / .z }"),
            Ok(QueryResult::Modified(4))
        ),
        "the update must run to completion rather than tearing at row 3"
    );
    // Rows 1, 2 and 4 carry their computed value; row 3's `required n` holds a
    // missing value, which is a known pre-existing gap (a per-row computed
    // miss landing in a required column). Refusing it here is what tore the
    // table, so the fix belongs with statement-level atomicity, not here.
    assert_eq!(
        rows(&mut engine, "T order .id { .id, .n }"),
        vec![
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(2), Value::Int(10)],
            vec![Value::Int(3), Value::Empty],
            vec![Value::Int(4), Value::Int(10)],
        ]
    );
}

/// An explicit `null` for an `auto` column means "give me the next sequence
/// value", exactly like omitting the column. Rejecting the missing value before
/// the sequence runs broke the spelling every driver that binds every column
/// emits.
#[test]
fn an_explicit_null_into_a_required_auto_column_uses_the_sequence() {
    let mut engine = Engine::new(&temp_dir("autonull")).unwrap();
    engine
        .execute_powql("type A { required auto id: int, required name: str }")
        .unwrap();
    assert_eq!(
        rows(
            &mut engine,
            "insert A { id := null, name := \"x\" } returning"
        ),
        vec![vec![Value::Int(1), Value::Str("x".into())]]
    );
    // The sequence keeps advancing across spellings.
    assert_eq!(
        rows(&mut engine, "insert A { name := \"y\" } returning"),
        vec![vec![Value::Int(2), Value::Str("y".into())]]
    );
    assert_eq!(
        rows(
            &mut engine,
            "insert A { id := null, name := \"z\" } returning"
        ),
        vec![vec![Value::Int(3), Value::Str("z".into())]]
    );
}

#[test]
fn unqualified_sort_key_resolves_through_a_join() {
    let mut engine = join_engine();
    // `.amount` resolves in the projection, the filter and the join key, so an
    // `order` clause that calls the same column unknown is a false statement
    // about a column the very next clause reads.
    assert_eq!(
        rows(
            &mut engine,
            "User join Order on User.id = Order.user_id order .amount desc { .name }"
        ),
        vec![
            vec![Value::Str("bob".into())],
            vec![Value::Str("ann".into())],
        ]
    );
    // The qualified spelling has always worked and must keep working.
    assert_eq!(
        rows(
            &mut engine,
            "User join Order on User.id = Order.user_id order Order.amount { .name }"
        ),
        vec![
            vec![Value::Str("ann".into())],
            vec![Value::Str("bob".into())],
        ]
    );
}

#[test]
fn unqualified_sort_key_resolves_through_a_join_with_a_limit() {
    let mut engine = join_engine();
    assert_eq!(
        rows(
            &mut engine,
            "User join Order on User.id = Order.user_id order .amount desc limit 1 { .name }"
        ),
        vec![vec![Value::Str("bob".into())]]
    );
}

#[test]
fn unqualified_sort_key_resolves_through_a_join_readonly() {
    let engine = join_engine();
    let sorted = engine
        .execute_powql_readonly(
            "User join Order on User.id = Order.user_id order .amount desc { .name }",
        )
        .expect("readonly sort over an unqualified join column");
    match sorted {
        QueryResult::Rows { rows, .. } => assert_eq!(
            rows,
            vec![
                vec![Value::Str("bob".into())],
                vec![Value::Str("ann".into())],
            ]
        ),
        other => panic!("expected rows, got {other:?}"),
    }
    let limited = engine
        .execute_powql_readonly(
            "User join Order on User.id = Order.user_id order .amount desc limit 1 { .name }",
        )
        .expect("readonly sort+limit over an unqualified join column");
    match limited {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows, vec![vec![Value::Str("bob".into())]])
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn an_unknown_sort_key_is_still_rejected() {
    let mut engine = join_engine();
    let message = error(
        &mut engine,
        "User join Order on User.id = Order.user_id order .nope { .name }",
    )
    .to_string();
    assert!(
        message.contains("column 'nope' not found"),
        "got: {message}"
    );
    // Suffix resolution takes the first hit, so a bare name two aliases expose
    // would silently sort by whichever side the plan put first. The sort key
    // has to reach the same ambiguity check the other clauses do.
    let message = error(
        &mut engine,
        "User join Order on User.id = Order.user_id order .id { .name }",
    )
    .to_string();
    assert!(
        message.contains("cannot resolve column 'id'") && message.contains("qualify"),
        "an ambiguous sort key must be a typed error, got: {message}"
    );
}
