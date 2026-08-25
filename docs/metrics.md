# Metrics

`powdb-server` can expose a Prometheus endpoint. It is off by default; turn it
on with either form:

```bash
powdb-server --metrics-addr 127.0.0.1:9100
POWDB_METRICS_ADDR=127.0.0.1:9100 powdb-server
```

The endpoint is hand-rolled (no HTTP framework, zero extra dependencies) and
serves exactly three paths:

| Path | Answer |
|------|--------|
| `GET /metrics` | Prometheus text exposition (the reference below) |
| `GET /health` (or `/healthz`) | `ok powdb <version>`, a liveness probe that never touches the engine lock, so a long-running query cannot make the process look dead |
| anything else | `404` |

The endpoint is **unauthenticated**: bind it to localhost or a private
interface, never the public internet. It runs on its own listener and its own
task, so scraping cannot contend with query traffic.

Latency histograms (`*_duration_seconds`) share the bucket bounds
`0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0` seconds, plus `+Inf`.

## Metric reference

This table lists every metric family the endpoint exposes. CI holds it equal
to `Metrics::render()` (`crates/server/tests/metrics_doc_sync.rs`), the same
doc-is-data pattern as `docs/errors.md`.

| Family | Type | Labels | Meaning |
|--------|------|--------|---------|
| `powdb_build_info` | gauge | `version` | Always `1`; the server version rides the label. |
| `powdb_uptime_seconds` | gauge | | Seconds since the server started. |
| `powdb_connections_active` | gauge | | Currently open client connections. |
| `powdb_connections_accepted_total` | counter | | Total client connections accepted. |
| `powdb_tls_handshake_failures_total` | counter | | Total TLS handshakes that failed. |
| `powdb_queries_total` | counter | `result` (`ok`, `error`) | Total queries executed, by result. Timeouts and gate timeouts count as `error` here too, so the error rate stays truthful. |
| `powdb_queries_in_flight` | gauge | | Queries currently executing (saturation behind the engine lock). |
| `powdb_query_duration_seconds` | histogram | | Query execution time in seconds. |
| `powdb_query_timeouts_total` | counter | | Total queries whose execution exceeded the configured query timeout threshold. |
| `powdb_query_memory_limit_exceeded_total` | counter | | Total queries rejected by the per-query memory budget. |
| `powdb_tx_gate_timeouts_total` | counter | | Total frames that gave up waiting on the transaction gate, across every frontend that waits on it (explicit `begin`, bare autocommit statement, private sync frame). |
| `powdb_tx_reaped_total` | counter | | Total explicit transactions rolled back for exceeding `POWDB_TX_MAX_LIFETIME_MS`. |
| `powdb_auth_failures_total` | counter | | Total authentication failures. |
| `powdb_database_size_bytes` | gauge | | Size on disk of the data directory excluding the write-ahead log. |
| `powdb_wal_size_bytes` | gauge | | Size on disk of the write-ahead log. |
| `powdb_wal_fsync_total` | counter | | Total WAL fsyncs issued (group-commit leaders plus background flushes). |
| `powdb_wal_fsync_seconds_total` | counter | | Total seconds spent inside WAL fsync. Divide its rate by the rate of `powdb_wal_fsync_total` for mean fsync latency. |
| `powdb_wal_fsync_failures_total` | counter | | Total WAL fsyncs that returned an error (commits may not be durable; the WAL is poisoned and the server must be restarted). |
| `powdb_sync_operations_total` | counter | `operation` (`status`, `pull`, `ack`), `result` (`ok`, `error`) | Total private sync protocol operations, by operation and result. |
| `powdb_sync_operation_duration_seconds` | histogram | `operation` | Private sync protocol operation time in seconds. |
| `powdb_sync_pull_units_total` | counter | | Total retained units served by private sync pull responses. |
| `powdb_sync_pull_bytes_total` | counter | | Total retained-unit wire payload bytes served by private sync pull responses. |
| `powdb_sync_ack_advanced_total` | counter | | Total sync acknowledgements that advanced a replica cursor. |
| `powdb_sync_repair_actions_total` | counter | `operation`, `repair_action` (`none`, `pull`, `await_archive`, `rebootstrap`) | Total sync status repair actions returned, by operation. |

The two size gauges (`powdb_database_size_bytes`, `powdb_wal_size_bytes`)
appear only when the metrics registry knows the data directory, which the
server always wires up; embedders constructing `Metrics` directly get them by
calling `with_data_dir`.

## Alerting starting points

- `powdb_wal_fsync_failures_total > 0`: page. Durability is compromised and
  the WAL refuses further writes until restart.
- `rate(powdb_queries_total{result="error"}[5m])` sustained above your normal
  baseline: investigate.
- `powdb_queries_in_flight` pinned at a plateau while
  `rate(powdb_queries_total[1m])` drops toward zero: queries are stuck behind
  the engine lock.
- `histogram_quantile(0.99, rate(powdb_query_duration_seconds_bucket[5m]))`
  for latency SLOs.
