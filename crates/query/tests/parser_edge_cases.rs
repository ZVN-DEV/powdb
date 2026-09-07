//! Parser edge cases, pinned.
//!
//! Every test in this file asserts one exact outcome: either the AST an input
//! produces, field by field, or the refusal it earns. "It does not panic" is
//! not an expectation, it is the fuzzer's job: `crates/query/fuzz` has a
//! `fuzz_parser` target whose corpus CI replays on every pull request, so
//! nothing here needs to re-state it. What a test here buys instead is that a
//! silent change of meaning fails the build.

use powdb_query::ast::{
    AggFunc, AggregateMode, AlterAction, BinOp, Expr, Literal, ProjectionField, Statement,
};
use powdb_query::parser::parse;

fn query(input: &str) -> powdb_query::ast::QueryExpr {
    match parse(input) {
        Ok(Statement::Query(q)) => q,
        other => panic!("expected a query for {input:?}, got {other:?}"),
    }
}

fn refusal(input: &str) -> String {
    match parse(input) {
        Err(error) => error.to_string(),
        Ok(stmt) => panic!("expected {input:?} to be refused, parsed as {stmt:?}"),
    }
}

fn assert_refused(input: &str, expected: &str) {
    let message = refusal(input);
    assert!(
        message.contains(expected),
        "refusal for {input:?} should mention {expected:?}, got: {message}"
    );
}

fn field(name: &str) -> Expr {
    Expr::Field(name.to_string())
}

fn int(value: i64) -> Expr {
    Expr::Literal(Literal::Int(value))
}

fn text(value: &str) -> Expr {
    Expr::Literal(Literal::String(value.to_string()))
}

fn cmp(lhs: Expr, op: BinOp, rhs: Expr) -> Expr {
    Expr::BinaryOp(Box::new(lhs), op, Box::new(rhs))
}

