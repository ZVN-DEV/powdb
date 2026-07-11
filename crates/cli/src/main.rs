use powdb_query::executor::Engine;
use powdb_query::lexer::{split_statements, POWQL_KEYWORDS};
use powdb_query::result::QueryResult;
use powdb_server::protocol::Message;
use powdb_storage::types::Value;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Editor, Helper};
use std::borrow::Cow;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::io::{BufReader, BufWriter};
use tokio::net::TcpStream;
use tracing_subscriber::EnvFilter;

// ─── Tab completion helper ─────────────────────────────────────────────────

const CLI_COMMANDS: &[&str] = &["exec", "prepare"];
const DEFAULT_DB_NAME: &str = "default";
const META_COMMANDS: &[&str] = &[".exit", ".help", ".quit", ".schema", ".tables", ".timing"];

fn archive_wal_records_if_sync_enabled(
    data_dir: &Path,
    records: &[powdb_storage::wal::WalRecord],
) -> io::Result<()> {
    match powdb_sync::read_identity(data_dir) {
        Ok(identity) => powdb_sync::archive_wal_records_for_identity(data_dir, identity, records),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

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
        } else if start == 0 && !word.is_empty() {
            push_keyword_matches(
                POWQL_KEYWORDS.iter().chain(CLI_COMMANDS.iter()),
                &lower,
                first_char_uppercase(word),
                &mut matches,
            );
        } else if !word.is_empty() {
            push_keyword_matches(
                POWQL_KEYWORDS.iter(),
                &lower,
                first_char_uppercase(word),
                &mut matches,
            );
        }

        Ok((start, matches))
    }
}

fn first_char_uppercase(word: &str) -> bool {
    word.chars().next().is_some_and(|c| c.is_uppercase())
}

