#![no_main]
use libfuzzer_sys::fuzz_target;
use powdb_query::lexer::lex;
use powdb_query::parser::parse;
use powdb_query::token::Token;

// A real text round trip for PowQL.
//
// The previous version of this target lexed the input, lexed it a second
// time, and asserted the two token COUNTS were equal. That is a determinism
// check on `lex`, and a weak one: a lexer that dropped every literal would
// drop it identically both times and stay green. It never left the token
// domain, so nothing it asserted could fail for a round-trip reason.
//
// This target closes the loop through text:
//
//   1. parse(input)                      -> ast1        (skip unparseable input)
//   2. render(lex(input))                -> canonical   (PowQL source text)
//   3. parse(canonical)                  -> ast2        (must SUCCEED)
//   4. ast1 == ast2                                     (meaning preserved)
//   5. render(lex(canonical)) == canonical              (text is a fixed point)
//
// Step 4 is the load-bearing one: a statement that survives serialisation with
// a different meaning is exactly the bug class that silently rewrites a stored
// view definition. Step 5 catches printers that are correct once but not
// stable, which is what makes stored text safe to re-serialise.
//
// Honest scope note: `render` below is a TOKEN printer, not an AST printer.
// PowDB has no AST-to-source printer to call (the parser's own `tokens_to_text`
// is private and token-level too), and writing one solely for this fuzzer
// would mean the fuzzer mostly tested the fuzzer's printer. Rendering the
// token stream keeps the printer total over a finite enum, while steps 3 and 4
// still assert at the AST level, which is where the meaning lives.

