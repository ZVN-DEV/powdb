//! Result rendering: table, JSON, and CSV for local and remote results.

use super::*;

// ─── Output formatting ──────────────────────────────────────────────────────

pub(crate) fn print_local_result(result: &QueryResult, mode: OutputMode) {
    match mode {
        OutputMode::Table => print_local_result_table(result),
        OutputMode::Json => print_local_result_json(result),
        OutputMode::Csv => print_local_result_csv(result),
    }
}

pub(crate) fn print_local_result_table(result: &QueryResult) {
    match result {
        QueryResult::Rows { columns, rows } => {
            if rows.is_empty() {
                println!("(empty set)");
                return;
            }
            let str_rows: Vec<Vec<String>> = rows
                .iter()
                .map(|row| row.iter().map(format_value).collect())
                .collect();
            print_table(columns, &str_rows);
        }
        QueryResult::Scalar(val) => {
            println!("{}", format_value(val));
        }
        QueryResult::Modified(n) => {
            println!("{n} row{} affected", if *n == 1 { "" } else { "s" });
        }
        QueryResult::Created(name) => {
            println!("type {name} created");
        }
        QueryResult::Executed { message } => {
            println!("{message}");
        }
    }
}

/// One JSON document per statement, on one line, so `--format json` output can
/// be piped straight into `jq` (and read line by line for multi-statement runs).
pub(crate) fn print_local_result_json(result: &QueryResult) {
    match result {
        QueryResult::Rows { columns, rows } => println!("{}", rows_to_json(columns, rows)),
        QueryResult::Scalar(val) => println!("{{\"value\":{}}}", value_to_json(val)),
        QueryResult::Modified(n) => println!("{{\"affected\":{n}}}"),
        QueryResult::Created(name) => println!("{{\"created\":{}}}", json_string(name)),
        QueryResult::Executed { message } => {
            println!("{{\"message\":{}}}", json_string(message))
        }
    }
}