/// Push completion candidates for every keyword that prefix-matches `lower`,
/// preserving the case style the user typed (leading uppercase is echoed back).
fn push_keyword_matches<'a>(
    keywords: impl Iterator<Item = &'a &'a str>,
    lower: &str,
    uppercase_first: bool,
    matches: &mut Vec<Pair>,
) {
    for kw in keywords {
        if kw.starts_with(lower) {
            let replacement = if uppercase_first {
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

enum Action {
    /// Default behaviour: REPL or one-shot `--exec`.
    Default,
    /// Snapshot the embedded DB at `--data-dir` into `dest`.
    /// When `base` is set, write an incremental (differential) backup
    /// diffed against the full backup at that directory.
    Backup { dest: String, base: Option<String> },
    /// Rebuild a data dir from a backup. When `apply` is non-empty, treat
    /// `backup_dir` as a full base and apply the ordered increments on top.
    Restore {
        backup_dir: String,
        dest: String,
        apply: Vec<String>,
        sync_mode: powdb_backup::RestoreSyncMode,
    },
    /// Offline sync bootstrap: create this data dir's sync identity.
    SyncEnable,
    /// Restore a sync-enabled backup into a replica and publish its cursor.
    SyncBootstrap {
        backup_dir: String,
        replica_dir: String,
        replica_id: String,
    },
    /// Offline/admin: inspect primary-side sync cursor status.
    SyncStatus { replica_id: Option<String> },
    /// Offline user-admin: create a user in the data dir's UserStore.
    UserAdd { name: String },
    /// Offline user-admin: delete a user from the data dir's UserStore.
    UserDel { name: String },
    /// Offline user-admin: change a user's password in the data dir's UserStore.
    Passwd { name: String },
    /// Offline user-admin: list users (name + role) from the data dir's UserStore.
    Users,
}

struct CliArgs {
    data_dir: String,
    remote: Option<String>,
    db: String,
    password: Option<String>,
    user: Option<String>,
    /// Role for the `useradd` subcommand (defaults to "readwrite").
    role: Option<String>,
    exec: Option<String>,
    action: Action,
}

fn set_restore_sync_mode(
    current: &mut powdb_backup::RestoreSyncMode,
    was_set: &mut bool,
    next: powdb_backup::RestoreSyncMode,
    flag: &str,
) {
    if *was_set && *current != next {
        eprintln!("Error: conflicting restore sync identity mode flag: {flag}");
        std::process::exit(2);
    }
    *current = next;
    *was_set = true;
}

fn restore_sync_mode_for_flag(flag: &str) -> Option<powdb_backup::RestoreSyncMode> {
    match flag {
        "--sync-strip" => Some(powdb_backup::RestoreSyncMode::StripSyncIdentity),
        "--sync-preserve" => Some(powdb_backup::RestoreSyncMode::PreserveSyncIdentity),
        "--sync-fork" => Some(powdb_backup::RestoreSyncMode::ForkWithNewSyncIdentity),
        _ => None,
    }
}

fn parse_args() -> CliArgs {
    let mut data_dir = "./powdb_data".to_string();
    let mut remote: Option<String> = None;
    let mut db: String = DEFAULT_DB_NAME.to_string();
    let mut password: Option<String> = std::env::var("POWDB_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty());
    let mut user: Option<String> = None;
    let mut role: Option<String> = None;
    let mut exec: Option<String> = None;
    let mut exec_file: Option<String> = None;
    let mut action = Action::Default;
    // Accumulators for backup/restore modifier flags, which may appear after
    // the subcommand and its positionals.
    let mut backup_base: Option<String> = None;
    let mut restore_apply: Vec<String> = Vec::new();
    let mut restore_sync_mode = powdb_backup::RestoreSyncMode::StripSyncIdentity;
    let mut restore_sync_mode_was_set = false;

    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    let mut saw_positional = false;
    while i < argv.len() {
        match argv[i].as_str() {
            "--exec" | "-c" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: --exec requires a PowQL query");
                    std::process::exit(2);
                }
                exec = Some(argv[i].clone());
            }
            "--exec-file" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: --exec-file requires a path ('-' for stdin)");
                    std::process::exit(2);
                }
                exec_file = Some(argv[i].clone());
            }
            "--remote" | "-r" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: --remote requires host:port");
                    std::process::exit(2);
                }
                remote = Some(argv[i].clone());
            }
            "--db" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: --db requires a name");
                    std::process::exit(2);
                }
                db = argv[i].clone();
            }
            "--password" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: --password requires a value");
                    std::process::exit(2);
                }
                password = Some(argv[i].clone());
            }
            "--user" | "-u" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: --user requires a name");
                    std::process::exit(2);
                }
                user = Some(argv[i].clone());
            }
            "--role" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: --role requires a value");
                    std::process::exit(2);
                }
                role = Some(argv[i].clone());
            }
            "--data-dir" | "-d" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: --data-dir requires a path");
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
                println!("    powdb-cli --data-dir <DIR> backup <DEST_DIR> [--base <FULL_DIR>]");
                println!(
                    "    powdb-cli restore <BACKUP_DIR> <DEST_DATA_DIR> [--apply <INC_DIR>]... [--sync-strip|--sync-preserve|--sync-fork]"
                );
                println!(
                    "    powdb-cli --data-dir <PRIMARY_DIR> sync-bootstrap <BACKUP_DIR> <REPLICA_DIR> <REPLICA_ID>"
                );
                println!("    powdb-cli --data-dir <PRIMARY_DIR> sync-status [REPLICA_ID]");
                println!();
                println!("OPTIONS:");
                println!("    -c, --exec <QUERY>         Run one or more `;`-separated PowQL statements and exit");
                println!("        --exec-file <PATH>     Run PowQL read from a file ('-' for stdin) and exit");
                println!("    -r, --remote <HOST:PORT>   Connect to a remote server over TCP");
                println!("        --db <NAME>            Database name (default: default)");
                println!("        --password <PW>        Password for remote auth");
                println!("    -u, --user <NAME>          Username for multi-user remote auth");
                println!(
                    "    -d, --data-dir <PATH>      Embedded data dir (default: ./powdb_data)"
                );
                println!("    -h, --help                 Print this message");
                println!("    -V, --version              Print version and exit");
                println!();
                println!("MODES:");
                println!("    Embedded REPL:       powdb-cli ./mydata");
                println!(
                    "    Remote REPL:         powdb-cli --remote 127.0.0.1:5433 --password secret"
                );
                println!("    One-shot:            powdb-cli --exec 'count(User)'");
                println!("    Load a file:         powdb-cli --data-dir ./sandbox --exec-file dump.powql");
                println!("    One-shot (remote):   powdb-cli -r 127.0.0.1:5433 -c 'User filter .age > 25 limit 5'");
                println!();
                println!("SUBCOMMANDS:");
                println!("    backup <DEST_DIR> [--base <FULL_DIR>]");
                println!("        Snapshot --data-dir into DEST_DIR. With --base, write an");
                println!("        incremental (differential) backup of only the 4 KB pages that");
                println!("        changed since the full backup at FULL_DIR.");
                println!("    restore <BKP> <DEST_DIR> [--apply <INC_DIR>]...");
                println!("        Rebuild a data dir from a backup. Pass --apply once per");
                println!("        increment (in order) to chain-restore a full base plus");
                println!("        incrementals for coarse point-in-time restore.");
                println!("        Sync identity modes for sync-enabled backups:");
                println!("          --sync-strip     Default. Restore data without sync identity.");
                println!(
                    "          --sync-preserve  Disaster recovery: keep source sync identity."
                );
                println!("          --sync-fork      Clone/fork: mint a fresh sync identity.");
                println!("    sync-enable");
                println!("        Offline/admin: create sync identity and checkpoint retained WAL");
                println!("        for --data-dir so future backups can bootstrap replicas.");
                println!("    sync-bootstrap <BKP> <REPLICA_DIR> <REPLICA_ID>");
                println!("        Offline/admin: restore a sync-enabled full backup into");
                println!("        REPLICA_DIR and publish REPLICA_ID's primary-side cursor.");
                println!("    sync-status [REPLICA_ID]");
                println!("        Offline/admin: show primary-side cursor, lag, and repair");
                println!("        action for one replica or every registered replica.");
                println!("        Uses sync-aware open and may archive/checkpoint pending WAL.");
                println!();
                println!("USER ADMIN (offline — operate on --data-dir's user store):");
                println!("    useradd <NAME> --role <ROLE> --password <PW>");
                println!("        Create a user. --role defaults to readwrite (admin|readwrite|");
                println!("        readonly). Password from --password or POWDB_NEW_PASSWORD.");
                println!("    userdel <NAME>");
                println!("        Delete a user.");
                println!("    passwd <NAME> --password <PW>");
                println!("        Change a user's password (or via POWDB_NEW_PASSWORD).");
                println!("    users");
                println!("        List users (name + role).");
                std::process::exit(0);
            }
            "backup" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: backup requires a destination dir");
                    std::process::exit(2);
                }
                action = Action::Backup {
                    dest: argv[i].clone(),
                    base: None,
                };
            }
            "--base" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: --base requires a full backup dir");
                    std::process::exit(2);
                }
                backup_base = Some(argv[i].clone());
            }
            "--apply" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: --apply requires an increment dir");
                    std::process::exit(2);
                }
                restore_apply.push(argv[i].clone());
            }
            "--sync-strip" => {
                set_restore_sync_mode(
                    &mut restore_sync_mode,
                    &mut restore_sync_mode_was_set,
                    powdb_backup::RestoreSyncMode::StripSyncIdentity,
                    "--sync-strip",
                );
            }
            "--sync-preserve" => {
                set_restore_sync_mode(
                    &mut restore_sync_mode,
                    &mut restore_sync_mode_was_set,
                    powdb_backup::RestoreSyncMode::PreserveSyncIdentity,
                    "--sync-preserve",
                );
            }
            "--sync-fork" => {
                set_restore_sync_mode(
                    &mut restore_sync_mode,
                    &mut restore_sync_mode_was_set,
                    powdb_backup::RestoreSyncMode::ForkWithNewSyncIdentity,
                    "--sync-fork",
                );
            }
            "restore" => {
                i += 1;
                let mut backup_dir: Option<String> = None;
                let mut dest: Option<String> = None;
                while i < argv.len() {
                    let arg = argv[i].as_str();
                    if arg == "--apply" {
                        i += 1;
                        if i >= argv.len() {
                            eprintln!("Error: --apply requires an increment dir");
                            std::process::exit(2);
                        }
                        restore_apply.push(argv[i].clone());
                    } else if let Some(next) = restore_sync_mode_for_flag(arg) {
                        set_restore_sync_mode(
                            &mut restore_sync_mode,
                            &mut restore_sync_mode_was_set,
                            next,
                            arg,
                        );
                    } else if arg.starts_with('-') {
                        eprintln!("Error: unknown restore argument: {arg}");
                        eprintln!("try --help");
                        std::process::exit(2);
                    } else if backup_dir.is_none() {
                        backup_dir = Some(argv[i].clone());
                    } else if dest.is_none() {
                        dest = Some(argv[i].clone());
                    } else {
                        eprintln!("Error: unexpected restore argument: {arg}");
                        eprintln!("try --help");
                        std::process::exit(2);
                    }
                    i += 1;
                }
                let Some(backup_dir) = backup_dir else {
                    eprintln!("Error: restore requires a backup dir and a destination data dir");
                    std::process::exit(2);
                };
                let Some(dest) = dest else {
                    eprintln!("Error: restore requires a destination data dir");
                    std::process::exit(2);
                };
                action = Action::Restore {
                    backup_dir,
                    dest,
                    apply: Vec::new(),
                    sync_mode: powdb_backup::RestoreSyncMode::StripSyncIdentity,
                };
            }
            "sync-enable" => {
                action = Action::SyncEnable;
            }
            "sync-bootstrap" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: sync-bootstrap requires a backup dir");
                    std::process::exit(2);
                }
                let backup_dir = argv[i].clone();
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: sync-bootstrap requires a replica data dir");
                    std::process::exit(2);
                }
                let replica_dir = argv[i].clone();
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: sync-bootstrap requires a replica id");
                    std::process::exit(2);
                }
                action = Action::SyncBootstrap {
                    backup_dir,
                    replica_dir,
                    replica_id: argv[i].clone(),
                };
            }
            "sync-status" => {
                i += 1;
                let mut replica_id: Option<String> = None;
                while i < argv.len() {
                    let arg = argv[i].as_str();
                    if arg.starts_with('-') {
                        eprintln!("Error: unknown sync-status argument: {arg}");
                        eprintln!("try --help");
                        std::process::exit(2);
                    } else if replica_id.is_none() {
                        replica_id = Some(argv[i].clone());
                    } else {
                        eprintln!("Error: unexpected sync-status argument: {arg}");
                        eprintln!("try --help");
                        std::process::exit(2);
                    }
                    i += 1;
                }
                action = Action::SyncStatus { replica_id };
            }
            "useradd" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: useradd requires a user name");
                    std::process::exit(2);
                }
                action = Action::UserAdd {
                    name: argv[i].clone(),
                };
            }
            "userdel" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: userdel requires a user name");
                    std::process::exit(2);
                }
                action = Action::UserDel {
                    name: argv[i].clone(),
                };
            }
            "passwd" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: passwd requires a user name");
                    std::process::exit(2);
                }
                action = Action::Passwd {
                    name: argv[i].clone(),
                };
            }
            "users" => {
                action = Action::Users;
            }
            other if !other.starts_with('-') && !saw_positional => {
                data_dir = other.to_string();
                saw_positional = true;
            }
            other => {
                eprintln!("Error: unknown argument: {other}");
                eprintln!("try --help");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    // Fold the modifier flags into their subcommand actions.
    match &mut action {
        Action::Backup { base, .. } => {
            if !restore_apply.is_empty() {
                eprintln!("Error: --apply is only valid with the `restore` subcommand");
                std::process::exit(2);
            }
            if restore_sync_mode_was_set {
                eprintln!("Error: --sync-strip, --sync-preserve, and --sync-fork are only valid with the `restore` subcommand");
                std::process::exit(2);
            }
            *base = backup_base;
        }
        Action::Restore {
            apply, sync_mode, ..
        } => {
            if backup_base.is_some() {
                eprintln!("Error: --base is only valid with the `backup` subcommand");
                std::process::exit(2);
            }
            *apply = restore_apply;
            *sync_mode = restore_sync_mode;
        }
        _ => {
            if backup_base.is_some() {
                eprintln!("Error: --base is only valid with the `backup` subcommand");
                std::process::exit(2);
            }
            if !restore_apply.is_empty() {
                eprintln!("Error: --apply is only valid with the `restore` subcommand");
                std::process::exit(2);
            }
            if restore_sync_mode_was_set {
                eprintln!("Error: --sync-strip, --sync-preserve, and --sync-fork are only valid with the `restore` subcommand");
                std::process::exit(2);
            }
        }
    }

    // `--exec-file <PATH>` reads a whole PowQL file (or stdin for `-`) and
    // feeds it through the same one-shot path as `--exec`, sidestepping the
    // ARG_MAX ceiling on large loads. The two flags are mutually exclusive.
    if let Some(path) = exec_file {
        if exec.is_some() {
            eprintln!("Error: --exec and --exec-file are mutually exclusive");
            std::process::exit(2);
        }
        let contents = if path == "-" {
            let mut buf = String::new();
            if let Err(e) = io::Read::read_to_string(&mut io::stdin(), &mut buf) {
                eprintln!("Error: failed to read PowQL from stdin: {e}");
                std::process::exit(1);
            }
            buf
        } else {
            match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error: failed to read {path}: {e}");
                    std::process::exit(1);
                }
            }
        };
        exec = Some(contents);
    }

    CliArgs {
        data_dir,
        remote,
        db,
        password,
        user,
        role,
        exec,
        action,
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

    match &args.action {
        Action::Backup { dest, base } => {
            std::process::exit(run_backup(&args.data_dir, dest, base.as_deref()));
        }
        Action::Restore {
            backup_dir,
            dest,
            apply,
            sync_mode,
        } => {
            std::process::exit(run_restore(backup_dir, dest, apply, *sync_mode));
        }
        Action::SyncEnable => {
            std::process::exit(run_sync_enable(&args.data_dir));
        }
        Action::SyncBootstrap {
            backup_dir,
            replica_dir,
            replica_id,
        } => {
            std::process::exit(run_sync_bootstrap(
                &args.data_dir,
                backup_dir,
                replica_dir,
                replica_id,
            ));
        }
        Action::SyncStatus { replica_id } => {
            std::process::exit(run_sync_status(&args.data_dir, replica_id.as_deref()));
        }
        Action::UserAdd { name } => {
            std::process::exit(run_useradd(
                &args.data_dir,
                name,
                args.role.as_deref(),
                args.password.as_deref(),
            ));
        }
        Action::UserDel { name } => {
            std::process::exit(run_userdel(&args.data_dir, name));
        }
        Action::Passwd { name } => {
            std::process::exit(run_passwd(&args.data_dir, name, args.password.as_deref()));
        }
        Action::Users => {
            std::process::exit(run_users(&args.data_dir));
        }
        Action::Default => {}
    }

    if let Some(remote_addr) = &args.remote {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("Error: failed to build tokio runtime: {e}");
                std::process::exit(1);
            }
        };
        if let Some(query) = args.exec.clone() {
            let code = rt.block_on(exec_remote(
                remote_addr.clone(),
                args.db.clone(),
                args.password.clone(),
                args.user.clone(),
                query,
            ));
            std::process::exit(code);
        }
        rt.block_on(run_remote(
            remote_addr.clone(),
            args.db.clone(),
            args.password.clone(),
            args.user.clone(),
        ));
    } else if let Some(query) = args.exec {
        std::process::exit(exec_embedded(&args.data_dir, &query));
    } else {
        run_embedded(&args.data_dir);
    }
}

