//! PowDB TCP server — handles client connections, authentication, and query dispatch.
//!
//! Supports TLS via `tokio-rustls`, password authentication with rate limiting,
//! and concurrent query execution via `spawn_blocking`.

pub mod handler;
pub mod metrics;
pub mod protocol;
pub mod redact;
