# Dependency security disposition — 2026-10-07

This record applies to the source candidate for the next release, not to the
already published v0.28.0 packages. Do not read a passing audit as a claim that
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

## Remaining optional comparison-tool dependency

`lru` 0.12.5 remains reachable only through:

```text
powdb-compare (publish = false, optional mysql feature, disabled by default)
  -> mysql 25.0.1 -> lru 0.12.5
```

It is absent from the default dependency graph and all eight published Rust
crates, the server/CLI release binaries, and the embedded Node addon. This is a
release-scope disposition, not a fix or a general safety guarantee for `lru`.
The GitHub alerts and audit warnings remain visible; neither is dismissed or
suppressed.

- [RUSTSEC-2026-0002](https://rustsec.org/advisories/RUSTSEC-2026-0002): the
  mutable iterator violates Stacked Borrows. The MySQL statement cache does not
  use `iter_mut`.
- [RUSTSEC-2026-0253](https://rustsec.org/advisories/RUSTSEC-2026-0253): `pop`
  is not panic-safe when dropping a key panics. The MySQL statement cache's
  key is `u32`, which has no panicking destructor. This does not excuse using
  the affected library in other contexts.

The follow-up is migration of the optional comparison tool to `mysql >= 28.0.3`
with `lru >= 0.18.2`, including its changed TLS feature selection and a real
MySQL comparison smoke. Upgrading only to `lru` 0.16.3 does not fix the newer
panic-safety advisory. No such migration is claimed in this candidate.

Recheck the scope on each dependency refresh:

```bash
cargo tree --workspace --all-features --locked -i lru
cargo tree --workspace --locked
cargo audit
npm audit --prefix bindings/node
```

Other pre-existing unmaintained-dependency notices remain visible in audit
output. The optional MySQL tool's existing `rkyv` exception is unchanged.