// ─── Backup / restore ───────────────────────────────────────────────────────

fn run_backup(data_dir: &str, dest: &str, base: Option<&str>) -> i32 {
    let mut catalog = match powdb_sync::open_preserving_retained_segments(Path::new(data_dir)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: failed to open data dir {data_dir}: {e}");
            return 1;
        }
    };

    match base {
        // ── Incremental (differential) backup against a full base ──────────
        Some(full_dir) => {
            let base_manifest = match powdb_backup::BackupManifest::read(Path::new(full_dir)) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("Error: failed to read base backup {full_dir}: {e}");
                    return 1;
                }
            };
            let base_lsn = base_manifest.source_lsn;
            match powdb_backup::incremental_backup(&mut catalog, &base_manifest, Path::new(dest)) {
                Ok(m) => {
                    use powdb_backup::ChangedFile;
                    let mut delta_pages: usize = 0;
                    let mut whole_files: usize = 0;
                    for cf in &m.changed {
                        match cf {
                            ChangedFile::Pages { page_indices, .. } => {
                                delta_pages += page_indices.len();
                            }
                            ChangedFile::Whole { .. } => {
                                whole_files += 1;
                            }
                        }
                    }
                    println!(
                        "incremental backup: {} changed files ({whole_files} whole, {} paged), \
                         {delta_pages} delta pages, base lsn {base_lsn} -> lsn {} -> {dest}",
                        m.changed.len(),
                        m.changed.len() - whole_files,
                        m.source_lsn
                    );
                    0
                }
                Err(e) => {
                    eprintln!("Error: incremental backup failed: {e}");
                    1
                }
            }
        }
        // ── Full backup ────────────────────────────────────────────────────
        None => match powdb_backup::full_backup(&mut catalog, Path::new(dest)) {
            Ok(m) => {
                let total_bytes: u64 = m.files.iter().map(|f| f.len).sum();
                println!(
                    "backed up {} files ({total_bytes} bytes) at lsn {} -> {dest}",
                    m.files.len(),
                    m.source_lsn
                );
                0
            }
            Err(e) => {
                eprintln!("Error: backup failed: {e}");
                1
            }
        },
    }
}

