<!-- markdownlint-disable MD013 -->

# Branch Cleanup Audit — explicit-transactions + stale remote branches

Audit timestamp: 2026-06-16 00:59-01:05 UTC  
Repository: `ZVN-DEV/powdb`  
Default branch: `origin/main` (`c3a73bc`, merge of PR #91)
Current worker worktree: detached at `52f087f`; local `main` is 3 commits ahead of `origin/main`, so this audit uses `origin/main` as the remote-delete baseline.

## Method

Fresh refs were fetched with:

```bash
git fetch --all --prune --tags
git remote show origin
git branch -r --merged origin/main
git branch -r --no-merged origin/main
git worktree list --porcelain
gh pr list --state all --limit 100 --json number,title,state,headRefName,baseRefName,mergedAt,closedAt,updatedAt,url
```

For each candidate branch, the audit checked:

```bash
git merge-base --is-ancestor origin/<branch> origin/main
git rev-list --left-right --count origin/main...origin/<branch>
gh pr list --state all --head <branch> --json number,state,mergedAt,closedAt,title,url
gh pr view <number> --json number,state,headRefName,headRefOid,baseRefName,mergeCommit,mergedAt,closedAt,url,title
```

A remote branch is considered safe to delete when it is not checked out in any listed worktree and either:

1. `git merge-base --is-ancestor origin/<branch> origin/main` returns success, or
2. GitHub reports the branch's PR as `MERGED` and the current remote branch tip equals the PR `headRefOid`, which rules out post-merge branch commits after the PR merge.

No remote deletion was performed by this audit.

## Exact safe-delete candidates

Run this exact command to delete only the audited stale remote branches:

```bash
git push origin --delete \
  explicit-transactions \
  smoke-audit-fixes \
  fix/v0.4.9-security-patch \
  chore/bench-depot-runner \
  release/0.4.7 \
  chore/ts-client-0.5.0 \
  fix/fly-bind-ipv6 \
  chore/gold-standard-prod-hardening \
  release/0.4.8
```

## Evidence by safe-delete candidate

| Remote branch | Tip | Git ancestor of `origin/main`? | Ahead/behind vs `origin/main` | PR evidence | Safe-delete rationale |
| --- | --- | --- | --- | --- | --- |
| `origin/explicit-transactions` | `a89552c` | yes | `82 0` | PR #58 `MERGED`, head `a89552c`, merge `15fc8f2`, merged `2026-05-27T05:01:23Z` | Explicit target branch is already contained in `origin/main`; PR is merged and branch tip matches PR head. |
| `origin/smoke-audit-fixes` | `cf70bf8` | yes | `78 0` | PR #56 `MERGED`, head `cf70bf8`, merge `15fc8f2`, merged `2026-05-27T05:01:23Z` | Already contained in `origin/main`; PR is merged and branch tip matches PR head. |
| `origin/fix/v0.4.9-security-patch` | `c3a837e` | yes | `1 0` | PR #91 `MERGED`, head `c3a837e`, merge `c3a73bc`, merged `2026-06-15T04:37:03Z` | Already contained in `origin/main`; PR is merged and branch tip matches PR head. |
| `origin/chore/bench-depot-runner` | `aed0dc3` | no | `20 5` | PR #76 `MERGED`, head `aed0dc3`, merge `f4152e1`, merged `2026-06-09T01:32:57Z` | Squash/rebase merge leaves non-ancestor commits, but GitHub merged the PR and current remote tip equals the merged PR head. |
| `origin/release/0.4.7` | `be17bdc` | no | `7 1` | PR #86 `MERGED`, head `be17bdc`, merge `408be65`, merged `2026-06-10T04:27:12Z` | Squash/rebase merge; current remote tip equals merged PR head. |
| `origin/chore/ts-client-0.5.0` | `3707b5c` | no | `6 1` | PR #87 `MERGED`, head `3707b5c`, merge `0001c33`, merged `2026-06-10T04:41:26Z` | Squash/rebase merge; current remote tip equals merged PR head. |
| `origin/fix/fly-bind-ipv6` | `0a6cb02` | no | `5 1` | PR #88 `MERGED`, head `0a6cb02`, merge `23707c7`, merged `2026-06-10T05:02:52Z` | Squash/rebase merge; current remote tip equals merged PR head. |
| `origin/chore/gold-standard-prod-hardening` | `e0a90dc` | no | `4 1` | PR #89 `MERGED`, head `e0a90dc`, merge `fa58095`, merged `2026-06-13T05:21:27Z` | Squash/rebase merge; current remote tip equals merged PR head. |
| `origin/release/0.4.8` | `067993a` | no | `3 1` | PR #90 `MERGED`, head `067993a`, merge `aa6b564`, merged `2026-06-14T03:36:18Z` | Squash/rebase merge; current remote tip equals merged PR head. |

## Do not delete in this pass

These branches are not safe-delete candidates from the current evidence:

| Remote branch | Tip | Reason |
| --- | --- | --- |
| `origin/docs/remaining-work-backlog` | `b1392a0` | PR #96 is `OPEN`; branch is ahead of `origin/main` by 1 commit. |
| `origin/dependabot/npm_and_yarn/clients/ts/dev-a243c58be6` | `edfcf15` | PR #92 is `OPEN`; branch is ahead of `origin/main` by 1 commit. |
| `origin/dependabot/github_actions/actions-6cc2358003` | `b7ea572` | PR #93 is `OPEN`; branch is ahead of `origin/main` by 1 commit. |
| `origin/dependabot/cargo/patch-c9529a37dd` | `0d1c52a` | PR #94 is `OPEN`; branch is ahead of `origin/main` by 1 commit. |
| `origin/dependabot/cargo/zeroize-1.9.0` | `97b4067` | PR #95 is `OPEN`; branch is ahead of `origin/main` by 1 commit. |
| `origin/claude/audit-drivers-platform-BaXOH` | `e09ee39` | No matching PR found by `gh pr list --state all --head`; branch is not merged and is ahead by 1 commit. Needs owner decision, not safe-delete automation. |

## Worktree and divergence safety notes

`git worktree list --porcelain` showed these attached worktrees:

```text
/Users/macbookpro-kirby/Desktop/Coding/ZVN/PowDB -> main @ e879cc6
worker-1 -> detached @ c3a73bc
worker-2 -> detached @ 52f087f
worker-3 -> detached @ e879cc6
```

None of the remote-delete candidate branch names are checked out in these worktrees.
Local `main...origin/main` divergence is `3 0`; therefore local `main` is not the
remote-delete baseline for this audit. The safe-delete list above is based on
`origin/main` plus GitHub PR merge evidence.

## Raw branch classification snapshot

`git branch -r --merged origin/main` returned:

```text
origin/HEAD -> origin/main
origin/explicit-transactions
origin/fix/v0.4.9-security-patch
origin/main
origin/smoke-audit-fixes
```

`git branch -r --no-merged origin/main` returned:

```text
origin/chore/bench-depot-runner
origin/chore/gold-standard-prod-hardening
origin/chore/ts-client-0.5.0
origin/claude/audit-drivers-platform-BaXOH
origin/dependabot/cargo/patch-c9529a37dd
origin/dependabot/cargo/zeroize-1.9.0
origin/dependabot/github_actions/actions-6cc2358003
origin/dependabot/npm_and_yarn/clients/ts/dev-a243c58be6
origin/docs/remaining-work-backlog
origin/fix/fly-bind-ipv6
origin/release/0.4.7
origin/release/0.4.8
```
