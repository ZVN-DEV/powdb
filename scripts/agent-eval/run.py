#!/usr/bin/env python3
"""run.py — offline, model-agnostic scorer for the PowDB agent-eval harness.

Reads a candidates JSONL file (one {"task_id": ..., "statement": ...} per
line), runs each statement against a fresh per-candidate copy of the golden
data dir, and scores the CLI output against the matching task's `check` in
tasks.json.

Stdlib only. No model calls, no network. Always exits 0 (this is a scoring
tool, not a CI gate). Writes results.json next to the candidates file and
prints a per-category pass-rate summary.

Usage:
    python3 scripts/agent-eval/run.py <candidates.jsonl>
    python3 scripts/agent-eval/run.py <candidates.jsonl> --tasks <tasks.json>

Setup (once): scripts/agent-eval/setup.sh  (builds the CLI, seeds .golden-data/)

CLI output formats this scorer understands (see crates/cli/src/main.rs
print_local_result / print_table):
  - Scalar      : a single line holding the value, e.g. "5" or "4.25".
  - Rows        : a header line, a "---+---" separator, N data lines,
                  then a "(N rows)" / "(1 row)" trailer. Empty results
                  print "(empty set)".
  - Modified    : "N row(s) affected".
  - Created     : "type NAME created".
  - Executed    : a free-form message (e.g. "index on '...' created",
                  "transaction rolled back").
  - Error       : exit code 1, message on stderr ("Error: ...").
"""

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
CLI = os.path.join(REPO_ROOT, "target", "release", "powdb-cli")
GOLDEN = os.path.join(HERE, ".golden-data")
DEFAULT_TASKS = os.path.join(HERE, "tasks.json")

TIMEOUT_SECS = 30


# ── CLI invocation ───────────────────────────────────────────────────────────


class RunResult:
    def __init__(self, ok, stdout, stderr):
        self.ok = ok  # True when exit code == 0
        self.stdout = stdout
        self.stderr = stderr


def run_statement(statement):
    """Run one statement against a private copy of the golden data dir."""
    tmp = tempfile.mkdtemp(prefix="powdb_eval_")
    data_dir = os.path.join(tmp, "data")
    try:
        shutil.copytree(GOLDEN, data_dir)
        proc = subprocess.run(
            [CLI, "--data-dir", data_dir, "--exec", statement],
            capture_output=True,
            text=True,
            timeout=TIMEOUT_SECS,
        )
        return RunResult(proc.returncode == 0, proc.stdout, proc.stderr)
    except subprocess.TimeoutExpired:
        return RunResult(False, "", "timeout")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ── Output extraction ────────────────────────────────────────────────────────


def _content_lines(stdout):
    """Significant output lines: drop blanks and the table chrome.

    Drops the separator line ("---+---") and the "(N rows)"/"(empty set)"
    trailer so what remains is header + data (for tables) or the lone value
    (for scalars).
    """
    out = []
    for raw in stdout.splitlines():
        line = raw.rstrip()
        if line.strip() == "":
            continue
        if re.fullmatch(r"-[-+]*-?", line.strip()):
            continue
        if re.fullmatch(r"\(\d+ rows?\)", line.strip()):
            continue
        if line.strip() == "(empty set)":
            continue
        out.append(line)
    return out


def extract_scalar(stdout):
    """Last numeric token of the last significant line, as a string.

    Handles the transaction batch case where the scalar count is the last
    of several output blocks.
    """
    lines = _content_lines(stdout)
    if not lines:
        return None
    nums = re.findall(r"-?\d+(?:\.\d+)?", lines[-1])
    if not nums:
        # fall back to the whole last line, trimmed
        return lines[-1].strip()
    return nums[-1]


def is_empty_set(stdout):
    return any(l.strip() == "(empty set)" for l in stdout.splitlines())


def extract_rows(stdout):
    """Parse table data rows into a sorted list of cell lists.

    A table has a header line then data lines, all pipe-delimited. The
    header is the first content line; the rest are data. Returns [] for an
    empty set. Cells are stripped.
    """
    if is_empty_set(stdout):
        return []
    lines = _content_lines(stdout)
    if len(lines) <= 1:
        # no data rows (only a header, or nothing)
        return []
    data = lines[1:]  # drop header
    rows = []
    for line in data:
        cells = [c.strip() for c in line.split("|")]
        rows.append(cells)
    rows.sort()
    return rows