fn run_restore(
    backup_dir: &str,
    dest: &str,
    apply: &[String],
    sync_mode: powdb_backup::RestoreSyncMode,
) -> i32 {
    if apply.is_empty() {
        // Full restore.
        return match powdb_backup::restore_with_sync_mode(
            Path::new(backup_dir),
            Path::new(dest),
            sync_mode,
        ) {
            Ok(()) => {
                println!("restored backup {backup_dir} -> {dest}");
                0
            }
            Err(e) => {
                eprintln!("Error: restore failed: {e}");
                1
            }
        };
    }

    // Chain restore: full base + ordered increments.
    let increments: Vec<&Path> = apply.iter().map(|s| Path::new(s.as_str())).collect();
    match powdb_backup::restore_chain_with_sync_mode(
        Path::new(backup_dir),
        &increments,
        Path::new(dest),
        sync_mode,
    ) {
        Ok(()) => {
            println!(
                "restored backup {backup_dir} + {} increment(s) -> {dest}",
                apply.len()
            );
            for inc in apply {
                println!("  applied {inc}");
            }
            0
        }
        Err(e) => {
            eprintln!("Error: chain restore failed: {e}");
            1
        }
    }
}

fn run_sync_enable(data_dir: &str) -> i32 {
    let mut catalog = match powdb_sync::open_preserving_retained_segments(Path::new(data_dir)) {
        Ok(catalog) => catalog,
        Err(e) => {
            eprintln!("Error: failed to open data dir {data_dir}: {e}");
            return 1;
        }
    };
    if let Err(e) = powdb_sync::checkpoint_with_retained_segments(&mut catalog) {
        eprintln!("Error: sync enable failed: {e}");
        return 1;
    }
    match powdb_sync::read_identity_snapshot(Path::new(data_dir)) {
        Ok(Some(snapshot)) => {
            println!(
                "sync enabled: database {} generation {} at lsn {}",
                snapshot.database_id,
                snapshot.primary_generation,
                catalog.max_lsn()
            );
            0
        }
        Ok(None) => {
            eprintln!("Error: sync enable did not create identity metadata");
            1
        }
        Err(e) => {
            eprintln!("Error: failed to read sync identity: {e}");
            1
        }
    }
}

