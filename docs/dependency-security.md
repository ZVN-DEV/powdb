# Dependency security disposition — 2026-10-07

This record covers dependencies in v0.29.0, not the older v0.28.0 packages.
Do not read a passing audit as a claim that
every upstream informational advisory has disappeared.

## Compatible fixes

- The Node addon's development lockfile now uses `js-yaml` 4.3.2, addressing
  [GHSA-2883-xcg3-v3hh](https://github.com/nodeca/js-yaml/security/advisories/GHSA-2883-xcg3-v3hh).
- The workspace lockfile uses `rand` 0.9.3 and 0.10.1, the patched versions for
  the [custom-logger soundness issue](https://github.com/rust-random/rand/pull/1763).
  These are reached through property-test dependencies and the PostgreSQL
  comparison driver, respectively, not the shipped database's random generator.
- `chacha20` moves from a yanked version to the compatible, non-yanked 0.10.2.

These fixes preserve the Rust 1.93 minimum. No new dependency or advisory
suppression is introduced.

## Optional comparison-tool refresh

The new cross-engine collector upgrades the unpublished optional comparison
client to `mysql` 28.0.4, using `default-rust` and `rustls-tls`. Its resolved
statement-cache dependency is `lru` 0.18.5, replacing 0.12.5 and removing the paths
to [RUSTSEC-2026-0002](https://rustsec.org/advisories/RUSTSEC-2026-0002) and
[RUSTSEC-2026-0253](https://rustsec.org/advisories/RUSTSEC-2026-0253).

The refreshed graph no longer contains `rkyv`. The old optional-tool exception
for RUSTSEC-2026-0235 has been removed from `.cargo/audit.toml`; both advisory
suppression lists are empty. This is a dependency-path change, not a security
audit or a claim that no advisory can affect the new graph. Production engine
and published-package dependency selections are unchanged.

Verify the graph and the current advisory database:

```bash
cargo tree --workspace --all-features --locked -i lru
cargo tree --workspace --all-features --locked -i rkyv
cargo audit
```

The `rkyv` query is expected to report no matching package. The new client's
build and live MySQL fixture must both pass before treating this migration as
validated for server comparisons. Benchmark clients remain `publish = false`.
