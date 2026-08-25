//! Command-line surface: the subcommands, flags, and their parser.

use super::*;

pub(crate) enum Action {
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
    /// Offline admin: mark-and-sweep reclaim orphaned overflow pages. `table`
    /// is a table name, or "all" to sweep every table. Reports pages reclaimed.
    Sweep { table: String },
}

pub(crate) struct CliArgs {
    pub(crate) data_dir: String,
    pub(crate) remote: Option<String>,
    pub(crate) db: String,
    pub(crate) password: Option<String>,
    pub(crate) user: Option<String>,
    /// Role for the `useradd` subcommand (defaults to "readwrite").
    pub(crate) role: Option<String>,
    pub(crate) exec: Option<String>,
    /// Language `--exec` / `--exec-file` statements are written in, and the
    /// dialect the REPL starts in. Selected with `--sql` (default PowQL).
    pub(crate) dialect: Dialect,
    /// Result rendering for one-shot mode and the REPL's initial `.mode`.
    pub(crate) output: OutputMode,
    pub(crate) action: Action,
    pub(crate) tls: TlsOpts,
}

/// TLS settings for remote mode.
pub(crate) struct TlsOpts {
    /// Wrap the remote connection in TLS: `--tls`, implied by the other TLS
    /// flags, or the `POWDB_TLS` env fallback (truthy).
    pub(crate) enabled: bool,
    /// Custom root CA PEM (`--tls-ca` / `POWDB_TLS_CA`) for self-signed
    /// deployments; default is the built-in webpki (Mozilla) root store.
    pub(crate) ca_path: Option<String>,
    /// Certificate name override (`--tls-server-name` /
    /// `POWDB_TLS_SERVER_NAME`), for connecting by IP to a hostname cert.
    pub(crate) server_name: Option<String>,
}

/// Parse the `POWDB_TLS` env value. Truthy on `1`/`true`/`yes`/`on` (any
/// case), matching the server's `POWDB_REQUIRE_TLS` grammar; default off.
pub(crate) fn parse_tls_enabled(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

pub(crate) fn set_restore_sync_mode(
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

/// Every bare-word subcommand, for typo detection on a stray positional.
/// A word close to one of these is a mistake, not a data directory.
pub(crate) const SUBCOMMANDS: &[&str] = &[
    "backup",
    "restore",
    "sync-enable",
    "sync-bootstrap",
    "sync-status",
    "useradd",
    "userdel",
    "passwd",
    "users",
    "sweep",
];

/// Levenshtein edit distance, for "did you mean" on a mistyped subcommand.
/// Two rolling rows: the inputs here are single command words.
pub(crate) fn edit_distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut cur = vec![0usize; b_chars.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let substitution = prev[j] + usize::from(ca != *cb);
            cur[j + 1] = substitution.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b_chars.len()]
}

/// The subcommand `token` was probably meant to be, if any.
///
/// Deliberately conservative, because a bare positional is also the documented
/// `powdb-cli [DATA_DIR]` form: a token that looks like a path (or names a
/// directory that exists) is taken at face value, and the distance budget is
/// one edit for short words, two for longer ones. That still catches every
/// plausible slip (`usrs`, `backupp`, `restor`, `useradds`, `sync-statuss`)
/// without stealing a data dir someone genuinely named `backups`.
pub(crate) fn nearest_subcommand(token: &str) -> Option<&'static str> {
    if token.contains('/') || token.contains(std::path::MAIN_SEPARATOR) {
        return None;
    }
    if token.starts_with('.') || token.starts_with('~') || Path::new(token).exists() {
        return None;
    }
    let budget = if token.chars().count() <= 4 { 1 } else { 2 };
    SUBCOMMANDS
        .iter()
        .map(|cmd| (edit_distance(token, cmd), *cmd))
        .filter(|(distance, _)| *distance <= budget)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, cmd)| cmd)
}

pub(crate) fn restore_sync_mode_for_flag(flag: &str) -> Option<powdb_backup::RestoreSyncMode> {
    match flag {
        "--sync-strip" => Some(powdb_backup::RestoreSyncMode::StripSyncIdentity),
        "--sync-preserve" => Some(powdb_backup::RestoreSyncMode::PreserveSyncIdentity),
        "--sync-fork" => Some(powdb_backup::RestoreSyncMode::ForkWithNewSyncIdentity),
        _ => None,
    }
}