pub(crate) fn print_local_result_csv(result: &QueryResult) {
    match result {
        QueryResult::Rows { columns, rows } => {
            println!(
                "{}",
                columns
                    .iter()
                    .map(|c| csv_field(c))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            for row in rows {
                println!(
                    "{}",
                    row.iter()
                        .map(|v| csv_field(&format_value(v)))
                        .collect::<Vec<_>>()
                        .join(",")
                );
            }
        }
        QueryResult::Scalar(val) => println!("{}", csv_field(&format_value(val))),
        QueryResult::Modified(n) => println!("{n}"),
        QueryResult::Created(name) => println!("{}", csv_field(name)),
        QueryResult::Executed { message } => println!("{}", csv_field(message)),
    }
}

/// The `{"columns":[…],"rows":[…]}` document for a typed row set.
///
/// Shared by embedded results and typed remote results so `--format json`
/// renders byte-identically on both transports: the same query over the same
/// data must not change JSON types just because it went through a server.
pub(crate) fn rows_to_json(columns: &[String], rows: &[Vec<Value>]) -> String {
    let cols = columns
        .iter()
        .map(|c| json_string(c))
        .collect::<Vec<_>>()
        .join(",");
    let body = rows
        .iter()
        .map(|row| {
            let cells = row.iter().map(value_to_json).collect::<Vec<_>>().join(",");
            format!("[{cells}]")
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"columns\":[{cols}],\"rows\":[{body}]}}")
}

/// Render a string as a JSON string literal (RFC 8259 escaping).
pub(crate) fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Render one typed cell as JSON. Ints, floats, and bools keep their JSON type;
/// a json cell is emitted as the document itself (already canonical JSON text);
/// everything else, including datetimes (microseconds since epoch) and uuids,
/// is a string, matching how the table renderer displays it.
pub(crate) fn value_to_json(v: &Value) -> String {
    match v {
        Value::Int(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Float(n) if n.is_finite() => format!("{n}"),
        Value::Empty => "null".to_string(),
        Value::Json(b) => powdb_storage::pj1::pj1_to_text(b).unwrap_or_else(|_| "null".into()),
        other => json_string(&format_value(other)),
    }
}

/// RFC 4180 CSV field: quote when the value contains a comma, quote, CR, or LF,
/// doubling any embedded quote.
pub(crate) fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Render one wire cell for display. The server serializes NULL as the
/// bareword "null" (the sentinel the TS client's typed decoder matches);
/// remote mode renders it as `NULL`, matching the embedded REPL. A string
/// column whose *value* is literally "null" is indistinguishable on the
/// untyped wire — same tradeoff the TS client documents.
pub(crate) fn render_remote_cell(cell: &str) -> String {
    if cell == "null" {
        "NULL".into()
    } else {
        cell.into()
    }
}

pub(crate) fn print_remote_result(msg: &Message, mode: OutputMode) {
    match mode {
        OutputMode::Table => print_remote_result_table(msg),
        OutputMode::Json => print_remote_result_json(msg),
        OutputMode::Csv => print_remote_result_csv(msg),
    }
}

/// `--format json` asks the server for typed results (see
/// [`negotiate_typed_json`]), so the common case renders through exactly the
/// embedded renderer and the two transports agree byte for byte.
///
/// The stringly-typed arms below are the fallback for a server too old to
/// carry typed frames: every cell is then a JSON string except the NULL
/// sentinel, which becomes JSON `null`.
pub(crate) fn print_remote_result_json(msg: &Message) {
    match msg {
        Message::ResultRowsNative { columns, rows } => println!("{}", rows_to_json(columns, rows)),
        Message::ResultScalarNative { value } => {
            println!("{{\"value\":{}}}", value_to_json(value))
        }
        Message::ResultRows { columns, rows } => {
            let cols = columns
                .iter()
                .map(|c| json_string(c))
                .collect::<Vec<_>>()
                .join(",");
            let body = rows
                .iter()
                .map(|row| {
                    let cells = row
                        .iter()
                        .map(|c| remote_cell_to_json(c))
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("[{cells}]")
                })
                .collect::<Vec<_>>()
                .join(",");
            println!("{{\"columns\":[{cols}],\"rows\":[{body}]}}");
        }
        Message::ResultScalar { value } => {
            println!("{{\"value\":{}}}", remote_cell_to_json(value))
        }
        Message::ResultOk { affected } => println!("{{\"affected\":{affected}}}"),
        Message::ResultMessage { message } => {
            println!("{{\"message\":{}}}", json_string(message))
        }
        Message::Error { message } => eprintln!("{{\"error\":{}}}", json_string(message)),
        other => eprintln!("Error: unexpected response: {other:?}"),
    }
}

pub(crate) fn remote_cell_to_json(cell: &str) -> String {
    if cell == "null" {
        "null".to_string()
    } else {
        json_string(cell)
    }
}

pub(crate) fn print_remote_result_csv(msg: &Message) {
    match msg {
        Message::ResultRows { columns, rows } => {
            println!(
                "{}",
                columns
                    .iter()
                    .map(|c| csv_field(c))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            for row in rows {
                println!(
                    "{}",
                    row.iter()
                        .map(|c| csv_field(&render_remote_cell(c)))
                        .collect::<Vec<_>>()
                        .join(",")
                );
            }
        }
        Message::ResultScalar { value } => println!("{}", csv_field(&render_remote_cell(value))),
        Message::ResultOk { affected } => println!("{affected}"),
        Message::ResultMessage { message } => println!("{}", csv_field(message)),
        Message::Error { message } => eprintln!("Error: {message}"),
        other => eprintln!("Error: unexpected response: {other:?}"),
    }
}

pub(crate) fn print_remote_result_table(msg: &Message) {
    match msg {
        Message::ResultRows { columns, rows } => {
            if rows.is_empty() {
                println!("(empty set)");
                return;
            }
            let rendered: Vec<Vec<String>> = rows
                .iter()
                .map(|row| row.iter().map(|c| render_remote_cell(c)).collect())
                .collect();
            print_table(columns, &rendered);
        }
        Message::ResultScalar { value } => {
            println!("{}", render_remote_cell(value));
        }
        Message::ResultOk { affected } => {
            println!(
                "{affected} row{} affected",
                if *affected == 1 { "" } else { "s" }
            );
        }
        Message::ResultMessage { message } => {
            println!("{message}");
        }
        Message::Error { message } => {
            eprintln!("Error: {message}");
        }
        other => {
            eprintln!("Error: unexpected response: {other:?}");
        }
    }
}

pub(crate) fn print_table(columns: &[String], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();
    for row in rows {
        for (i, val) in row.iter().enumerate() {
            if i < widths.len() && val.len() > widths[i] {
                widths[i] = val.len();
            }
        }
    }

    let header: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{:width$}", c, width = widths[i]))
        .collect();
    println!(" {} ", header.join(" | "));
    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    println!("-{}-", sep.join("-+-"));

    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, v)| format!("{:width$}", v, width = widths[i]))
            .collect();
        println!(" {} ", cells.join(" | "));
    }

    println!(
        "({} row{})",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
}

pub(crate) fn format_value(v: &Value) -> String {
    match v {
        Value::Int(n) => n.to_string(),
        Value::Float(n) => format!("{n}"),
        Value::Bool(b) => b.to_string(),
        Value::Str(s) => s.clone(),
        Value::DateTime(t) => format!("{t}"),
        Value::Uuid(u) => format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7],
            u[8], u[9], u[10], u[11], u[12], u[13], u[14], u[15]
        ),
        Value::Bytes(b) => format!("<{} bytes>", b.len()),
        // Render canonical JSON text from the PJ1 bytes.
        Value::Json(b) => {
            powdb_storage::pj1::pj1_to_text(b).unwrap_or_else(|_| "null".into())
        }
        Value::Empty => "NULL".into(),
    }
}
