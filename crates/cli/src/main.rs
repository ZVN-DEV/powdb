use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_server::protocol::Message;
use powdb_storage::types::Value;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Editor, Helper};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::io::{BufReader, BufWriter};
use tokio::net::TcpStream;
use tracing_subscriber::EnvFilter;

// ─── Tab completion helper ─────────────────────────────────────────────────

const POWQL_KEYWORDS: &[&str] = &[
    "abs", "alter", "and", "as", "asc", "avg", "between", "bool", "bytes", "case", "cast",
    "ceil", "coalesce", "concat", "count", "cross", "datetime", "delete", "desc", "distinct",
    "drop", "else", "end", "exec", "explain", "extract", "false", "filter", "float", "floor",
    "group", "having", "in", "index", "inner", "insert", "int", "is", "join", "left", "length",
    "like", "limit", "lower", "max", "min", "not", "now", "null", "offset", "on", "or", "order",
    "pow", "prepare", "refresh", "required", "round", "sqrt", "str", "substring", "sum", "then",
    "trim", "true", "type", "union", "update", "upper", "upsert", "uuid", "view", "when", "where",
];

const META_COMMANDS: &[&str] = &[
    ".exit", ".help", ".quit", ".schema", ".tables", ".timing",
];

struct PowqlHelper;

impl Helper for PowqlHelper {}

impl Completer for PowqlHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let (start, word) = find_word_start(line, pos);
        let lower = word.to_lowercase();

        let mut matches: Vec<Pair> = Vec::new();

        if word.starts_with('.') {
            // Complete meta-commands
            for cmd in META_COMMANDS {
                if cmd.starts_with(&lower) {
                    matches.push(Pair {
                        display: cmd.to_string(),
                        replacement: cmd.to_string(),
                    });
                }
            }
        } else if !word.is_empty() {
            // Complete PowQL keywords
            for kw in POWQL_KEYWORDS {
                if kw.starts_with(&lower) {
                    // Preserve the case style of what the user typed
                    let replacement = if word.chars().next().is_some_and(|c| c.is_uppercase()) {
                        let mut s = kw.to_string();
                        s[..1].make_ascii_uppercase();
                        s
                    } else {
                        kw.to_string()
                    };
                    matches.push(Pair {
                        display: kw.to_string(),
                        replacement,
                    });
                }
            }
        }

        Ok((start, matches))
    }
}

impl Hinter for PowqlHelper {
    type Hint = String;

    fn hint(&self, _line: &str, _pos: usize, _ctx: &rustyline::Context<'_>) -> Option<String> {
        None
    }
}

impl Highlighter for PowqlHelper {
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Borrowed(hint)
    }
}

impl Validator for PowqlHelper {}

fn find_word_start(line: &str, pos: usize) -> (usize, &str) {
    let bytes = &line.as_bytes()[..pos];
    let start = bytes
        .iter()
        .rposition(|&b| b == b' ' || b == b'\t')
        .map(|i| i + 1)
        .unwrap_or(0);
    (start, &line[start..pos])
}

fn history_path() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".powdb_history")
}

struct CliArgs {
    data_dir: String,
    remote: Option<String>,
    db: String,
    password: Option<String>,
}

