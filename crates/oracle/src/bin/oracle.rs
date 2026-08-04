//! Manual runner for the differential oracle.
//!
//! `cargo test -p powdb-oracle` is the CI gate and runs a fixed budget. This
//! binary exists to widen the search locally without changing what CI does.

use powdb_oracle::run::{run, Config};

fn main() {
    let mut config = Config::default();
    let mut verbose = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut take = |name: &str| -> u64 {
            args.next()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("--{name} needs a number"))
        };
        match arg.as_str() {
            "--seed" => config.seed = take("seed"),
            "--per-shape" => config.per_shape = take("per-shape") as usize,
            "--budget" => config.budget = take("budget") as usize,
            "--all" => verbose = true,
            other => {
                eprintln!("unknown argument `{other}`");
                eprintln!("usage: powdb-oracle [--seed N] [--per-shape N] [--budget N] [--all]");
                std::process::exit(2);
            }
        }
    }

    match run(&config) {
        Ok(report) => {
            print!(
                "{}",
                if verbose {
                    report.full()
                } else {
                    report.summary()
                }
            );
            if !report.is_clean() {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("oracle failed to run: {e}");
            std::process::exit(2);
        }
    }
}
