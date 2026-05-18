//! PowDB query engine — lexer, parser, planner, and executor for PowQL.
//!
//! The query pipeline: PowQL text → Lexer (tokens) → Parser (AST) → Planner (plan tree) → Executor (results).
//! The planner is pure (no catalog access); plan lowering happens at execution time.

pub mod ast;
pub mod canonicalize;
pub mod executor;
pub mod lexer;
pub mod parser;
pub mod plan;
pub mod plan_cache;
pub mod planner;
pub mod result;
pub mod token;