/// Render a token stream back to PowQL source text.
///
/// Every token is separated by a single space, so no two tokens can fuse into
/// a third (`<` `=` must not become `<=`). Values are escaped so they re-lex
/// to the identical token.
fn render(tokens: &[Token]) -> Option<String> {
    let mut out = String::with_capacity(64);
    for tok in tokens {
        if matches!(tok, Token::Eof) {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        match tok {
            Token::Ident(s) => out.push_str(s),
            Token::DotIdent(s) => {
                out.push('.');
                out.push_str(s);
            }
            Token::Param(s) => {
                out.push('$');
                out.push_str(s);
            }
            Token::IntLit(v) => out.push_str(&v.to_string()),
            Token::FloatLit(v) => {
                // The lexer has no exponent form: a float literal is
                // `[-] digits . digits`. Rust's shortest-round-trip formatting
                // switches to `1e21` for extreme magnitudes, which no PowQL
                // lexer can read back. Those values are unreachable from
                // source text of the accepted grammar in the first place, so
                // decline to round-trip them rather than report a bug the
                // parser cannot have.
                let s = v.to_string();
                if !s
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
                {
                    return None;
                }
                out.push_str(&s);
                // `4` lexes as IntLit, not FloatLit. Keep the token kind
                // stable across the round trip.
                if !s.contains('.') {
                    out.push_str(".0");
                }
            }
            Token::StringLit(s) => {
                out.push('"');
                for c in s.chars() {
                    // The lexer unescapes `\"` and `\\`; every other character
                    // (including a raw newline or tab) is taken literally, so
                    // these two are the only ones that must be re-escaped.
                    if c == '"' || c == '\\' {
                        out.push('\\');
                    }
                    out.push(c);
                }
                out.push('"');
            }
            Token::BoolLit(v) => out.push_str(if *v { "true" } else { "false" }),

            Token::Type => out.push_str("type"),
            Token::Filter => out.push_str("filter"),
            Token::Order => out.push_str("order"),
            Token::Limit => out.push_str("limit"),
            Token::Offset => out.push_str("offset"),
            Token::Insert => out.push_str("insert"),
            Token::Update => out.push_str("update"),
            Token::Delete => out.push_str("delete"),
            Token::Upsert => out.push_str("upsert"),
            Token::Returning => out.push_str("returning"),
            Token::Select => out.push_str("select"),
            Token::Required => out.push_str("required"),
            Token::Default => out.push_str("default"),
            Token::Auto => out.push_str("auto"),
            Token::Multi => out.push_str("multi"),
            Token::Link => out.push_str("link"),
            Token::Index => out.push_str("index"),
            Token::Unique => out.push_str("unique"),
            Token::On => out.push_str("on"),
            Token::Conflict => out.push_str("conflict"),
            Token::Asc => out.push_str("asc"),
            Token::Desc => out.push_str("desc"),
            Token::And => out.push_str("and"),
            Token::Or => out.push_str("or"),
            Token::Not => out.push_str("not"),
            Token::Exists => out.push_str("exists"),
            Token::Let => out.push_str("let"),
            Token::As => out.push_str("as"),
            Token::Match => out.push_str("match"),
            Token::Group => out.push_str("group"),
            Token::Join => out.push_str("join"),
            Token::Inner => out.push_str("inner"),
            Token::LeftKw => out.push_str("left"),
            Token::RightKw => out.push_str("right"),
            Token::Outer => out.push_str("outer"),
            Token::Cross => out.push_str("cross"),
            Token::Transaction => out.push_str("transaction"),
            Token::Begin => out.push_str("begin"),
            Token::Commit => out.push_str("commit"),
            Token::Rollback => out.push_str("rollback"),
            Token::View => out.push_str("view"),
            Token::Materialized => out.push_str("materialized"),
            Token::Refresh => out.push_str("refresh"),
            Token::Union => out.push_str("union"),
            Token::Having => out.push_str("having"),
            Token::Distinct => out.push_str("distinct"),
            Token::In => out.push_str("in"),
            Token::Between => out.push_str("between"),
            Token::Like => out.push_str("like"),
            Token::Count => out.push_str("count"),
            Token::Avg => out.push_str("avg"),
            Token::Sum => out.push_str("sum"),
            Token::Min => out.push_str("min"),
            Token::Max => out.push_str("max"),
            Token::Raw => out.push_str("raw"),
            Token::Is => out.push_str("is"),
            Token::Null => out.push_str("null"),

            Token::Upper => out.push_str("upper"),
            Token::Lower => out.push_str("lower"),
            Token::Length => out.push_str("length"),
            Token::Trim => out.push_str("trim"),
            Token::Substring => out.push_str("substring"),
            Token::Concat => out.push_str("concat"),

            Token::Abs => out.push_str("abs"),
            Token::Round => out.push_str("round"),
            Token::Ceil => out.push_str("ceil"),
            Token::Floor => out.push_str("floor"),
            Token::Sqrt => out.push_str("sqrt"),
            Token::Pow => out.push_str("pow"),

            Token::Now => out.push_str("now"),
            Token::Extract => out.push_str("extract"),
            Token::DateAdd => out.push_str("date_add"),
            Token::DateDiff => out.push_str("date_diff"),

            Token::JsonType => out.push_str("json_type"),
            Token::JsonText => out.push_str("json_text"),

            Token::Cast => out.push_str("cast"),

            Token::Case => out.push_str("case"),
            Token::When => out.push_str("when"),
            Token::Then => out.push_str("then"),
            Token::Else => out.push_str("else"),
            Token::End => out.push_str("end"),

            Token::Over => out.push_str("over"),
            Token::Partition => out.push_str("partition"),
            Token::RowNumber => out.push_str("row_number"),
            Token::Rank => out.push_str("rank"),
            Token::DenseRank => out.push_str("dense_rank"),

            Token::Alter => out.push_str("alter"),
            Token::Drop => out.push_str("drop"),
            Token::Add => out.push_str("add"),
            Token::Column => out.push_str("column"),
            Token::Explain => out.push_str("explain"),
            Token::Schema => out.push_str("schema"),
            Token::Describe => out.push_str("describe"),

            Token::Eq => out.push('='),
            Token::Neq => out.push_str("!="),
            Token::Lt => out.push('<'),
            Token::Gt => out.push('>'),
            Token::Lte => out.push_str("<="),
            Token::Gte => out.push_str(">="),
            Token::Assign => out.push_str(":="),
            Token::Arrow => out.push_str("->"),
            Token::Pipe => out.push('|'),
            Token::Coalesce => out.push_str("??"),
            Token::Plus => out.push('+'),
            Token::Minus => out.push('-'),
            Token::Star => out.push('*'),
            Token::Slash => out.push('/'),

            Token::LBrace => out.push('{'),
            Token::RBrace => out.push('}'),
            Token::LParen => out.push('('),
            Token::RParen => out.push(')'),
            Token::Comma => out.push(','),
            Token::Colon => out.push(':'),
            Token::Dot => out.push('.'),

            Token::Eof => unreachable!("Eof is filtered above"),
        }
    }
    Some(out)
}

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(tokens) = lex(input) else {
        return;
    };
    // Only round-trip statements the parser accepts: a rejected input has no
    // meaning to preserve.
    let Ok(ast1) = parse(input) else {
        return;
    };
    let Some(canonical) = render(&tokens) else {
        return;
    };

    let ast2 = match parse(&canonical) {
        Ok(a) => a,
        Err(e) => panic!(
            "round trip did not re-parse\n  input     : {input:?}\n  canonical : {canonical:?}\n  error     : {e:?}"
        ),
    };
    assert_eq!(
        ast1, ast2,
        "round trip changed the statement\n  input     : {input:?}\n  canonical : {canonical:?}"
    );

    // Serialising the canonical form again must be a no-op, otherwise stored
    // text drifts every time it is rewritten.
    let tokens2 = match lex(&canonical) {
        Ok(t) => t,
        Err(e) => panic!(
            "canonical text failed to re-lex\n  canonical : {canonical:?}\n  error     : {e:?}"
        ),
    };
    let Some(canonical2) = render(&tokens2) else {
        panic!("canonical text rendered a non-renderable token stream: {canonical:?}")
    };
    assert_eq!(
        canonical, canonical2,
        "canonical text is not a fixed point\n  input : {input:?}"
    );
});
