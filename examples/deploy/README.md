# Deployment examples

Reference configurations for self-hosting `powdb-server`. These are templates,
not live deployments — replace placeholder names and secrets before running.

## Why auto-restart is required (read this first)

**PowDB is crash-only by design, so a process supervisor with auto-restart is
MANDATORY in production.** The server is built with `panic = "abort"`: on an
unrecoverable error it exits immediately rather than trying to limp along in a
half-broken state. On the next start, WAL replay rolls the data dir forward to
the last consistent state, recovering committed writes. This is fast and safe —
but it only works if *something* restarts the process.

Every example here ships with auto-restart already wired in:

| Example            | Auto-restart mechanism                                              |
| ------------------ | ------------------------------------------------------------------ |
| Fly.io             | `auto_start_machines = true`, `min_machines_running = 1` (fly.toml) |
| Railway            | `restartPolicyType = "ON_FAILURE"` (railway.toml)                  |
| Cloudflare Tunnel  | `restart: unless-stopped` on both services (docker-compose.yml)    |
| AWS ECS Fargate    | the service reconciles tasks toward `desired_count = 1` (main.tf)  |

If you adapt these to another platform, keep an equivalent supervisor
(systemd `Restart=always`, Kubernetes Deployment/`restartPolicy: Always`,
`docker run --restart unless-stopped`, etc.). Running `powdb-server` as a bare,
unsupervised process means a single crash leaves the database **down** until a
human restarts it — even though the data on disk is fully recoverable.

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

## Docker

Quick-start with the published image (note: the repo-root `docker-compose.yml`
is the benchmark harness — it does not define a PowDB service):

```bash
docker run -d --name powdb \
  -p 5433:5433 \
  -v powdb_data:/data \
  -e POWDB_DATA=/data \
  -e POWDB_BIND=0.0.0.0 \
  -e POWDB_PASSWORD=change-me \
  ghcr.io/zvn-dev/powdb:v0.22.0
```

## AWS ECS Fargate + EFS

[`aws-ecs/`](./aws-ecs/) is a Terraform module that provisions an ECS
cluster, a single Fargate task running `ghcr.io/zvn-dev/powdb:v0.22.0`, and
an EFS file system backing `POWDB_DATA`. Read
[`aws-ecs/README.md`](./aws-ecs/README.md) for trade-offs (single-writer,
EFS fsync latency) before applying.

## Cloudflare Tunnel

[`cloudflare-tunnel/`](./cloudflare-tunnel/) runs `powdb-server` +
`cloudflared` together. Zero host ingress ports — the wire protocol is
only reachable through the tunnel. Good fit for laptop / homelab /
single-VPS deploys that need a stable hostname without a public IP.

## Railway

[`railway/`](./railway/) wires PowDB to Railway's persistent volumes via
the repo Dockerfile. Best for developer-friendly hosted deploys; see the
[`railway/README.md`](./railway/README.md) for the gotchas at scale.

## Other platforms

PowDB ships a container image at `ghcr.io/zvn-dev/powdb`. Any
platform that can run a long-lived TCP container with a persistent volume
(Hetzner, EC2 direct, k8s with a PVC) will work — the above are just
worked examples.
