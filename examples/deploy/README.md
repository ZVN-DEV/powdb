# Deployment examples

Reference configurations for self-hosting `powdb-server`. These are templates,
not live deployments — replace placeholder names and secrets before running.

## Fly.io

[`fly.toml`](./fly.toml) is a minimal stateful TCP deployment: one machine,
one persistent volume, no HTTP layer, no TLS termination. PowDB clients speak
the native binary wire protocol directly.

```bash
# From repo root, with examples/deploy/fly.toml as your starting point:
cp examples/deploy/fly.toml ./fly.toml
# edit `app = "your-powdb-app"` in fly.toml
fly launch --copy-config --no-deploy
fly volumes create powdb_data --region iad --size 1
fly secrets set POWDB_PASSWORD=$(openssl rand -hex 32)
fly deploy
```

Notes:

- The Fly machine uses the repo `Dockerfile` (built via `[build] dockerfile`).
- TLS is **not** terminated by Fly for raw TCP services — either enable
  PowDB's own TLS (`POWDB_TLS_CERT` / `POWDB_TLS_KEY` secrets) or run behind
  a TLS-terminating proxy you control.
- `min_machines_running = 1` keeps the database always-on; `auto_stop_machines`
  is `false` so Fly never suspends a stateful service.

## Docker / Compose

See [`docker-compose.yml`](https://github.com/zvndev/powdb/blob/main/docker-compose.yml)
in the repo root for a local-only quick-start.

## Other platforms

PowDB ships a multi-arch container image at `ghcr.io/zvndev/powdb`. Any
platform that can run a long-lived TCP container with a persistent volume
(Railway, Hetzner, EC2, k8s with a PVC) will work — the Fly config is just a
worked example.
