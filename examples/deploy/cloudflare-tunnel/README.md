# PowDB behind a Cloudflare Tunnel

Self-host `powdb-server` anywhere (laptop, homelab, VPS) and expose it
through a Cloudflare Tunnel — no public ports, no inbound firewall rules,
no static IP needed. Clients connect through Cloudflare's edge.

## What this example does

- Runs `ghcr.io/zvn-dev/powdb:v0.19.1` on an internal docker network.
- Runs `cloudflare/cloudflared` as a sidecar that establishes an
  outbound-only tunnel to Cloudflare.
- Routes the hostname you own (e.g. `powdb.example.com`) over TCP into
  `powdb-server:5433` on the internal network.
- **Publishes zero host ports.** The PowDB wire protocol is only reachable
  via the tunnel — there is no LAN exposure.

## One-time setup

1. Install [`cloudflared`](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/) on the machine that will run docker compose.
2. Authenticate and create a tunnel:
   ```bash
   cloudflared tunnel login          # opens browser, picks a Cloudflare zone
   cloudflared tunnel create powdb   # prints a UUID + writes credentials JSON
   ```
3. Copy the credentials file into this directory:
   ```bash
   cp ~/.cloudflared/<TUNNEL_UUID>.json ./tunnel-creds.json
   ```
4. Edit [`config.yml`](./config.yml): replace `<TUNNEL_UUID>` with the
   value from step 2, and `powdb.example.com` with the DNS name you want.
5. Tell Cloudflare to route that hostname to this tunnel:
   ```bash
   cloudflared tunnel route dns powdb powdb.example.com
   ```
6. Set the password the server will require:
   ```bash
   cp .env.example .env
   # edit .env, replace the placeholder with a strong secret
   ```

## Run

```bash
docker compose up -d
docker compose logs -f cloudflared   # confirms the tunnel registered
```

The tunnel comes up in a few seconds. `cloudflared` reconnects
automatically on network blips.

## Connect from a client

Cloudflare Tunnels for TCP require a client-side proxy. On the consuming
machine:

```bash
cloudflared access tcp \
  --hostname powdb.example.com \
  --url localhost:5433

# In another terminal:
powdb-cli --remote 127.0.0.1:5433 --password "$POWDB_PASSWORD"
```

## Lock it down with Cloudflare Access (recommended)

By default, anyone with `cloudflared access` and the hostname can attempt
to connect (and gets stopped by `POWDB_PASSWORD`). For real production
access control, attach a Cloudflare Access policy:

1. Cloudflare Zero Trust dashboard → Access → Applications → Add an
   application → **Self-hosted**, hostname `powdb.example.com`.
2. Add a policy: allow only specific emails, groups, or service tokens.
3. Issue service tokens to programmatic clients; humans get the SSO flow
   when running `cloudflared access`.

## Trade-offs

- **Higher tail latency** than a same-VPC connection — every packet
  traverses Cloudflare's edge. Fine for an admin shell, slow for hot-path
  query traffic.
- **TLS terminates at Cloudflare.** The link from Cloudflare → your
  origin runs over the tunnel (encrypted) but PowDB itself sees plaintext
  on `5433`. Enable PowDB's own TLS if you don't want to trust the tunnel
  edge.
- **Single writer** — same constraint as every other PowDB deploy.

## Validation (no Cloudflare account needed)

```bash
docker compose -f docker-compose.yml config -q
```

This parses the compose file and exits 0 if the schema is valid. CI runs
exactly this check.
