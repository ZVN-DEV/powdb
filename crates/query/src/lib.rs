//! PowDB query engine — lexer, parser, planner, and executor for PowQL.
//!
//! The query pipeline: PowQL text → Lexer (tokens) → Parser (AST) → Planner (plan tree) → Executor (results).
//! The planner is pure (no catalog access); plan lowering happens at execution time.
//!
//! # Public API boundary
//!
//! `canonicalize`, `plan`, `plan_cache`, and `token` are marked
//! `#[doc(hidden)]`. They stay `pub` and code that imports them still compiles:
//! the attribute hides a module from the generated documentation without
//! restricting access to it. They are `pub` so that `powdb-server` can
//! canonicalize a query for redaction, `powdb-cli` can inspect a token stream,
//! and this crate's own integration tests can assert on plan shapes. None of
//! that is an interface: plan shapes track the executor and tokens track the
//! grammar, so both change without notice whenever those change.
//!
//! The supported entry points are the `powdb` facade crate and
//! [`executor::Engine`]. See `docs/STABILITY.md` for what a version bump
//! promises.

pub mod ast;
pub mod cancel;
#[doc(hidden)]
pub mod canonicalize;
pub mod executor;
pub mod lexer;
pub mod parser;
#[doc(hidden)]
pub mod plan;
#[doc(hidden)]
pub mod plan_cache;
pub mod planner;
pub mod result;
pub mod sql;
#[doc(hidden)]
pub mod token;
