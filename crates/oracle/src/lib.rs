//! A differential SQLite oracle for PowDB.
//!
//! `crates/compare` already links rusqlite and times SQLite next to PowDB, but
//! it never checks that the two agree. This crate is the check.
//!
//! # What it does
//!
//! For each seeded fixture it loads the *same* rows into PowDB and into
//! SQLite, then runs each generated case three ways: native PowQL, PowDB's SQL
//! frontend, and SQLite. It compares the **full result sets** (column names,
//! every value, every value's type, and row order whenever the query fixes
//! one). It does not compare row counts: two engines can return the same
//! number of wrong rows, which is exactly how a datetime bug once survived a
//! green suite.
//!
//! # Why the comparison can be trusted
//!
//! - Fixture data reaches SQLite as **bound parameters**, so SQLite holds the
//!   generator's exact intended value and a broken PowQL literal cannot be
//!   cancelled out by an identically broken SQLite literal.
//! - The [`normalize`] layer only reinterprets SQLite's storage class back
//!   into the PowDB type the *schema* declares, and only for the four PowDB
//!   types SQLite has no carrier for. Every rule is justified in place and
//!   fails loudly rather than coercing. No values are rewritten.
//! - Accepted differences live in `known_divergences.toml`, where every entry
//!   carries a reason and an owning `file:line`, must match at least one
//!   divergence in the run, and must name a shape a generator really produces.
//!   See [`divergence`].
//!
//! # Running it
//!
//! `cargo test -p powdb-oracle` runs the fixed-seed, fixed-budget CI gate.
//! `cargo run -p powdb-oracle --bin powdb-oracle -- --budget 5000` widens the
//! search locally.

pub mod compare;
pub mod divergence;
pub mod engines;
pub mod fixture;
pub mod model;
pub mod mutations;
pub mod normalize;
pub mod rng;
pub mod run;
pub mod shapes;
