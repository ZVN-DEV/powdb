#!/usr/bin/env bash
# scripts/ci/check-tool-pins.sh: every `cargo install` in a workflow is pinned,
# and a tool installed by more than one workflow is pinned to the same version
# in all of them.
#
# The commit that pinned "the third-party tools the gates run on" pinned
# gitleaks (by sha256), cargo-semver-checks (by sha256), cargo-audit
# (--version --locked) and ci.yml's corpus-replay cargo-fuzz
# (--version 0.13.2 --locked), and left fuzz.yml's `cargo install cargo-fuzz`
# floating. The nightly campaign's compiler was pinned to the day while its
# harness was whatever crates.io served that morning, which is precisely what
# the sibling job's own comment argues against: it makes the verdict a function
# of the day rather than of the commit.
#
# Nothing noticed, because "we pinned the tools" was a claim in a commit
# message rather than a check. This is the check. It fails on:
#
#   * a `cargo install` with no --version, or no --locked
#   * the same tool pinned to two different versions across workflows
#   * finding no `cargo install` at all (the scan having gone vacuous)
#
# Usage:
#   check-tool-pins.sh             scan .github/workflows
#   check-tool-pins.sh --selftest  prove each refusal fires
#
# Env:
#   WORKFLOW_DIR  directory to scan (default .github/workflows)

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORKFLOW_DIR="${WORKFLOW_DIR:-${REPO_ROOT}/.github/workflows}"

# version_of <file> <spec>: resolve the argument of `--version`. A literal
# comes back as itself. "$VAR" or "${VAR}" is looked up as `VAR: <rhs>` in the
# same file, and the whole right-hand side is returned, so a value that is a
# workflow expression comes back looking like one. Empty means the name is
# defined nowhere, which is a typo waiting to install the wrong thing.
version_of() {
  local file="$1" spec="$2" name
  case "${spec}" in
    *'$'*)
      name="$(tr -cd 'A-Za-z0-9_' <<<"${spec}")"
      [[ -n "${name}" ]] || return 0
      sed -nE "s/^[[:space:]]*${name}:[[:space:]]*(.*[^[:space:]])[[:space:]]*\$/\1/p" "${file}" | head -1
      ;;
    *)
      sed -E 's/^"//; s/"$//' <<<"${spec}"
      ;;
  esac
}