pub(crate) fn parse_args() -> CliArgs {
    let mut data_dir = "./powdb_data".to_string();
    let mut remote: Option<String> = None;
    let mut db: String = DEFAULT_DB_NAME.to_string();
    let mut password: Option<String> = std::env::var("POWDB_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty());
    let mut user: Option<String> = None;
    let mut role: Option<String> = None;
    // TLS for remote mode: env fallbacks first (same POWDB_-prefixed
    // convention as POWDB_PASSWORD above), overridden by the flags below.
    let mut tls_ca: Option<String> = std::env::var("POWDB_TLS_CA").ok().filter(|s| !s.is_empty());
    let mut tls_server_name: Option<String> = std::env::var("POWDB_TLS_SERVER_NAME")
        .ok()
        .filter(|s| !s.is_empty());
    let mut tls_enabled = tls_enabled_from_env(
        std::env::var("POWDB_TLS").ok().as_deref(),
        tls_ca.as_deref(),
        tls_server_name.as_deref(),
    );
    let mut exec: Option<String> = None;
    let mut exec_file: Option<String> = None;
    let mut dialect = Dialect::Powql;
    let mut output = OutputMode::Table;
    let mut action = Action::Default;
    // Accumulators for backup/restore modifier flags, which may appear after
    // the subcommand and its positionals.
    let mut backup_base: Option<String> = None;
    let mut restore_apply: Vec<String> = Vec::new();
    let mut restore_sync_mode = powdb_backup::RestoreSyncMode::StripSyncIdentity;
    let mut restore_sync_mode_was_set = false;

    // `std::env::args()` panics on a non-UTF-8 argument (it unwraps the
    // OsString→String conversion internally). Use `args_os` and reject
    // invalid UTF-8 with a clean error + exit code instead of a panic.
    let argv: Vec<String> = match std::env::args_os()
        .map(|a| a.into_string())
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(v) => v,
        Err(bad) => {
            eprintln!(
                "Error: argument is not valid UTF-8: {}",
                bad.to_string_lossy()
            );
            std::process::exit(2);
        }
    };
    let mut i = 1;
    let mut saw_positional = false;
    let mut data_dir_explicit = false;
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
            "--sql" => {
                dialect = Dialect::Sql;
            }
            "--format" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: --format requires one of table, json, csv");
                    std::process::exit(2);
                }
                match parse_output_mode(&argv[i]) {
                    Some(mode) => output = mode,
                    None => {
                        eprintln!(
                            "Error: unknown --format '{}' (want table, json, or csv)",
                            argv[i]
                        );
                        std::process::exit(2);
                    }
                }
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
            "--tls" => {
                tls_enabled = true;
            }
            "--tls-ca" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: --tls-ca requires a path");
                    std::process::exit(2);
                }
                tls_ca = Some(argv[i].clone());
                tls_enabled = true;
            }
            "--tls-server-name" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: --tls-server-name requires a name");
                    std::process::exit(2);
                }
                tls_server_name = Some(argv[i].clone());
                tls_enabled = true;
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
                data_dir_explicit = true;
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
                println!("                               (statements are separated by `;`, never by newlines)");
                println!("        --exec-file <PATH>     Run PowQL read from a file ('-' for stdin) and exit");
                println!("                               (same `;` rule: a newline continues a statement)");
                println!("        --sql                  Treat --exec / --exec-file input as SQL, and start");
                println!("                               the REPL in SQL mode (see docs/SQL.md for the subset)");
                println!("        --format <FMT>         Result rendering: table (default), json, or csv.");
                println!("                               json and csv make the CLI scriptable");
                println!("    -r, --remote <HOST:PORT>   Connect to a remote server over TCP");
                println!("        --db <NAME>            Database name (default: default)");
                println!("        --password <PW>        Password for remote auth");
                println!("    -u, --user <NAME>          Username for multi-user remote auth");
                println!("        --tls                  Encrypt the remote connection with TLS");
                println!("                               (env fallback: POWDB_TLS=1)");
                println!(
                    "        --tls-ca <PATH>        Root CA PEM to trust instead of the built-in"
                );
                println!(
                    "                               web roots; implies --tls. For self-signed"
                );
                println!("                               deployments (env fallback: POWDB_TLS_CA)");
                println!(
                    "        --tls-server-name <N>  Hostname to verify the server certificate"
                );
                println!(
                    "                               against; implies --tls. Use when connecting"
                );
                println!("                               by IP to a cert issued for a hostname");
                println!("                               (env fallback: POWDB_TLS_SERVER_NAME)");
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
                println!(
                    "    One-shot SQL:        powdb-cli --sql -c 'SELECT name FROM User LIMIT 5'"
                );
                println!("    Scriptable output:   powdb-cli --format json -c 'count(User)'");
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
                println!("    sweep <TABLE|all>");
                println!("        Reclaim orphaned overflow pages for a table (or all).");
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
            "sweep" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("Error: sweep requires a table name or \"all\"");
                    std::process::exit(2);
                }
                action = Action::Sweep {
                    table: argv[i].clone(),
                };
            }
            // A bare positional is the documented `powdb-cli [DATA_DIR]` form,
            // but it must never *silently* become a data directory. A mistyped
            // subcommand (`powdb-cli -d /var/lib/powdb usrs`) used to ignore
            // the explicit -d, create a fresh empty database in ./usrs, and
            // exit 0 — the operator's real database was never touched and
            // nothing said so.
            other if !other.starts_with('-') => {
                if saw_positional || data_dir_explicit {
                    eprintln!("Error: unexpected argument: {other}");
                    if let Some(near) = nearest_subcommand(other) {
                        eprintln!("note: did you mean the `{near}` subcommand?");
                    } else if data_dir_explicit {
                        eprintln!(
                            "note: the data directory is already set to '{data_dir}' by -d/--data-dir"
                        );
                    }
                    eprintln!("try --help");
                    std::process::exit(2);
                }
                if let Some(near) = nearest_subcommand(other) {
                    eprintln!("Error: unknown subcommand: {other}");
                    eprintln!("note: did you mean `{near}`?");
                    eprintln!(
                        "note: to use '{other}' as a data directory instead, pass it as -d {other}"
                    );
                    std::process::exit(2);
                }
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
        dialect,
        output,
        action,
        tls: TlsOpts {
            enabled: tls_enabled,
            ca_path: tls_ca,
            server_name: tls_server_name,
        },
    }
}