fn run_sync_bootstrap(
    primary_dir: &str,
    backup_dir: &str,
    replica_dir: &str,
    replica_id: &str,
) -> i32 {
    let mut primary = match powdb_sync::open_preserving_retained_segments(Path::new(primary_dir)) {
        Ok(catalog) => catalog,
        Err(e) => {
            eprintln!("Error: failed to open primary data dir {primary_dir}: {e}");
            return 1;
        }
    };
    match powdb_backup::bootstrap_replica_from_full_backup(
        &mut primary,
        Path::new(backup_dir),
        Path::new(replica_dir),
        replica_id,
    ) {
        Ok(summary) => {
            println!(
                "sync replica bootstrapped: {} snapshot lsn {} remote lsn {} retained units {} -> {}",
                summary.replica_id,
                summary.snapshot_lsn,
                summary.remote_lsn,
                summary.retained_units_available,
                replica_dir
            );
            0
        }
        Err(e) => {
            eprintln!("Error: sync bootstrap failed: {e}");
            1
        }
    }
}

fn format_optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "null".to_string(), |n| n.to_string())
}

fn format_sync_repair_action(action: powdb_sync::SyncRepairAction) -> &'static str {
    match action {
        powdb_sync::SyncRepairAction::None => "none",
        powdb_sync::SyncRepairAction::Pull => "pull",
        powdb_sync::SyncRepairAction::AwaitArchive => "awaitArchive",
        powdb_sync::SyncRepairAction::Rebootstrap => "rebootstrap",
    }
}

fn print_replica_sync_status(status: &powdb_sync::ReplicaSyncStatus) {
    println!("replica {}", status.replica_id);
    println!("  active: {}", status.active);
    println!(
        "  lastAppliedLsn: {}",
        format_optional_u64(status.last_applied_lsn)
    );
    println!("  remoteLsn: {}", status.remote_lsn);
    println!(
        "  servableLsn: {}",
        format_optional_u64(status.servable_lsn)
    );
    println!(
        "  unarchivedLsn: {}",
        format_optional_u64(status.unarchived_lsn)
    );
    println!("  lagLsn: {}", format_optional_u64(status.lag_lsn));
    println!("  lagBytes: {}", format_optional_u64(status.lag_bytes));
    println!("  lagMs: {}", format_optional_u64(status.lag_ms));
    println!("  stale: {}", status.stale);
    println!(
        "  repairAction: {}",
        format_sync_repair_action(status.repair_action)
    );
    if let Some(err) = &status.last_sync_error {
        println!("  lastSyncError: {err}");
    }
}

fn run_sync_status(data_dir: &str, replica_id: Option<&str>) -> i32 {
    let catalog = match powdb_sync::open_preserving_retained_segments(Path::new(data_dir)) {
        Ok(catalog) => catalog,
        Err(e) => {
            eprintln!("Error: failed to open data dir {data_dir}: {e}");
            return 1;
        }
    };
    let remote_lsn = catalog.max_lsn();

    let identity = match powdb_sync::read_identity_snapshot_if_exists(Path::new(data_dir)) {
        Ok(Some(identity)) => identity,
        Ok(None) => {
            eprintln!("Error: sync-status requires a sync-enabled data dir; run sync-enable first");
            return 1;
        }
        Err(e) => {
            eprintln!("Error: failed to read sync identity: {e}");
            return 1;
        }
    };

    println!(
        "sync status: database {} generation {} remoteLsn {}",
        identity.database_id, identity.primary_generation, remote_lsn
    );

    if let Some(replica_id) = replica_id {
        match powdb_sync::replica_sync_status(Path::new(data_dir), replica_id, remote_lsn) {
            Ok(status) => {
                print_replica_sync_status(&status);
                0
            }
            Err(e) => {
                eprintln!("Error: failed to read sync status for {replica_id}: {e}");
                1
            }
        }
    } else {
        let mut cursors = match powdb_sync::read_replica_cursors(Path::new(data_dir)) {
            Ok(cursors) => cursors,
            Err(e) => {
                eprintln!("Error: failed to read replica cursors: {e}");
                return 1;
            }
        };
        cursors.sort_by(|a, b| a.replica_id.cmp(&b.replica_id));
        println!("replicas: {}", cursors.len());
        for cursor in cursors {
            match powdb_sync::replica_sync_status(
                Path::new(data_dir),
                &cursor.replica_id,
                remote_lsn,
            ) {
                Ok(status) => print_replica_sync_status(&status),
                Err(e) => {
                    eprintln!(
                        "Error: failed to read sync status for {}: {e}",
                        cursor.replica_id
                    );
                    return 1;
                }
            }
        }
        0
    }
}