scan() {
  if [[ ! -d "${WORKFLOW_DIR}" ]]; then
    echo "::error::check-tool-pins: no workflow directory at ${WORKFLOW_DIR}" >&2
    return 1
  fi

  local status=0 seen=0
  # tool -> "version@file" of the first sighting, so a disagreement can name both.
  local -a pinned_tools=() pinned_versions=() pinned_files=()

  local hits
  hits="$(grep -rn -- 'cargo install ' "${WORKFLOW_DIR}")"
  local grep_status=$?
  if [[ "${grep_status}" -gt 1 ]]; then
    echo "::error::check-tool-pins: the scan of ${WORKFLOW_DIR} failed (grep exit ${grep_status})" >&2
    return 1
  fi

  local line file lineno text tool spec version i
  while IFS= read -r line; do
    [[ -z "${line}" ]] && continue
    file="${line%%:*}"
    lineno="${line#*:}"; lineno="${lineno%%:*}"
    text="${line#*:*:}"
    # Only real command lines. A YAML comment or a step `name:` that happens to
    # contain the phrase is prose, not an install, and counting it as one gave
    # this script a false positive on the very step that runs it.
    case "${text}" in
      *'#'*'cargo install'*) continue ;;
    esac
    if [[ "${text}" =~ ^[[:space:]]*#  ]] || [[ "${text}" =~ ^[[:space:]]*-?[[:space:]]*name: ]]; then
      continue
    fi
    # The crate name is the word after `cargo install`.
    tool="$(sed -E 's/.*cargo install[[:space:]]+([A-Za-z0-9_.-]+).*/\1/' <<<"${text}")"
    [[ -n "${tool}" && "${tool}" != "${text}" ]] || continue
    seen=$((seen + 1))

    if ! grep -q -- '--version' <<<"${text}"; then
      echo "::error::${file}:${lineno} installs ${tool} with no --version." >&2
      echo "         An unpinned build tool makes this job's verdict a function of the day" >&2
      echo "         crates.io was read, not of the commit under test." >&2
      status=1
      continue
    fi
    if ! grep -q -- '--locked' <<<"${text}"; then
      echo "::error::${file}:${lineno} installs ${tool} with --version but no --locked, so its own dependency tree still floats." >&2
      status=1
      continue
    fi

    spec="$(sed -E 's/.*--version[[:space:]]+([^[:space:]]+).*/\1/' <<<"${text}")"
    version="$(version_of "${file}" "${spec}")"
    if [[ -z "${version}" ]]; then
      echo "::error::${file}:${lineno} pins ${tool} to ${spec}, which is defined nowhere in that file." >&2
      status=1
      continue
    fi
    # A version that is a workflow expression is resolved at dispatch time.
    # post-publish-smoke installs the release under test that way, which is the
    # point of that job: it is pinned, just not to a constant. Nothing to
    # compare across files, so it does not join the agreement check.
    # shellcheck disable=SC2016  # matching the literal ${{ of a workflow expression
    if [[ "${version}" == *'${{'* ]]; then
      continue
    fi
    version="$(sed -E 's/^"//; s/"$//' <<<"${version}")"

    for (( i = 0; i < ${#pinned_tools[@]}; i++ )); do
      if [[ "${pinned_tools[i]}" == "${tool}" && "${pinned_versions[i]}" != "${version}" ]]; then
        echo "::error::${tool} is pinned to ${pinned_versions[i]} in ${pinned_files[i]} and ${version} in ${file}:${lineno}." >&2
        echo "         Two workflows running different builds of the same gate tool do not" >&2
        echo "         corroborate each other." >&2
        status=1
      fi
    done
    pinned_tools+=("${tool}")
    pinned_versions+=("${version}")
    pinned_files+=("${file}:${lineno}")
  done <<<"${hits}"

  if [[ "${seen}" -eq 0 ]]; then
    echo "::error::check-tool-pins: found no 'cargo install' line under ${WORKFLOW_DIR}; this check has gone vacuous" >&2
    return 1
  fi

  if [[ "${status}" -eq 0 ]]; then
    echo "check-tool-pins: ${seen} cargo install site(s), all pinned by --version and --locked, no version disagreements."
  fi
  return "${status}"
}

# --- selftest -------------------------------------------------------------

fail_selftest() {
  echo "::error::check-tool-pins selftest: $*" >&2
  exit 1
}

run_case() {
  # run_case <label> <pass|fail> <dir>
  local label="$1" expect="$2" dir="$3" log status
  log="${SELFTEST_WORK}/$(tr -c 'a-zA-Z0-9' '-' <<<"${label}").log"
  WORKFLOW_DIR="${dir}" bash "${SELFTEST_SELF}" > "${log}" 2>&1
  status=$?
  if [[ "${expect}" == pass && "${status}" -ne 0 ]]; then
    sed 's/^/    /' "${log}" >&2
    fail_selftest "${label}: expected exit 0, got ${status}"
  fi
  if [[ "${expect}" == fail && "${status}" -eq 0 ]]; then
    sed 's/^/    /' "${log}" >&2
    fail_selftest "${label}: expected a non-zero exit, got 0; this refusal cannot fire"
  fi
  SELFTEST_LOG="${log}"
  echo "  ok: ${label} (exit ${status})"
}

selftest() {
  # Not `local`: the EXIT trap fires after this function returns.
  SELFTEST_WORK="$(mktemp -d "${TMPDIR:-/tmp}/powdb-tool-pins-selftest.XXXXXX")" || fail_selftest "mktemp -d failed"
  trap 'rm -rf "${SELFTEST_WORK}"' EXIT
  SELFTEST_SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
  local w="${SELFTEST_WORK}"

  mkdir -p "${w}/good"
  cat > "${w}/good/a.yml" <<'YML'
      - name: Install cargo-fuzz (pinned)
        env:
          CARGO_FUZZ_VERSION: 0.13.2
        run: command -v cargo-fuzz >/dev/null 2>&1 || cargo install cargo-fuzz --version "$CARGO_FUZZ_VERSION" --locked
YML
  cat > "${w}/good/b.yml" <<'YML'
      - name: Install cargo-fuzz (pinned)
        env:
          CARGO_FUZZ_VERSION: 0.13.2
        run: cargo install cargo-fuzz --version "$CARGO_FUZZ_VERSION" --locked
YML
  run_case "matching pins pass" pass "${w}/good"

  # The finding: an unpinned install alongside a pinned one.
  mkdir -p "${w}/unpinned"
  cp "${w}/good/a.yml" "${w}/unpinned/a.yml"
  cat > "${w}/unpinned/b.yml" <<'YML'
      - name: Install cargo-fuzz
        run: cargo install cargo-fuzz
YML
  run_case "an unpinned cargo install fails" fail "${w}/unpinned"
  grep -q 'no --version' "${SELFTEST_LOG}" || {
    sed 's/^/    /' "${SELFTEST_LOG}" >&2
    fail_selftest "an unpinned install was not reported as unpinned"
  }

  # Prose that mentions the phrase is not an install.
  mkdir -p "${w}/prose"
  cp "${w}/good/a.yml" "${w}/prose/a.yml"
  cat > "${w}/prose/b.yml" <<'YML'
      # every cargo install in a workflow is pinned
      - name: Every cargo install in a workflow is pinned
        run: bash scripts/ci/check-tool-pins.sh
YML
  run_case "prose mentioning the phrase is not an install" pass "${w}/prose"

  mkdir -p "${w}/unlocked"
  cp "${w}/good/a.yml" "${w}/unlocked/a.yml"
  cat > "${w}/unlocked/b.yml" <<'YML'
        run: cargo install cargo-fuzz --version 0.13.2
YML
  run_case "a --version without --locked fails" fail "${w}/unlocked"

  mkdir -p "${w}/disagree"
  cp "${w}/good/a.yml" "${w}/disagree/a.yml"
  cat > "${w}/disagree/b.yml" <<'YML'
        run: cargo install cargo-fuzz --version 0.12.0 --locked
YML
  run_case "two workflows pinning one tool differently fails" fail "${w}/disagree"
  grep -q 'is pinned to' "${SELFTEST_LOG}" || {
    sed 's/^/    /' "${SELFTEST_LOG}" >&2
    fail_selftest "a version disagreement was not reported as one"
  }

  # A version resolved at dispatch time is pinned, just not to a constant.
  mkdir -p "${w}/dynamic"
  cp "${w}/good/a.yml" "${w}/dynamic/a.yml"
  cat > "${w}/dynamic/b.yml" <<'YML'
        env:
          SMOKE_VERSION: ${{ inputs.version }}
        run: cargo install powdb-cli --version "$SMOKE_VERSION" --locked
YML
  run_case "a dispatch-time version is accepted" pass "${w}/dynamic"

  mkdir -p "${w}/dangling"
  cat > "${w}/dangling/a.yml" <<'YML'
        run: cargo install cargo-fuzz --version "$NOT_DEFINED_ANYWHERE" --locked
YML
  run_case "a --version naming an undefined variable fails" fail "${w}/dangling"

  mkdir -p "${w}/empty"
  cat > "${w}/empty/a.yml" <<'YML'
      - name: nothing to see
        run: echo hello
YML
  run_case "a scan that matches nothing fails" fail "${w}/empty"
  grep -q 'gone vacuous' "${SELFTEST_LOG}" || {
    sed 's/^/    /' "${SELFTEST_LOG}" >&2
    fail_selftest "an empty scan did not report itself vacuous"
  }

  echo "check-tool-pins: selftest ok"
}

case "${1:-}" in
  --selftest) selftest ;;
  '') scan ;;
  *)
    echo "::error::check-tool-pins: usage: check-tool-pins.sh [--selftest]" >&2
    exit 1
    ;;
esac
