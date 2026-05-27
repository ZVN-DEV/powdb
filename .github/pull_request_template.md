<!--
Thanks for contributing to PowDB! A few quick checks before you submit:

  - `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings`
    both clean
  - `cargo test --workspace` passes locally
  - If you changed anything on a hot path, `cargo bench -p powdb-bench` and
    `cargo run -p powdb-bench --bin compare` both green
-->

## Summary

<!-- One or two sentences on what this changes and why. -->

## Type of change

- [ ] Bug fix
- [ ] New feature
- [ ] Documentation
- [ ] Refactor / cleanup
- [ ] Performance improvement
- [ ] CI / tooling

## Behavior change

<!-- User-visible behavior, wire-protocol changes, on-disk format changes, or
PowQL syntax changes. Write "none" if internal-only. -->

## Test plan

- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo fmt --all -- --check` clean
- [ ] (if perf-sensitive) `cargo bench -p powdb-bench` shows no regression beyond gate thresholds
- [ ] Docs updated (if applicable)
- [ ] CHANGELOG.md updated (if user-facing)

## Related issues

<!-- Link to any related issue: "Fixes #123" / "Refs #45". -->