// ─── User administration (offline / embedded) ───────────────────────────────

/// Resolve a new-user/new-password value from `--password` or, failing that,
/// the `POWDB_NEW_PASSWORD` env var. Returns `None` when neither is set.
fn resolve_new_password(flag: Option<&str>) -> Option<String> {
    flag.map(|s| s.to_string()).or_else(|| {
        std::env::var("POWDB_NEW_PASSWORD")
            .ok()
            .filter(|s| !s.is_empty())
    })
}

fn run_useradd(data_dir: &str, name: &str, role: Option<&str>, password: Option<&str>) -> i32 {
    let role = role.unwrap_or("readwrite");
    let Some(pw) = resolve_new_password(password) else {
        eprintln!(
            "Error: a password is required \u{2014} pass --password <PW> or set POWDB_NEW_PASSWORD"
        );
        return 2;
    };
    let dir = Path::new(data_dir);
    let mut store = match powdb_auth::UserStore::load(dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: failed to load user store from {data_dir}: {e}");
            return 1;
        }
    };
    if let Err(e) = store.create_user(name, &pw, role) {
        eprintln!("Error: {e}");
        return 1;
    }
    if let Err(e) = store.save(dir) {
        eprintln!("Error: failed to save user store: {e}");
        return 1;
    }
    println!("user '{name}' created (role {role})");
    0
}

fn run_userdel(data_dir: &str, name: &str) -> i32 {
    let dir = Path::new(data_dir);
    let mut store = match powdb_auth::UserStore::load(dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: failed to load user store from {data_dir}: {e}");
            return 1;
        }
    };
    if let Err(e) = store.delete_user(name) {
        eprintln!("Error: {e}");
        return 1;
    }
    if let Err(e) = store.save(dir) {
        eprintln!("Error: failed to save user store: {e}");
        return 1;
    }
    println!("user '{name}' deleted");
    0
}

fn run_passwd(data_dir: &str, name: &str, password: Option<&str>) -> i32 {
    let Some(pw) = resolve_new_password(password) else {
        eprintln!(
            "Error: a password is required \u{2014} pass --password <PW> or set POWDB_NEW_PASSWORD"
        );
        return 2;
    };
    let dir = Path::new(data_dir);
    let mut store = match powdb_auth::UserStore::load(dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: failed to load user store from {data_dir}: {e}");
            return 1;
        }
    };
    if let Err(e) = store.set_password(name, &pw) {
        eprintln!("Error: {e}");
        return 1;
    }
    if let Err(e) = store.save(dir) {
        eprintln!("Error: failed to save user store: {e}");
        return 1;
    }
    println!("password updated for user '{name}'");
    0
}

fn run_users(data_dir: &str) -> i32 {
    let dir = Path::new(data_dir);
    let store = match powdb_auth::UserStore::load(dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: failed to load user store from {data_dir}: {e}");
            return 1;
        }
    };
    let users = store.list_users();
    if users.is_empty() {
        println!("(no users \u{2014} shared-password mode)");
        return 0;
    }
    // Simple table: name + role. Never print password hashes.
    let name_w = users.iter().map(|(n, _)| n.len()).max().unwrap_or(4).max(4);
    let role_w = users.iter().map(|(_, r)| r.len()).max().unwrap_or(4).max(4);
    println!(" {:<name_w$} | {:<role_w$} ", "Name", "Role");
    println!("-{}-+-{}-", "-".repeat(name_w), "-".repeat(role_w));
    for (n, r) in &users {
        println!(" {n:<name_w$} | {r:<role_w$} ");
    }
    println!(
        "({} user{})",
        users.len(),
        if users.len() == 1 { "" } else { "s" }
    );
    0
}

// ─── One-shot execution (embedded) ──────────────────────────────────────────

fn exec_embedded(data_dir: &str, query: &str) -> i32 {
    let mut engine = match Engine::new_with_wal_archive(
        Path::new(data_dir),
        archive_wal_records_if_sync_enabled,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Error: failed to initialize engine: {e}");
            return 1;
        }
    };
    // Statement-aware splitting (#150): a `;` inside a string literal or a
    // `#` comment is not a boundary, so text-heavy rows load intact.
    let statements = split_statements(query);
    for stmt in &statements {
        if stmt.starts_with('.') {
            let cmd = stmt.split_whitespace().next().unwrap_or(stmt);
            eprintln!(
                "Error: '{}' is a REPL-only command \u{2014} start the interactive REPL without -c to use it",
                cmd
            );
            return 1;
        }
        match engine.execute_powql(stmt) {
            Ok(result) => {
                print_local_result(&result);
            }
            Err(e) => {
                eprintln!("Error: {e}");
                return 1;
            }
        }
    }
    0
}

// ─── One-shot execution (remote) ────────────────────────────────────────────

