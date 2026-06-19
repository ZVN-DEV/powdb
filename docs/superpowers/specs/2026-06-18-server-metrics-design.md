# Design: powdb-server Prometheus metrics endpoint

_Date: 2026-06-18 · Status: approved (with DB-expert review folded in) · Author: Claude + Kirby_

## Goal

Give operators day-one observability for `powdb-server` in production (Docker/Fly/k8s): the
four golden signals (traffic, errors, latency, saturation) plus security and build identity —
without touching the binary wire protocol, without new dependencies, and without contending
with the `RwLock<Engine>` on scrape.

## Locked decisions

1. **Transport:** a separate HTTP listener serving Prometheus text exposition at `GET /metrics`
   on its own port. Never touches the PowDB binary wire protocol on `:5433`.
2. **Dependencies:** hand-rolled, **zero new crates**. Only existing `tokio` + `std::sync::atomic`.
   The HTTP responder parses the request line only.
3. **Posture:** opt-in, off by default. Enabled via `--metrics-addr <ip:port>` /
   `POWDB_METRICS_ADDR`. **Unauthenticated** (standard Prometheus model; isolate at the network
   layer). Bind failure when explicitly requested is **fatal at startup** (consistent with the
   main TCP/TLS/engine bind failures in `main.rs`).
4. **Scrape is lock-free:** `render()` reads only atomics. It never locks the engine.

## Metric set (core operational)

All names carry the `powdb_` namespace prefix. Counters end in `_total`. Both label values are
always emitted (no appearing/disappearing series).

| Metric | Type | Notes |
|---|---|---|
| `powdb_build_info{version="x.y.z"}` | gauge (=1) | version from `env!("CARGO_PKG_VERSION")`, matches `ConnectOk` |
| `powdb_uptime_seconds` | gauge | `start: Instant` elapsed, computed in `render()` |
| `powdb_connections_active` | gauge | RAII guard; holds while a connection holds a permit |
| `powdb_connections_accepted_total` | counter | incremented per accepted TCP connection |
| `powdb_tls_handshake_failures_total` | counter | incremented in `main.rs` at the TLS-accept error arm |
| `powdb_queries_total{result="ok"\|"error"}` | counter | classified inside `run_blocking_query` |
| `powdb_queries_in_flight` | gauge | RAII; inc before `spawn_blocking`, dec after — saturation signal behind the engine lock |
| `powdb_query_timeouts_total` | counter | the `Err(_)` timeout arm of `run_blocking_query` (also counts as `result=error`) |
| `powdb_query_memory_limit_exceeded_total` | counter | when the typed error is `QueryError::MemoryLimitExceeded` |
| `powdb_auth_failures_total` | counter | `AuthOutcome::Rejected` only — NOT rate-limited rejections, NOT "expected CONNECT" |
| `powdb_query_duration_seconds` | histogram | fixed buckets; `_sum` in seconds (float), `_count`, cumulative `_bucket{le=...}` |

Histogram buckets (seconds): `0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1, 5, +Inf`.

**Deliberately excluded:** `connections_rejected` (the accept loop awaits the semaphore permit
*before* accepting, so backpressure is via the OS backlog — there is no active-reject path, the
counter would always be 0). Storage internals (WAL size, page cache, B-tree depth) — out of
scope; would require reaching through the `RwLock<Engine>` on every scrape.

## Architecture

```
                ┌───────────────┐   inc/record (lock-free atomics, Relaxed)
   handlers ───▶│ Arc<Metrics>  │◀──────────────────────────────┐
                └──────┬────────┘                                │
                       │ render() -> Prometheus text             │
   Prometheus ──GET /metrics──▶ serve_metrics task (own port) ───┘
```

New file `crates/server/src/metrics.rs`. `Metrics` is `Arc`-shared into `ConnOpts` (always
present — see SA-8) and into the `main.rs` accept loop.

## Components

### `metrics.rs`
- `struct Metrics` — atomics for every counter/gauge above; the histogram is `[AtomicU64; N]`
  buckets + `sum_nanos: AtomicU64` + `count: AtomicU64`; plus `start: Instant`,
  `version: &'static str`.
- Methods (all `&self`, lock-free):
  - `record_query(elapsed: Duration, outcome: QueryOutcome)` where
    `enum QueryOutcome { Ok, Error, Timeout, MemoryLimit }`. Updates `queries_total`,
    the histogram, and the specific counters. **Write order (load-bearing, MF-4):** bucket →
    `sum_nanos` → `count` **last**.
  - `inc_connection_accepted()`, RAII `ActiveGuard` (active gauge), RAII `InFlightGuard`,
    `inc_auth_failure()`, `inc_tls_failure()`.
  - `render(&self) -> String` — valid Prometheus text. **Read order (MF-4):** read `count`
    **first**, then buckets/sum, so the only possible skew is count slightly behind buckets
    (benign; keeps `+Inf >= _count`). Emits `# HELP`/`# TYPE` once per family, cumulative
    buckets ascending by `le`, `le="+Inf"`, `_sum` as seconds float, escapes the `build_info`
    version label (`\\`, `\"`, `\n`).
