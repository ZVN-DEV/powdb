# Security Policy

## Supported Versions

PowDB ships security fixes only for the latest minor series. Upgrade to the
latest release to stay supported.

| Version         | Supported          |
| --------------- | ------------------ |
| 0.6.x           | :white_check_mark: |
| 0.5.x           | :x: (superseded)   |
| 0.4.4 – 0.4.9   | :x: (superseded)   |
| 0.4.1 – 0.4.3   | :x: (yanked)       |
| ≤ 0.4.0         | :x:                |

> **v0.4.1, v0.4.2, and v0.4.3 are yanked** for data-loss bugs in crash
> recovery and were replaced by **v0.4.4**, which added a permanent durability
> regression suite. If you are on any of those three versions — or any release
> older than the current minor series — upgrade to the latest 0.6.x release.
> See `CHANGELOG.md` for details.

## Reporting a Vulnerability

If you discover a security vulnerability in PowDB, please report it responsibly.

**Do not open a public issue.** Instead, email:

**78920650+zvndev@users.noreply.github.com**

Include:
- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

You should receive an acknowledgment within 48 hours. We aim to provide a fix or mitigation within 7 days for critical issues.

## Scope

PowDB is a storage engine and query executor. Security-relevant areas include:

- **Wire protocol** (`crates/server/`) — binary framing, authentication, connection limits
- **Query parser** (`crates/query/src/parser.rs`) — input validation, nesting depth limits
- **Storage engine** (`crates/storage/`) — WAL integrity, mmap safety, file I/O bounds
- **Network binding** — server binds to `127.0.0.1` by default (not `0.0.0.0`)

## Transport Security (TLS)

PowDB supports native TLS for encrypted client-server connections. To enable TLS, set the following environment variables when starting the server:

- `POWDB_TLS_CERT` — path to the PEM-encoded TLS certificate
- `POWDB_TLS_KEY` — path to the PEM-encoded TLS private key

When both are set, the server requires TLS for all connections. When unset, the server accepts plaintext TCP connections. For production deployments, always enable TLS or use a reverse proxy / SSH tunnel.

## Authentication

PowDB supports two authentication modes:

1. **Shared password** — set the `POWDB_PASSWORD` environment variable. All clients authenticate with the same shared secret. Applies only when no named users are defined.
2. **Named users with roles** (since 0.4.5) — users with `admin`, `readwrite`, or `readonly` roles, managed via `powdb-cli useradd` / `passwd` / `userdel`. Passwords are stored as argon2id hashes only (`auth.json` in the data directory, `0600` on Unix). When `POWDB_ADMIN_USER` and `POWDB_ADMIN_PASSWORD` are both set, the server bootstraps an initial admin on startup without the CLI. Once any user is defined, the shared password is no longer used.

In both modes:

- **Rate limiting**: authentication attempts are rate-limited to prevent brute-force attacks.
- **Pre-auth payload limits**: the server enforces frame size limits on unauthenticated connections to prevent resource exhaustion.
- **Connection limits**: the server enforces a maximum number of concurrent connections.

> **Note on the `readonly` role:** in releases up to and including 0.4.5, role storage is in place but read-only restrictions are **not enforced** at the query layer — do not rely on the `readonly` role as a security boundary against writes on those versions. Read-only restrictions are **enforced as of 0.4.6** at the server dispatch layer: write statements from `readonly` users are rejected with `permission denied`, and unknown roles fail closed.

## Known Limitations

- Roles are coarse (`admin` / `readwrite` / `readonly`). There are no per-table ACLs, row-level security, or multi-tenant isolation; `readonly` enforcement is absent in ≤0.4.5 and enforced from 0.4.6 (see note above).
- The query parser has a nesting depth limit. Runaway queries are bounded by `POWDB_QUERY_TIMEOUT` (default 30s) and the per-query memory budget (`POWDB_QUERY_MEMORY_LIMIT`).