async fn exec_remote(
    addr: String,
    db: String,
    password: Option<String>,
    username: Option<String>,
    query: String,
) -> i32 {
    let stream = match TcpStream::connect(&addr).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: connection failed: {e}");
            return 1;
        }
    };
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    let connect = Message::Connect {
        db_name: db,
        password: password.map(Into::into),
        username,
    };
    if connect.write_to(&mut writer).await.is_err()
        || tokio::io::AsyncWriteExt::flush(&mut writer).await.is_err()
    {
        eprintln!("Error: failed to send CONNECT");
        return 1;
    }
    match Message::read_from(&mut reader).await {
        Ok(Some(Message::ConnectOk { .. })) => {}
        Ok(Some(Message::Error { message })) => {
            eprintln!("Error: server rejected connection: {message}");
            return 1;
        }
        _ => {
            eprintln!("Error: handshake failed");
            return 1;
        }
    }

    // The wire protocol carries one statement per `Query` message, so split
    // client-side (#150) and send each in turn, stopping on the first error.
    let mut code = 0;
    for stmt in split_statements(&query) {
        if stmt.starts_with('.') {
            let cmd = stmt.split_whitespace().next().unwrap_or(stmt);
            eprintln!(
                "Error: '{}' is a REPL-only command \u{2014} start the interactive REPL without -c to use it",
                cmd
            );
            code = 1;
            break;
        }

        let q = Message::Query {
            query: stmt.to_string(),
        };
        if q.write_to(&mut writer).await.is_err()
            || tokio::io::AsyncWriteExt::flush(&mut writer).await.is_err()
        {
            eprintln!("Error: write failed");
            code = 1;
            break;
        }

        match Message::read_from(&mut reader).await {
            Ok(Some(msg)) => {
                let is_error = matches!(msg, Message::Error { .. });
                print_remote_result(&msg);
                if is_error {
                    code = 1;
                    break;
                }
            }
            Ok(None) => {
                eprintln!("Error: server closed connection");
                code = 1;
                break;
            }
            Err(e) => {
                eprintln!("Error: read failed: {e}");
                code = 1;
                break;
            }
        }
    }

    let _ = Message::Disconnect.write_to(&mut writer).await;
    let _ = tokio::io::AsyncWriteExt::flush(&mut writer).await;
    code
}

// ─── Multi-line input ───────────────────────────────────────────────────────

/// True when `buffer` has unbalanced `{`/`(` outside string literals, i.e. the
/// REPL should read another line before executing. String literals follow the
/// lexer's rules (`crates/query/src/lexer.rs`): a backslash escapes the next
/// character, so `\"` inside a string does not terminate it.
fn needs_continuation(buffer: &str) -> bool {
    let mut depth: i64 = 0;
    let mut in_str = false;
    let mut chars = buffer.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_str => in_str = false,
            '"' => in_str = true,
            // Lexer treats backslash as an escape inside strings; skip the
            // escaped char so `\"` doesn't toggle the string state.
            '\\' if in_str => {
                chars.next();
            }
            '{' | '(' if !in_str => depth += 1,
            '}' | ')' if !in_str => depth -= 1,
            _ => {}
        }
    }
    depth > 0 || in_str
}

// ─── Embedded mode ──────────────────────────────────────────────────────────