- `enum QueryOutcome` + RAII guards (`ActiveGuard`, `InFlightGuard`) with `Drop` impls.

### `serve_metrics(addr, metrics: Arc<Metrics>, mut shutdown_rx: watch::Receiver<bool>)`
- `TcpListener::bind(addr)` — caller fails fast on error.
- Accept loop in `tokio::select!` with `shutdown_rx.changed()` (drains on SIGINT/SIGTERM).
- Each connection is `tokio::spawn`-ed so one slow client can't wedge the accept loop (MF-3).
- Per-connection handler (MF-3 hardening, mandatory):
  - **read cap** ~8 KB (mirror `MAX_CONNECT_PAYLOAD_SIZE`); bail → `400`/`431` past it.
  - **read timeout** ~5s via `tokio::time::timeout` (snappy; NOT the 300s idle timeout).
  - read until first `\r\n` (request line) or cap; ignore headers/body.
  - parse robustly: missing CRLF, bare `\n`, no spaces, non-ASCII, empty → `400`, never panic.
  - `GET /metrics` → `200` with `Content-Type: text/plain; version=0.0.4; charset=utf-8` +
    rendered body; any other path/method → `404`. `Connection: close`, then close (no keep-alive).

### Wiring — `main.rs`
- `Args` gets `metrics_addr: Option<String>`; `parse_args` reads `POWDB_METRICS_ADDR` then
  `--metrics-addr` (exit(2) on missing value), same pattern as the other flags. Add to `--help`
  and the `ENVIRONMENT` block; log it in the startup `info!`.
- Construct `Arc<Metrics>` always. If `metrics_addr` is set: `TcpListener::bind` **early**
  (before the "listening" log / accept loop); on error `error!` + `exit(1)`; else
  `tokio::spawn(serve_metrics(...))` with a `shutdown_rx.clone()` taken before the accept loop.
- In the spawned per-connection task: construct `ActiveGuard::new(metrics.clone())` right after
  the permit is acquired, **before** the TLS branch, so it brackets the same scope as `permit`
  and drops on every early return/panic (MF-2). `inc_connection_accepted()` at the same spot.
- TLS-accept `Err` arm: `metrics.inc_tls_failure()` (SA-2 — this lives in `main.rs`, not the
  handler).

### Wiring — `handler.rs`
- `ConnOpts` gets `metrics: Arc<Metrics>` (always present; tests pass `Metrics::new()` — SA-8).
- `run_blocking_query` takes `&Arc<Metrics>`. Time around `spawn_blocking`+`timeout`; classify
  from the four typed arms (MF-1):
  - `Ok(Ok(Ok(_)))` → `QueryOutcome::Ok`
  - `Ok(Ok(Err(QueryError::MemoryLimitExceeded{..})))` → `QueryOutcome::MemoryLimit`
  - `Ok(Ok(Err(_)))` / `Ok(Err(_))` → `QueryOutcome::Error`
  - `Err(_)` (timeout) → `QueryOutcome::Timeout` (records the timeout duration)
  - Wrap the body in an `InFlightGuard` so `queries_in_flight` is correct across the timeout/abort.
- Auth: `metrics.inc_auth_failure()` at the `AuthOutcome::Rejected` arm only (SA-1) — do not
  count the rate-limited rejection or the "expected CONNECT" path.

## Error handling
- Metric updates are pure atomics: never block, fail, or meaningfully slow the query path
  (`Instant::now() × 2` ≈ tens of ns vs a lock + `spawn_blocking` already costing microseconds).
- `serve_metrics`: malformed/oversized/slow requests → `4xx` or dropped on timeout; per-conn
  errors logged at `debug`; one bad request never kills the listener.
- Metrics hold **no** engine reference, so the shutdown `drop(engine)` is unaffected; the metrics
  task winds down via the watch channel concurrently with connection drain.

## Testing
- **Unit (`metrics.rs`):** `render()` format — `# HELP`/`# TYPE` once per family, cumulative
  buckets ascending, `le="+Inf"` == `_count`, `_sum` present as seconds; `record_query` lands in
  the right bucket; counter/label correctness; `build_info` label escaping.
- **Concurrency race (NTH-1):** N tasks hammer `record_query`/`inc_*` while another loops
  `render()`; assert no panic, final `queries_total == N`, histogram `_count == N`, `+Inf == _count`.
  This guards the MF-4 ordering.
- **`serve_metrics` robustness:** valid `GET /metrics` → 200 + body has metric names; non-GET →
  404; unknown path → 404; garbage/empty/no-CRLF request line → 400 (no panic); oversized line →
  bail; slowloris (open, send nothing) → dropped within the read timeout.
- **Gauge-leak (MF-2):** drive the accept-loop wiring (or an equivalent), open a connection, drop
  it mid-flight/abort the task, scrape, assert `connections_active` returns to 0.
- **Format validity:** if `promtool` is available in CI use it; otherwise a structural assertion
  (HELP/TYPE pairing, ascending `le`, `+Inf == count`, counters monotonic).

## Out of scope (follow-ups)
Auth token on `/metrics`; storage internals; HTTP keep-alive/anything beyond `GET /metrics`;
`process_start_time_seconds` (optional later).