def count_data_rows(stdout):
    """Number of data rows. Prefer the explicit "(N rows)" trailer."""
    m = re.search(r"\((\d+) rows?\)", stdout)
    if m:
        return int(m.group(1))
    if is_empty_set(stdout):
        return 0
    # scalar / modified / created outputs are not row sets
    return 0


# ── Scoring ──────────────────────────────────────────────────────────────────


def score(check, res):
    """Return (passed: bool, detail: str) for one candidate run."""
    ctype = check.get("type")

    if ctype == "error":
        if res.ok:
            return False, "expected the statement to be rejected, but it succeeded"
        return True, "rejected as expected"

    # All non-error checks require a successful run first.
    if not res.ok:
        return False, "statement failed: " + (res.stderr.strip() or "non-zero exit")

    if ctype == "ok":
        return True, "executed"

    if ctype == "scalar":
        got = extract_scalar(res.stdout)
        want = str(check["expected"])
        if got == want:
            return True, "scalar={}".format(got)
        return False, "scalar got={!r} want={!r}".format(got, want)

    if ctype == "rowcount":
        got = count_data_rows(res.stdout)
        want = int(check["expected"])
        if got == want:
            return True, "rowcount={}".format(got)
        return False, "rowcount got={} want={}".format(got, want)

    if ctype == "rows":
        got = extract_rows(res.stdout)
        want = sorted([[str(c) for c in row] for row in check["expected"]])
        if got == want:
            return True, "rows matched ({} rows)".format(len(got))
        return False, "rows got={} want={}".format(got, want)

    return False, "unknown check type: {!r}".format(ctype)


# ── Driver ───────────────────────────────────────────────────────────────────


def load_tasks(path):
    with open(path) as f:
        tasks = json.load(f)
    return {t["id"]: t for t in tasks}


def category_of(task_id):
    return task_id.split("-")[0]


def main(argv):
    if len(argv) < 2:
        print("usage: run.py <candidates.jsonl> [--tasks tasks.json]", file=sys.stderr)
        return 0  # scoring tool: never a hard failure

    candidates_path = argv[1]
    tasks_path = DEFAULT_TASKS
    if "--tasks" in argv:
        tasks_path = argv[argv.index("--tasks") + 1]

    if not os.path.exists(CLI):
        print(
            "error: powdb-cli not found at {}\n  run: bash {}/setup.sh".format(
                CLI, HERE
            ),
            file=sys.stderr,
        )
        return 0
    if not os.path.isdir(GOLDEN):
        print(
            "error: golden data dir not found at {}\n  run: bash {}/setup.sh".format(
                GOLDEN, HERE
            ),
            file=sys.stderr,
        )
        return 0

    tasks = load_tasks(tasks_path)

    results = []
    with open(candidates_path) as f:
        for lineno, raw in enumerate(f, 1):
            raw = raw.strip()
            if not raw:
                continue
            try:
                cand = json.loads(raw)
            except json.JSONDecodeError as e:
                print("line {}: bad JSON: {}".format(lineno, e), file=sys.stderr)
                continue
            task_id = cand.get("task_id")
            statement = cand.get("statement", "")
            task = tasks.get(task_id)
            if task is None:
                results.append(
                    {
                        "task_id": task_id,
                        "passed": False,
                        "detail": "no such task_id in tasks.json",
                    }
                )
                continue
            res = run_statement(statement)
            passed, detail = score(task["check"], res)
            results.append(
                {
                    "task_id": task_id,
                    "passed": passed,
                    "detail": detail,
                    "statement": statement,
                }
            )

    # ── report ────────────────────────────────────────────────────────────
    total = len(results)
    passed = sum(1 for r in results if r["passed"])

    print()
    for r in results:
        mark = "PASS" if r["passed"] else "FAIL"
        print("  [{}] {:<10} {}".format(mark, r["task_id"], r["detail"]))

    # per-category rollup
    cats = {}
    for r in results:
        c = category_of(r["task_id"] or "")
        cats.setdefault(c, [0, 0])
        cats[c][1] += 1
        if r["passed"]:
            cats[c][0] += 1

    print("\n  by category:")
    for c in sorted(cats):
        p, n = cats[c]
        print("    {:<10} {}/{}".format(c, p, n))

    print("\n  TOTAL: {}/{} passed".format(passed, total))

    out_path = os.path.join(os.path.dirname(os.path.abspath(candidates_path)), "results.json")
    with open(out_path, "w") as f:
        json.dump(
            {"total": total, "passed": passed, "results": results}, f, indent=2
        )
    print("  wrote {}".format(out_path))

    return 0  # always succeed


if __name__ == "__main__":
    sys.exit(main(sys.argv))