fn parse_args() -> CliArgs {
    let mut data_dir = "./powdb_data".to_string();
    let mut remote: Option<String> = None;
    let mut db: String = "main".to_string();
    let mut password: Option<String> = std::env::var("POWDB_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty());

    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    let mut saw_positional = false;
    while i < argv.len() {
        match argv[i].as_str() {
            "--remote" | "-r" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--remote requires host:port");
                    std::process::exit(2);
                }
                remote = Some(argv[i].clone());
            }
            "--db" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--db requires a name");
                    std::process::exit(2);
                }
                db = argv[i].clone();
            }
            "--password" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--password requires a value");
                    std::process::exit(2);
                }
                password = Some(argv[i].clone());
            }
            "--data-dir" | "-d" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--data-dir requires a path");
                    std::process::exit(2);
                }
                data_dir = argv[i].clone();
            }
            "--version" | "-V" => {
                println!("powdb-cli {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!("powdb-cli — PowQL interactive shell");
                println!();
                println!("USAGE:");
                println!("    powdb-cli [OPTIONS] [DATA_DIR]");
                println!();
                println!("OPTIONS:");
                println!("    -r, --remote <HOST:PORT>   Connect to a remote server over TCP");
                println!("        --db <NAME>            Database name (default: main)");
                println!("        --password <PW>        Password for remote auth");
                println!(
                    "    -d, --data-dir <PATH>      Embedded data dir (default: ./powdb_data)"
                );
                println!("    -h, --help                 Print this message");
                println!("    -V, --version              Print version and exit");
                println!();
                println!("MODES:");
                println!("    Embedded (default):  powdb-cli ./mydata");
                println!(
                    "    Remote:              powdb-cli --remote 127.0.0.1:5433 --password secret"
                );
                std::process::exit(0);
            }
            other if !other.starts_with('-') && !saw_positional => {
                data_dir = other.to_string();
                saw_positional = true;
            }
            other => {
                eprintln!("unknown argument: {other}");
                eprintln!("try --help");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    CliArgs {
        data_dir,
        remote,
        db,
        password,
    }
}

fn main() {
    // Tracing for the CLI (mostly off by default; users can set RUST_LOG=debug).
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    let args = parse_args();

    if let Some(remote_addr) = &args.remote {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime");
        rt.block_on(run_remote(
            remote_addr.clone(),
            args.db.clone(),
            args.password.clone(),
        ));
    } else {
        run_embedded(&args.data_dir);
    }
}

// ─── Embedded mode ──────────────────────────────────────────────────────────

fn run_embedded(data_dir: &str) {
    eprintln!(
        "PowDB v{} — embedded mode",
        env!("CARGO_PKG_VERSION")
    );
    eprintln!("Data directory: {data_dir}");
    eprintln!("Type PowQL queries. Use Ctrl-D to exit. Type .help for commands.\n");

    let mut engine = Engine::new(Path::new(data_dir)).expect("failed to initialize engine");

    let mut rl = Editor::new().expect("failed to init readline");
    rl.set_helper(Some(PowqlHelper));

    let hist = history_path();
    rl.load_history(&hist).ok();

    let mut timing_enabled = false;

    loop {
        let line = match rl.readline("powql> ") {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(rustyline::error::ReadlineError::Interrupted) => continue,
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        rl.add_history_entry(trimmed).ok();

        // ── Meta-commands ──────────────────────────────────────────────
        if trimmed.starts_with('.') {
            match trimmed {
                ".quit" | ".exit" => break,
                ".help" => {
                    println!("Meta-commands:");
                    println!("  .tables          List all tables");
                    println!("  .schema <TABLE>  Show columns and types for a table");
                    println!("  .timing          Toggle query timing on/off");
                    println!("  .help            Show this help");
                    println!("  .quit / .exit    Exit the REPL");
                }
                ".tables" => {
                    let tables = engine.catalog().list_tables();
                    if tables.is_empty() {
                        println!("(no tables)");
                    } else {
                        for t in &tables {
                            println!("  {t}");
                        }
                        println!(
                            "({} table{})",
                            tables.len(),
                            if tables.len() == 1 { "" } else { "s" }
                        );
                    }
                }
                ".timing" => {
                    timing_enabled = !timing_enabled;
                    println!(
                        "Timing is {}.",
                        if timing_enabled { "on" } else { "off" }
                    );
                }
                cmd if cmd.starts_with(".schema") => {
                    let table_name = cmd.strip_prefix(".schema").unwrap().trim();
                    if table_name.is_empty() {
                        eprintln!("Usage: .schema <TABLE_NAME>");
                    } else if let Some(schema) = engine.catalog().schema(table_name) {
                        println!("Table: {}", schema.table_name);
                        println!(
                            "  {:<20} {:<12} Required",
                            "Column", "Type"
                        );
                        println!("  {:-<20} {:-<12} {:-<8}", "", "", "");
                        for col in &schema.columns {
                            println!(
                                "  {:<20} {:<12} {}",
                                col.name,
                                match col.type_id {
                                    powdb_storage::types::TypeId::Int => "int",
                                    powdb_storage::types::TypeId::Float => "float",
                                    powdb_storage::types::TypeId::Bool => "bool",
                                    powdb_storage::types::TypeId::Str => "str",
                                    powdb_storage::types::TypeId::DateTime => "datetime",
                                    powdb_storage::types::TypeId::Uuid => "uuid",
                                    powdb_storage::types::TypeId::Bytes => "bytes",
                                    powdb_storage::types::TypeId::Empty => "empty",
                                },
                                if col.required { "yes" } else { "no" }
                            );
                        }
                    } else {
                        eprintln!("Error: table '{table_name}' not found");
                    }
                }
                other => {
                    eprintln!("Unknown meta-command: {other}");
                    eprintln!("Type .help for available commands.");
                }
            }
            continue;
        }

        // ── Execute PowQL query ────────────────────────────────────────
        let start = Instant::now();
        match engine.execute_powql(trimmed) {
            Ok(result) => {
                print_local_result(&result);
                if timing_enabled {
                    let elapsed = start.elapsed();
                    if elapsed.as_secs() >= 1 {
                        println!("Time: {:.2}s", elapsed.as_secs_f64());
                    } else {
                        println!("Time: {:.2}ms", elapsed.as_secs_f64() * 1000.0);
                    }
                }
            }
            Err(e) => eprintln!("Error: {e}"),
        }
    }

    rl.save_history(&hist).ok();
    eprintln!("\nBye!");
}

// ─── Remote (wire protocol) mode ────────────────────────────────────────────

async fn run_remote(addr: String, db: String, password: Option<String>) {
    eprintln!(
        "PowDB v{} — remote mode",
        env!("CARGO_PKG_VERSION")
    );
    eprintln!("Connecting to {addr} ...");

    let stream = match TcpStream::connect(&addr).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("connection failed: {e}");
            std::process::exit(1);
        }
    };

    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    // Send CONNECT
    let connect = Message::Connect {
        db_name: db.clone(),
        password,
    };
    if let Err(e) = connect.write_to(&mut writer).await {
        eprintln!("failed to send CONNECT: {e}");
        std::process::exit(1);
    }
    if let Err(e) = tokio::io::AsyncWriteExt::flush(&mut writer).await {
        eprintln!("flush error: {e}");
        std::process::exit(1);
    }

    // Read CONNECT_OK or ERROR
    match Message::read_from(&mut reader).await {
        Ok(Some(Message::ConnectOk { version })) => {
            eprintln!("Connected to db `{db}` (server v{version})");
            eprintln!("Type PowQL queries. Use Ctrl-D to exit.\n");
        }
        Ok(Some(Message::Error { message })) => {
            eprintln!("server rejected connection: {message}");
            std::process::exit(1);
        }
        Ok(Some(other)) => {
            eprintln!("unexpected handshake reply: {other:?}");
            std::process::exit(1);
        }
        Ok(None) => {
            eprintln!("server closed connection during handshake");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("handshake read error: {e}");
            std::process::exit(1);
        }
    }

    let mut rl = Editor::new().expect("failed to init readline");
    rl.set_helper(Some(PowqlHelper));

    let hist = history_path();
    rl.load_history(&hist).ok();

    let mut timing_enabled = false;

    loop {
        let line = match rl.readline("powql> ") {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(rustyline::error::ReadlineError::Interrupted) => continue,
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        rl.add_history_entry(trimmed).ok();

        // Handle local-only meta-commands in remote mode
        if trimmed.starts_with('.') {
            match trimmed {
                ".quit" | ".exit" => break,
                ".timing" => {
                    timing_enabled = !timing_enabled;
                    println!(
                        "Timing is {}.",
                        if timing_enabled { "on" } else { "off" }
                    );
                }
                ".help" => {
                    println!("Meta-commands (remote mode):");
                    println!("  .timing          Toggle query timing on/off");
                    println!("  .help            Show this help");
                    println!("  .quit / .exit    Exit the REPL");
                    println!();
                    println!("Note: .tables and .schema are only available in embedded mode.");
                }
                _ => {
                    eprintln!("Meta-commands (.tables, .schema) are not supported in remote mode.");
                    eprintln!("Type .help for available commands.");
                }
            }
            continue;
        }

        let q = Message::Query {
            query: trimmed.to_string(),
        };
        if q.write_to(&mut writer).await.is_err() {
            eprintln!("write error — disconnected");
            break;
        }
        if tokio::io::AsyncWriteExt::flush(&mut writer).await.is_err() {
            eprintln!("flush error — disconnected");
            break;
        }

        let start = Instant::now();
        match Message::read_from(&mut reader).await {
            Ok(Some(msg)) => {
                print_remote_result(&msg);
                if timing_enabled {
                    let elapsed = start.elapsed();
                    if elapsed.as_secs() >= 1 {
                        println!("Time: {:.2}s", elapsed.as_secs_f64());
                    } else {
                        println!("Time: {:.2}ms", elapsed.as_secs_f64() * 1000.0);
                    }
                }
            }
            Ok(None) => {
                eprintln!("server closed connection");
                break;
            }
            Err(e) => {
                eprintln!("read error: {e}");
                break;
            }
        }
    }

    // Best-effort goodbye
    let _ = Message::Disconnect.write_to(&mut writer).await;
    let _ = tokio::io::AsyncWriteExt::flush(&mut writer).await;

    rl.save_history(&hist).ok();
    eprintln!("\nBye!");
}

// ─── Output formatting ──────────────────────────────────────────────────────

fn print_local_result(result: &QueryResult) {
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

fn print_remote_result(msg: &Message) {
    match msg {
        Message::ResultRows { columns, rows } => {
            if rows.is_empty() {
                println!("(empty set)");
                return;
            }
            print_table(columns, rows);
        }
        Message::ResultScalar { value } => {
            println!("{value}");
        }
        Message::ResultOk { affected } => {
            println!(
                "{affected} row{} affected",
                if *affected == 1 { "" } else { "s" }
            );
        }
        Message::Error { message } => {
            eprintln!("Error: {message}");
        }
        other => {
            eprintln!("unexpected response: {other:?}");
        }
    }
}

fn print_table(columns: &[String], rows: &[Vec<String>]) {
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

fn format_value(v: &Value) -> String {
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
        Value::Empty => "NULL".into(),
    }
}
