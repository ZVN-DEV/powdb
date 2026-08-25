use powdb_query::executor::Engine;
use powdb_query::lexer::{split_statements, POWQL_KEYWORDS};
use powdb_query::result::QueryResult;
use powdb_server::protocol::{
    require_server_capabilities, ClientHello, Message, ServerHello, CLIENT_CATALOG_VERSION,
    FEATURE_NATIVE_TYPED, MIN_SUPPORTED_PROTOCOL_VERSION,
};
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

mod admin;
mod args;
mod embedded;
mod output;
mod remote;
mod repl;
#[cfg(test)]
mod tests;

// One namespace, as before the split: every module starts with
// `use super::*;` and main re-imports each module's items here.
use admin::*;
use args::*;
use embedded::*;
use output::*;
use remote::*;
use repl::*;

/// Which query language a statement is written in. PowQL is the native
/// language; SQL goes through the server-side/embedded SQL frontend
/// (`docs/SQL.md`), which lowers a supported subset onto the same planner.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dialect {
    Powql,
    Sql,
}

impl Dialect {
    fn prompt(self) -> &'static str {
        match self {
            Dialect::Powql => "powql> ",
            Dialect::Sql => "sql> ",
        }
    }
}

/// How results are rendered. `Table` is the human-readable default; `Json` and
/// `Csv` are the machine-readable modes that make the CLI scriptable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OutputMode {
    Table,
    Json,
    Csv,
}

/// The language + rendering pair chosen on the command line. Bundled so the
/// remote entry points stay under the argument-count lint.
#[derive(Clone, Copy)]
struct SessionOpts {
    dialect: Dialect,
    output: OutputMode,
}

fn parse_output_mode(name: &str) -> Option<OutputMode> {
    match name.trim().to_ascii_lowercase().as_str() {
        "table" => Some(OutputMode::Table),
        "json" => Some(OutputMode::Json),
        "csv" => Some(OutputMode::Csv),
        _ => None,
    }
}

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
        Action::Sweep { table } => {
            std::process::exit(run_sweep(&args.data_dir, table));
        }
        Action::Default => {}
    }

    let session = SessionOpts {
        dialect: args.dialect,
        output: args.output,
    };

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
                session,
                &args.tls,
            ));
            std::process::exit(code);
        }
        rt.block_on(run_remote(
            remote_addr.clone(),
            args.db.clone(),
            args.password.clone(),
            args.user.clone(),
            session,
            &args.tls,
        ));
    } else if let Some(query) = args.exec {
        std::process::exit(exec_embedded(&args.data_dir, &query, session));
    } else {
        run_embedded(&args.data_dir, session);
    }
}