fn run_embedded(data_dir: &str) {
    eprintln!("PowDB v{} — embedded mode", env!("CARGO_PKG_VERSION"));
    eprintln!("Data directory: {data_dir}");
    eprintln!("Type PowQL queries. Use Ctrl-D to exit. Type .help for commands.\n");

    let mut engine = match Engine::new_with_wal_archive(
        Path::new(data_dir),
        archive_wal_records_if_sync_enabled,
    ) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("Error: failed to initialize engine: {e}");
            std::process::exit(1);
        }
    };

    let mut rl = match Editor::new() {
        Ok(rl) => rl,
        Err(e) => {
            eprintln!("Error: failed to initialize readline: {e}");
            std::process::exit(1);
        }
    };
    rl.set_helper(Some(PowqlHelper));

    let hist = history_path();
    rl.load_history(&hist).ok();

    let mut timing_enabled = false;
    let mut buffer = String::new();

    loop {
        let prompt = if buffer.is_empty() {
            "powql> "
        } else {
            "  ...> "
        };
        let line = match rl.readline(prompt) {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(rustyline::error::ReadlineError::Interrupted) => {
                // Abandon any partial multi-line statement.
                buffer.clear();
                continue;
            }
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        };

        // Meta-commands are only recognized at the start of a statement.
        if buffer.is_empty() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // ── Meta-commands ──────────────────────────────────────────
            if trimmed.starts_with('.') {
                rl.add_history_entry(trimmed).ok();
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
                        println!("Timing is {}.", if timing_enabled { "on" } else { "off" });
                    }
                    cmd if cmd.starts_with(".schema") => {
                        let table_name = cmd.strip_prefix(".schema").unwrap().trim();
                        if table_name.is_empty() {
                            eprintln!("Usage: .schema <TABLE_NAME>");
                        } else if let Some(schema) = engine.catalog().schema(table_name) {
                            println!("Table: {}", schema.table_name);
                            println!("  {:<20} {:<12} Required", "Column", "Type");
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
        }

        // Accumulate input until braces/parens balance outside strings.
        buffer.push_str(&line);
        buffer.push('\n');
        if needs_continuation(&buffer) {
            continue;
        }

        let statement = buffer.trim().to_string();
        buffer.clear();
        if statement.is_empty() {
            continue;
        }
        rl.add_history_entry(&statement).ok();

        // ── Execute PowQL query ────────────────────────────────────────
        let start = Instant::now();
        match engine.execute_powql(&statement) {
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

async fn run_remote(addr: String, db: String, password: Option<String>, username: Option<String>) {
    eprintln!("PowDB v{} — remote mode", env!("CARGO_PKG_VERSION"));
    eprintln!("Connecting to {addr} ...");

    let stream = match TcpStream::connect(&addr).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: connection failed: {e}");
            std::process::exit(1);
        }
    };

    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    // Send CONNECT
    let connect = Message::Connect {
        db_name: db.clone(),
        password: password.map(Into::into),
        username,
    };
    if let Err(e) = connect.write_to(&mut writer).await {
        eprintln!("Error: failed to send CONNECT: {e}");
        std::process::exit(1);
    }
    if let Err(e) = tokio::io::AsyncWriteExt::flush(&mut writer).await {
        eprintln!("Error: flush failed: {e}");
        std::process::exit(1);
    }

    // Read CONNECT_OK or ERROR
    match Message::read_from(&mut reader).await {
        Ok(Some(Message::ConnectOk { version })) => {
            eprintln!("Connected to db `{db}` (server v{version})");
            eprintln!("Type PowQL queries. Use Ctrl-D to exit.\n");
        }
        Ok(Some(Message::Error { message })) => {
            eprintln!("Error: server rejected connection: {message}");
            std::process::exit(1);
        }
        Ok(Some(other)) => {
            eprintln!("Error: unexpected handshake reply: {other:?}");
            std::process::exit(1);
        }
        Ok(None) => {
            eprintln!("Error: server closed connection during handshake");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: handshake read failed: {e}");
            std::process::exit(1);
        }
    }

    let mut rl = match Editor::new() {
        Ok(rl) => rl,
        Err(e) => {
            eprintln!("Error: failed to initialize readline: {e}");
            std::process::exit(1);
        }
    };
    rl.set_helper(Some(PowqlHelper));

    let hist = history_path();
    rl.load_history(&hist).ok();

    let mut timing_enabled = false;
    let mut buffer = String::new();

    loop {
        let prompt = if buffer.is_empty() {
            "powql> "
        } else {
            "  ...> "
        };
        let line = match rl.readline(prompt) {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(rustyline::error::ReadlineError::Interrupted) => {
                buffer.clear();
                continue;
            }
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        };

        // Meta-commands are only recognized at the start of a statement.
        if buffer.is_empty() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Handle local-only meta-commands in remote mode
            if trimmed.starts_with('.') {
                rl.add_history_entry(trimmed).ok();
                match trimmed {
                    ".quit" | ".exit" => break,
                    ".timing" => {
                        timing_enabled = !timing_enabled;
                        println!("Timing is {}.", if timing_enabled { "on" } else { "off" });
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
                        eprintln!(
                            "Meta-commands (.tables, .schema) are not supported in remote mode."
                        );
                        eprintln!("Type .help for available commands.");
                    }
                }
                continue;
            }
        }

        // Accumulate input until braces/parens balance outside strings.
        buffer.push_str(&line);
        buffer.push('\n');
        if needs_continuation(&buffer) {
            continue;
        }

        let statement = buffer.trim().to_string();
        buffer.clear();
        if statement.is_empty() {
            continue;
        }
        rl.add_history_entry(&statement).ok();

        let q = Message::Query { query: statement };
        if q.write_to(&mut writer).await.is_err() {
            eprintln!("Error: write failed — disconnected");
            break;
        }
        if tokio::io::AsyncWriteExt::flush(&mut writer).await.is_err() {
            eprintln!("Error: flush failed — disconnected");
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
                eprintln!("Error: server closed connection");
                break;
            }
            Err(e) => {
                eprintln!("Error: read failed: {e}");
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

/// Render one wire cell for display. The server serializes NULL as the
/// bareword "null" (the sentinel the TS client's typed decoder matches);
/// remote mode renders it as `NULL`, matching the embedded REPL. A string
/// column whose *value* is literally "null" is indistinguishable on the
/// untyped wire — same tradeoff the TS client documents.
fn render_remote_cell(cell: &str) -> String {
    if cell == "null" {
        "NULL".into()
    } else {
        cell.into()
    }
}

fn print_remote_result(msg: &Message) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_null_sentinel_renders_as_null_like_embedded() {
        // The wire sends NULL as the bareword "null"; remote display must
        // match the embedded REPL's `NULL`.
        assert_eq!(render_remote_cell("null"), "NULL");
        assert_eq!(format_value(&Value::Empty), "NULL");
        // Ordinary values pass through untouched.
        assert_eq!(render_remote_cell("42"), "42");
        assert_eq!(render_remote_cell(""), "");
        assert_eq!(render_remote_cell("NULL"), "NULL");
    }

    #[test]
    fn remote_db_default_matches_ts_client() {
        assert_eq!(DEFAULT_DB_NAME, "default");
    }

    #[test]
    fn completion_keywords_include_current_lexer_surface() {
        for required in [
            "upsert",
            "conflict",
            "row_number",
            "dense_rank",
            "over",
            "partition",
            "date_add",
            "date_diff",
            "unique",
            "materialized",
            "explain",
        ] {
            assert!(
                POWQL_KEYWORDS.contains(&required),
                "lexer keyword missing from CLI completion source: {required}"
            );
        }
        assert!(CLI_COMMANDS.contains(&"exec"));
    }

    #[test]
    fn continuation_tracking() {
        assert!(needs_continuation("type User {"));
        assert!(needs_continuation("type User {\n  required name: str,"));
        assert!(!needs_continuation("type User { required name: str }"));
        // Brace inside a string literal must not count.
        assert!(!needs_continuation(r#"insert U { s := "}" }"#));
        assert!(needs_continuation(r#"insert U { s := "}" "#));
        // Parens.
        assert!(needs_continuation("count(User filter ("));
        assert!(!needs_continuation("count(User)"));
        // Nested.
        assert!(needs_continuation("insert U { a := (1 + "));
        // Over-closed input is NOT a continuation — let the parser error.
        assert!(!needs_continuation("User }"));
        // Escaped quote inside a string must not end the string.
        assert!(needs_continuation(r#"insert U { s := "a\" "#));
        assert!(!needs_continuation(r#"insert U { s := "a\"b" }"#));
    }
}
