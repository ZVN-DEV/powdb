# PowDB on Railway

Railway builds straight from the repo `Dockerfile` and attaches a named
volume to back `POWDB_DATA`. The `railway.toml` in this directory is the
service config; everything else lives in the Railway dashboard.

## What you wire up in Railway

1. **Create a new service** in your project, point it at this repo, and
   commit the [`railway.toml`](./railway.toml) to the repo root (or this
   directory if you set `RAILWAY_TOML_PATH=examples/deploy/railway/railway.toml`).
2. **Create a volume** named `powdb-data` (Railway dashboard → Volumes →
   New volume). Pick the size you need — start at 1 GB; grow with
   `railway volume update`.
3. **Set env vars** on the service:
   | Var | Value | Notes |
   |---|---|---|
   | `POWDB_PORT` | `5433` | Match the TCP port Railway exposes. |
   | `POWDB_BIND` | `0.0.0.0` | Bind all interfaces so the platform proxy can reach it. |
   | `POWDB_DATA` | `/data` | Matches the `mountPath` in `railway.toml`. |
   | `POWDB_PASSWORD` | _(strong secret)_ | Mark as a secret in Railway so it's not shown in logs. |
   | `POWDB_REQUIRE_TLS` | `1` if you front with a TLS proxy or mount certs into the container; otherwise leave unset. |
   | `POWDB_QUERY_MEMORY_LIMIT` | `268435456` | 256 MiB. Tune up with bigger plans. |
   | `RUST_LOG` | `info` | |
4. **Expose the TCP port.** Railway's default networking is HTTP; for the
   raw PowDB wire protocol, add a TCP proxy in the service settings and
   target port `5433`.
5. **Deploy.** Railway builds the Dockerfile and starts the service. Logs
   appear in the dashboard.

## Connecting clients

```bash
powdb-cli --remote <your-railway-tcp-host>:<railway-tcp-port> \
          --password "$POWDB_PASSWORD"
```

The host/port come from the TCP proxy you configured in step 4.

## Trade-offs at scale

- **Volumes are per-service, single-attach.** Fine for PowDB (single
  writer), so this matches the engine's model.
- **Cold starts can lose state if a deploy migrates regions.** Volumes
  are pinned to a region; if Railway moves your service across regions,
  you'll need to snapshot and restore manually. Pin the region in the
  dashboard for production.
- **No managed backups.** Use `powdb-cli backup` (full / incremental,
  plus coarse PITR on restore) against the volume — but note backups are
  **offline**: stop the server first, so schedule them in a maintenance
  window or snapshot the volume via Railway's API instead. See
  [docs/backup-and-restore.md](../../../docs/backup-and-restore.md).
- **Bandwidth pricing.** Heavy benchmark traffic over the TCP proxy bills
  egress. For benchmark runs, use the Fly.io or AWS examples — Railway
  is best for developer-friendly persistent deploys, not for hammering.

## No CI smoke check

Unlike the AWS and Cloudflare examples, Railway has no offline schema
validator for `railway.toml` — the config is consumed by Railway's API at
deploy time. The README itself is the deliverable, and the
[`railway.toml`](./railway.toml) is annotated.