fn count_binops(expr: &Expr, wanted: BinOp) -> usize {
    match expr {
        Expr::BinaryOp(lhs, op, rhs) => {
            usize::from(*op == wanted) + count_binops(lhs, wanted) + count_binops(rhs, wanted)
        }
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Empty / whitespace inputs
// ---------------------------------------------------------------------------

#[test]
fn empty_input_is_refused() {
    assert_refused("", "expected statement, got end of input");
}

#[test]
fn whitespace_only_is_refused() {
    assert_refused("   \t\n  ", "expected statement, got end of input");
}

#[test]
fn comment_only_is_refused() {
    assert_refused("# just a comment", "expected statement, got end of input");
}

// ---------------------------------------------------------------------------
// Valid queries, pinned clause by clause
// ---------------------------------------------------------------------------

#[test]
fn a_plain_table_scan_carries_no_clauses() {
    let q = query("User");
    assert_eq!(q.source, "User");
    assert_eq!(q.alias, None);
    assert!(q.joins.is_empty());
    assert_eq!(q.filter, None);
    assert_eq!(q.order, None);
    assert_eq!(q.limit, None);
    assert_eq!(q.offset, None);
    assert_eq!(q.projection, None);
    assert_eq!(q.aggregation, None);
    assert!(!q.distinct);
    assert_eq!(q.group_by, None);
}

#[test]
fn filter_with_int_literal() {
    let q = query("User filter .age > 30");
    assert_eq!(q.filter, Some(cmp(field("age"), BinOp::Gt, int(30))));
}

#[test]
fn filter_with_string_literal() {
    let q = query("User filter .name = \"Alice\"");
    assert_eq!(q.filter, Some(cmp(field("name"), BinOp::Eq, text("Alice"))));
}

#[test]
fn order_limit_offset() {
    let q = query("User order .name asc limit 10 offset 5");
    let order = q.order.expect("order clause");
    assert_eq!(order.keys.len(), 1);
    assert_eq!(order.keys[0].expr, field("name"));
    assert!(!order.keys[0].descending);
    assert_eq!(q.limit, Some(int(10)));
    assert_eq!(q.offset, Some(int(5)));
}

#[test]
fn projection_braces_keep_both_fields_in_order() {
    let q = query("User { .name, .age }");
    assert_eq!(
        q.projection,
        Some(vec![
            ProjectionField {
                alias: None,
                expr: field("name")
            },
            ProjectionField {
                alias: None,
                expr: field("age")
            },
        ])
    );
}

// ---------------------------------------------------------------------------
// Numeric boundary values
// ---------------------------------------------------------------------------

#[test]
fn i64_max_literal() {
    let q = query(&format!("User filter .id = {}", i64::MAX));
    assert_eq!(q.filter, Some(cmp(field("id"), BinOp::Eq, int(i64::MAX))));
}

#[test]
fn i64_min_literal() {
    let q = query(&format!("User filter .id = {}", i64::MIN));
    assert_eq!(q.filter, Some(cmp(field("id"), BinOp::Eq, int(i64::MIN))));
}

#[test]
fn zero_literal() {
    let q = query("User filter .count = 0");
    assert_eq!(q.filter, Some(cmp(field("count"), BinOp::Eq, int(0))));
}

#[test]
fn negative_float_keeps_its_sign_in_the_literal() {
    let q = query("User filter .score > -3.75");
    assert_eq!(
        q.filter,
        Some(cmp(
            field("score"),
            BinOp::Gt,
            Expr::Literal(Literal::Float(-3.75))
        ))
    );
}

// ---------------------------------------------------------------------------
// String edge cases
// ---------------------------------------------------------------------------

#[test]
fn empty_string_literal() {
    let q = query("User filter .name = \"\"");
    assert_eq!(q.filter, Some(cmp(field("name"), BinOp::Eq, text(""))));
}

#[test]
fn a_unicode_escape_is_decoded_in_the_literal() {
    let q = query("User filter .name = \"cafe\\u0301\"");
    assert_eq!(
        q.filter,
        Some(cmp(field("name"), BinOp::Eq, text("cafe\u{301}")))
    );
}

#[test]
fn string_with_spaces() {
    let q = query("User filter .name = \"hello world\"");
    assert_eq!(
        q.filter,
        Some(cmp(field("name"), BinOp::Eq, text("hello world")))
    );
}

// ---------------------------------------------------------------------------
// Identifier edge cases
// ---------------------------------------------------------------------------

#[test]
fn a_long_identifier_reaches_the_ast_whole() {
    let long_name = "A".repeat(1000);
    let q = query(&format!("{long_name} filter .id = 1"));
    assert_eq!(q.source, long_name);
}

#[test]
fn underscore_identifier() {
    let q = query("my_table filter .my_col = 1");
    assert_eq!(q.source, "my_table");
    assert_eq!(q.filter, Some(cmp(field("my_col"), BinOp::Eq, int(1))));
}

#[test]
fn identifier_with_digits() {
    let q = query("Table123 filter .col456 = 1");
    assert_eq!(q.source, "Table123");
    assert_eq!(q.filter, Some(cmp(field("col456"), BinOp::Eq, int(1))));
}

// ---------------------------------------------------------------------------
// Missing delimiters / incomplete input
// ---------------------------------------------------------------------------

#[test]
fn missing_closing_brace_is_refused() {
    assert_refused("User { .name, .age", "expected '}', got end of input");
}

#[test]
fn missing_closing_paren_is_refused() {
    assert_refused(
        "User filter .id in (1, 2, 3",
        "expected ')', got end of input",
    );
}

#[test]
fn dangling_filter_is_refused() {
    assert_refused(
        "User filter",
        "unexpected token in expression: end of input",
    );
}

#[test]
fn dangling_order_is_refused() {
    assert_refused("User order", "unexpected token in expression: end of input");
}

#[test]
fn a_field_with_no_source_is_refused() {
    assert_refused(".name", "expected statement, got field '.name'");
}

// ---------------------------------------------------------------------------
// Keywords cannot stand in for a type name
// ---------------------------------------------------------------------------

#[test]
fn keyword_filter_cannot_start_a_statement() {
    assert_refused("filter filter .x = 1", "expected statement, got 'filter'");
}

#[test]
fn keyword_order_cannot_start_a_statement() {
    assert_refused("order order .x asc", "expected statement, got 'order'");
}

#[test]
fn keyword_limit_cannot_start_a_statement() {
    assert_refused("limit limit 10", "expected statement, got 'limit'");
}

#[test]
fn keyword_insert_cannot_name_an_insert_target() {
    assert_refused(
        "insert insert { name := \"x\" }",
        "expected type name, got 'insert'",
    );
}

#[test]
fn keyword_delete_cannot_start_a_statement() {
    assert_refused(
        "delete delete filter .id = 1",
        "'delete' cannot start a statement",
    );
}

#[test]
fn keyword_update_cannot_start_a_statement() {
    assert_refused(
        "update update filter .id = 1 { name := \"x\" }",
        "'update' cannot start a statement",
    );
}

#[test]
fn keyword_type_cannot_name_a_type() {
    assert_refused(
        "type type { name: str }",
        "'type' is a reserved word and cannot be used as a type name",
    );
}

// ---------------------------------------------------------------------------
// Complex / nested expressions
// ---------------------------------------------------------------------------

#[test]
fn a_fifty_clause_and_chain_keeps_every_conjunct() {
    let clause = ".a = 1";
    let q = query(&format!(
        "User filter {}",
        (0..50).map(|_| clause).collect::<Vec<_>>().join(" and ")
    ));
    let filter = q.filter.expect("filter clause");
    assert_eq!(count_binops(&filter, BinOp::And), 49);
    assert_eq!(count_binops(&filter, BinOp::Eq), 50);
}

#[test]
fn redundant_parens_do_not_reach_the_ast() {
    let parenthesised = query("User filter (((.age > 10)))");
    let bare = query("User filter .age > 10");
    assert_eq!(parenthesised.filter, bare.filter);
}

#[test]
fn and_binds_tighter_than_or() {
    let q = query("User filter .a = 1 or .b = 2 and .c = 3");
    assert_eq!(
        q.filter,
        Some(cmp(
            cmp(field("a"), BinOp::Eq, int(1)),
            BinOp::Or,
            cmp(
                cmp(field("b"), BinOp::Eq, int(2)),
                BinOp::And,
                cmp(field("c"), BinOp::Eq, int(3))
            )
        ))
    );
}

// ---------------------------------------------------------------------------
// Aggregations
// ---------------------------------------------------------------------------

#[test]
fn count_over_a_table_takes_no_argument() {
    let q = query("count(User)");
    let agg = q.aggregation.expect("aggregation");
    assert_eq!(agg.function, AggFunc::Count);
    assert_eq!(agg.argument, None);
    assert_eq!(agg.mode, AggregateMode::Symmetric);
    assert_eq!(q.source, "User");
}

#[test]
fn a_projection_inside_an_aggregate_becomes_its_argument() {
    let q = query("sum(User filter .active = true { .price })");
    let agg = q.aggregation.expect("aggregation");
    assert_eq!(agg.function, AggFunc::Sum);
    assert_eq!(agg.argument, Some(field("price")));
    assert_eq!(
        q.filter,
        Some(cmp(
            field("active"),
            BinOp::Eq,
            Expr::Literal(Literal::Bool(true))
        ))
    );
    assert_eq!(q.projection, None);
}

// ---------------------------------------------------------------------------
// Insert / Update / Delete
// ---------------------------------------------------------------------------

#[test]
fn insert_basic() {
    let stmt = parse("insert User { name := \"Alice\", age := 30 }").expect("parses");
    let Statement::Insert(insert) = stmt else {
        panic!("expected an insert, got {stmt:?}");
    };
    assert_eq!(insert.target, "User");
    assert!(!insert.returning);
    assert_eq!(insert.rows.len(), 1);
    let names: Vec<&str> = insert.rows[0].iter().map(|a| a.field.as_str()).collect();
    assert_eq!(names, ["name", "age"]);
    assert_eq!(insert.rows[0][0].value, text("Alice"));
    assert_eq!(insert.rows[0][1].value, int(30));
}

#[test]
fn update_with_filter() {
    let stmt = parse("User filter .id = 1 update { name := \"Bob\" }").expect("parses");
    let Statement::UpdateQuery(update) = stmt else {
        panic!("expected an update, got {stmt:?}");
    };
    assert_eq!(update.source, "User");
    assert_eq!(update.filter, Some(cmp(field("id"), BinOp::Eq, int(1))));
    assert_eq!(update.assignments.len(), 1);
    assert_eq!(update.assignments[0].field, "name");
    assert_eq!(update.assignments[0].value, text("Bob"));
    assert!(!update.returning);
}

#[test]
fn delete_with_filter() {
    let stmt = parse("User filter .id = 1 delete").expect("parses");
    let Statement::DeleteQuery(delete) = stmt else {
        panic!("expected a delete, got {stmt:?}");
    };
    assert_eq!(delete.source, "User");
    assert_eq!(delete.filter, Some(cmp(field("id"), BinOp::Eq, int(1))));
    assert!(!delete.returning);
}

// ---------------------------------------------------------------------------
// DDL edge cases
// ---------------------------------------------------------------------------

#[test]
fn create_type_basic() {
    let stmt = parse("type Product { name: str, price: float }").expect("parses");
    let Statement::CreateType(create) = stmt else {
        panic!("expected a create type, got {stmt:?}");
    };
    assert_eq!(create.name, "Product");
    assert!(!create.if_not_exists);
    let fields: Vec<(&str, &str, bool)> = create
        .fields
        .iter()
        .map(|f| (f.name.as_str(), f.type_name.as_str(), f.required))
        .collect();
    assert_eq!(fields, [("name", "str", false), ("price", "float", false)]);
}

#[test]
fn a_type_with_no_fields_parses_to_an_empty_field_list() {
    let stmt = parse("type Empty { }").expect("parses");
    let Statement::CreateType(create) = stmt else {
        panic!("expected a create type, got {stmt:?}");
    };
    assert_eq!(create.name, "Empty");
    assert!(create.fields.is_empty());
}

#[test]
fn alter_add_column() {
    let stmt = parse("alter User add column status: str").expect("parses");
    let Statement::AlterTable(alter) = stmt else {
        panic!("expected an alter, got {stmt:?}");
    };
    assert_eq!(alter.table, "User");
    assert_eq!(
        alter.action,
        AlterAction::AddColumn {
            name: "status".to_string(),
            type_name: "str".to_string(),
            required: false,
        }
    );
}

#[test]
fn alter_drop_column() {
    let stmt = parse("alter User drop column status").expect("parses");
    let Statement::AlterTable(alter) = stmt else {
        panic!("expected an alter, got {stmt:?}");
    };
    assert_eq!(alter.table, "User");
    assert_eq!(
        alter.action,
        AlterAction::DropColumn {
            name: "status".to_string(),
            if_exists: false,
        }
    );
}

#[test]
fn drop_table() {
    let stmt = parse("drop User").expect("parses");
    let Statement::DropTable(drop) = stmt else {
        panic!("expected a drop, got {stmt:?}");
    };
    assert_eq!(drop.table, "User");
    assert!(!drop.if_exists);
}

// ---------------------------------------------------------------------------
// A clause that carries a value may be written once
// ---------------------------------------------------------------------------

#[test]
fn a_second_filter_is_refused() {
    assert_refused(
        "User filter .a = 1 filter .b = 2",
        "'filter' appears more than once in one pipeline",
    );
}

#[test]
fn a_second_order_is_refused() {
    assert_refused(
        "User order .a asc order .b desc",
        "'order' appears more than once in one pipeline",
    );
}

#[test]
fn a_second_limit_is_refused() {
    assert_refused("User limit 5 limit 9", "'limit' appears more than once");
}

#[test]
fn a_second_offset_is_refused() {
    assert_refused(
        "User limit 5 offset 1 offset 2",
        "'offset' appears more than once",
    );
}

#[test]
fn a_second_projection_is_refused() {
    assert_refused("User { .a } { .b }", "a projection appears more than once");
}

#[test]
fn a_second_group_is_refused() {
    assert_refused("User group .a group .b", "'group' appears more than once");
}

#[test]
fn a_second_filter_inside_an_aggregate_is_refused() {
    assert_refused(
        "count(User filter .a = 1 filter .b = 2)",
        "'filter' appears more than once in one pipeline",
    );
}

#[test]
fn a_second_limit_inside_a_nested_block_is_refused() {
    assert_refused(
        "User as u { u.name, orders: Order as o filter o.uid = u.id limit 1 limit 2 { o.id } }",
        "'limit' appears more than once",
    );
}

#[test]
fn a_second_having_still_chains_onto_the_first() {
    let q = query("User group .a having count(.b) > 1 having count(.b) < 5");
    let group = q.group_by.expect("group by");
    let having = group.having.expect("having");
    assert_eq!(count_binops(&having, BinOp::And), 1);
    assert_eq!(count_binops(&having, BinOp::Gt), 1);
    assert_eq!(count_binops(&having, BinOp::Lt), 1);
}

// ---------------------------------------------------------------------------
// Miscellaneous, pinned
// ---------------------------------------------------------------------------

#[test]
fn a_trailing_comma_in_a_projection_adds_no_field() {
    let q = query("User { .name, .age, }");
    assert_eq!(
        q.projection,
        Some(vec![
            ProjectionField {
                alias: None,
                expr: field("name")
            },
            ProjectionField {
                alias: None,
                expr: field("age")
            },
        ])
    );
}

#[test]
fn an_empty_in_list_parses_to_an_empty_list() {
    let q = query("User filter .id in ()");
    assert_eq!(
        q.filter,
        Some(Expr::InList {
            expr: Box::new(field("id")),
            list: vec![],
            negated: false,
        })
    );
}

#[test]
fn between_desugars_to_a_pair_of_comparisons() {
    let q = query("User filter .age between 18 and 65");
    assert_eq!(
        q.filter,
        Some(cmp(
            cmp(field("age"), BinOp::Gte, int(18)),
            BinOp::And,
            cmp(field("age"), BinOp::Lte, int(65))
        ))
    );
}

#[test]
fn like_keeps_its_own_operator() {
    let q = query("User filter .name like \"%alice%\"");
    assert_eq!(
        q.filter,
        Some(cmp(field("name"), BinOp::Like, text("%alice%")))
    );
}

#[test]
fn case_when_becomes_a_case_expression_under_its_alias() {
    let q = query("User { status: case when .age > 65 then \"senior\" else \"regular\" end }");
    assert_eq!(
        q.projection,
        Some(vec![ProjectionField {
            alias: Some("status".to_string()),
            expr: Expr::Case {
                whens: vec![(
                    Box::new(cmp(field("age"), BinOp::Gt, int(65))),
                    Box::new(text("senior"))
                )],
                else_expr: Some(Box::new(text("regular"))),
            },
        }])
    );
}
