# Trusted Publishing — the ZVN token-less release standard

**Goal: never create, paste, store, rotate, or forget a registry token again.**

Every ZVN package — npm and crates.io — publishes from CI using **Trusted
Publishing (OIDC)**. Instead of a long-lived token stored as a CI secret, the
GitHub Actions workflow proves its own identity to the registry with a
short-lived OIDC token minted per run. You configure the trust relationship
*once* on the registry's website; after that, releases are token-less.

This is the standard. Copy the template below into any repo, configure the
trusted publisher on the registry, and delete the old tokens.

## Why this over a stored token

| | Personal/automation token in CI | **Trusted Publishing (OIDC)** |
|---|---|---|
| Secret stored anywhere | yes (rotate, leak risk) | **none** |
| Per-repo setup | new token + secret each time | **~30s of clicks, once** |
| Leak blast radius | whatever the token scopes | **none — no credential exists** |
| Provenance attestation | no | **yes, automatic (public repos)** |

Provenance is the green "Published via GitHub Actions" badge on the package
page: a cryptographic link from the published artifact back to the exact commit
and workflow run that built it. Free supply-chain transparency.

## One-time setup (per package)

### npm

1. npmjs.com → your package → **Settings → Trusted Publishing → Add publisher**.
2. Select **GitHub Actions** and fill in:
   - Organization/user: e.g. `ZVN-DEV`
   - Repository: e.g. `powdb`
   - Workflow filename: e.g. `release.yml` (must match exactly)
   - Environment (optional): e.g. `npm-publish` — leave blank to match any.
3. Save. **No token is generated.** That's the point.

> Brand-new package name (doesn't exist on npm yet)? Do **one** bootstrap
> `npm publish` while logged in locally to create it, then configure trusted
> publishing for every release after. Existing packages: just configure.

> **PowDB npm packages (3 configs):**
> - `@zvndev/powdb-client` — workflow `release.yml`, env `npm-publish`.
> - `@zvndev/powdb-sync`: workflow `release.yml`, env `npm-publish`. The
>   experimental sync orchestration package. Also a brand-new name, so it needs
>   the one bootstrap `npm publish` above before the trusted publisher can be
>   configured. Until then the `npm-publish-sync` job in `release.yml` warns and
>   skips instead of failing the release.
> - `@zvndev/powdb-embedded` — workflow `publish-node-addon.yml`, env
>   `npm-publish`. This is the embedded Node addon; it ships **one** package
>   with prebuilt binaries for all platforms bundled in (no per-platform
>   sub-packages), so only this single name needs a trusted publisher. It's a
>   brand-new name, so it needs the one bootstrap `npm publish` above first.

### crates.io

Same idea, but configured **per crate** (a workspace with 7 published crates —
`powdb-storage`, `powdb-auth`, `powdb-query`, `powdb-backup`, `powdb-server`,
`powdb` (embedded facade), `powdb-cli` — needs 7 configs):

1. crates.io → each crate → **Settings → Trusted Publishing → Add**.
2. Owner `ZVN-DEV`, repo `powdb`, workflow `publish.yml`, optional environment.
3. Repeat for every publishable crate.

## The reusable workflow template (npm)

Drop this in as `.github/workflows/release-npm.yml`. Token-less,
least-privilege, provenance-signed, with a tag↔version guard so a mistagged
release can't publish.

```yaml
name: release-npm
on:
  push:
    tags: ["v*"]
permissions: {}                       # default deny; grant per-job
jobs:
  publish-npm:
    runs-on: ubuntu-latest
    environment: npm-publish          # optional; add required reviewers here for a manual gate
    permissions:
      contents: read
      id-token: write                 # the ONLY grant OIDC needs — no NPM secret
    steps:
      - uses: actions/checkout@<pin-to-sha>
      - uses: actions/setup-node@<pin-to-sha>
        with:
          node-version: "22"
          registry-url: "https://registry.npmjs.org"
      - run: npm install -g npm@latest # trusted publishing requires npm >= 11.5.1
      - name: Guard tag == package.json version
        run: |
          tag="${GITHUB_REF#refs/tags/v}"
          pkg="$(node -p "require('./package.json').version")"
          [ "$tag" = "$pkg" ] || { echo "::error::tag v$tag != pkg $pkg"; exit 1; }
      - run: npm ci && npm run build   # or pnpm install --frozen-lockfile && pnpm build
      - run: npm publish --provenance --access public   # no NODE_AUTH_TOKEN
```

For crates.io, the equivalent auth step is:

```yaml
    permissions:
      contents: read
      id-token: write
    steps:
      # ...checkout + toolchain...
      - uses: rust-lang/crates-io-auth-action@<pin-to-sha> # v1
        id: auth
      - run: cargo publish
        env:
          CARGO_REGISTRY_TOKEN: ${{ steps.auth.outputs.token }}
```

> Always pin third-party actions to a full commit SHA (not a tag) and leave the
> `# vX.Y.Z` comment for readability — a moving tag is a supply-chain hole.
> Look up the SHA with `gh api repos/<owner>/<repo>/commits/<tag> --jq .sha`.

## Security checklist

- [ ] `permissions:` defaults to `{}`; the publish job grants only `id-token: write` + `contents: read`.
- [ ] Trigger is a pushed `v*` tag (an immutable ref), not `pull_request` (forks can't get your OIDC identity anyway, but don't invite it).
- [ ] Third-party actions pinned to SHAs.
- [ ] A tag↔manifest version guard fails the run on mismatch.
- [ ] Optional: a GitHub **Environment** with required reviewers and/or a tag
      restriction, so publishing needs a human click and can only run from
      release tags. Reference the same environment name in the registry's
      trusted-publisher config to bind the trust tighter.
- [ ] `--provenance` on public repos for attestation.
- [ ] After migrating: **revoke the old tokens** to shrink the attack surface.

## Applying to your other repos

1. Copy the template, adjust install/build commands and `working-directory`.
2. Configure the trusted publisher on the registry (npm: per package;
   crates.io: per crate).
3. Tag a release. It publishes token-less.
4. Delete the repo's old token/secret.

## Troubleshooting

- **`npm error code E422` / "missing OIDC" on publish** — npm too old. Ensure
  `npm install -g npm@latest` runs before `npm publish` (need ≥ 11.5.1).
- **`401`/`403` from the registry** — the trusted publisher isn't configured, or
  org/repo/workflow/environment don't match exactly what's on the registry.
- **Provenance fails** — repo must be public and `package.json` needs a
  `repository` URL pointing at this repo.
- **crates.io auth fails on dry run** — expected; dry runs skip auth. Configure
  trusted publishing before the first real (`dry_run=false`) publish.
