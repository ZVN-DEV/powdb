#!/usr/bin/env bash
set -euo pipefail

MIN_RUNS=5
WRITE_IMPROVEMENT_REQUIRED=0.25
POINT_READ_IMPROVEMENT_REQUIRED=0.30
PROTECTED_REGRESSION_LIMIT=0.10
RELATIVE_SPREAD_LIMIT=0.20

usage() {
  cat >&2 <<'USAGE'
usage: scripts/paired-bench.sh \
  --baseline-bin /path/to/baseline/powdb-compare-paired \
  --candidate-bin /path/to/candidate/powdb-compare-paired \
  [--baseline-ref LABEL] [--candidate-ref LABEL] [--mode full|off] [--runs 5] \
  [--fixture-rows N] [--point-read-ops N] [--write-ops N] [--scan-ops N] [--batch-size N] \
  [--require-improvement] [--contaminated] [--contamination-note TEXT] \
  [--output result.json]

Selftest:
  scripts/paired-bench.sh --selftest

Runs explicit baseline/candidate binaries only. It creates temporary fixture
directories through the driver and never targets external database URLs.
USAGE
}

baseline_bin=
candidate_bin=
baseline_ref=baseline
candidate_ref=candidate
mode=full
runs=5
fixture_rows=20000
point_read_ops=5000
write_ops=1000
scan_ops=200
batch_size=100
output=
require_improvement=false
contaminated=false
contamination_note=
selftest=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --baseline-bin) baseline_bin=${2:?missing --baseline-bin value}; shift 2 ;;
    --candidate-bin) candidate_bin=${2:?missing --candidate-bin value}; shift 2 ;;
    --baseline-ref) baseline_ref=${2:?missing --baseline-ref value}; shift 2 ;;
    --candidate-ref) candidate_ref=${2:?missing --candidate-ref value}; shift 2 ;;
    --mode) mode=${2:?missing --mode value}; shift 2 ;;
    --runs) runs=${2:?missing --runs value}; shift 2 ;;
    --fixture-rows) fixture_rows=${2:?missing --fixture-rows value}; shift 2 ;;
    --point-read-ops) point_read_ops=${2:?missing --point-read-ops value}; shift 2 ;;
    --write-ops) write_ops=${2:?missing --write-ops value}; shift 2 ;;
    --scan-ops) scan_ops=${2:?missing --scan-ops value}; shift 2 ;;
    --batch-size) batch_size=${2:?missing --batch-size value}; shift 2 ;;
    --output) output=${2:?missing --output value}; shift 2 ;;
    --require-improvement) require_improvement=true; shift ;;
    --contaminated) contaminated=true; shift ;;
    --contamination-note) contamination_note=${2:?missing --contamination-note value}; shift 2 ;;
    --selftest) selftest=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

hash_file() {
  shasum -a 256 "$1" | awk '{print $1}'
}

aggregate_jsonl() {
  local jsonl=$1
  local baseline=$2
  local candidate=$3
  local aggregate_mode=$4
  local expected_runs=$5
  local require_flag=$6
  local contamination_flag=$7
  local contamination_text=$8
  local destination=${9:-}
  local host_os
  local host_arch
  host_os=$(uname -s)
  host_arch=$(uname -m)

  python3 - "$jsonl" "$baseline" "$candidate" "$aggregate_mode" "$expected_runs" \
    "$require_flag" "$contamination_flag" "$contamination_text" "$host_os" "$host_arch" \
    "$MIN_RUNS" "$WRITE_IMPROVEMENT_REQUIRED" "$POINT_READ_IMPROVEMENT_REQUIRED" \
    "$PROTECTED_REGRESSION_LIMIT" "$RELATIVE_SPREAD_LIMIT" <<'PY' > "${destination:-/dev/stdout}"
import json
import statistics
import sys
from collections import defaultdict

(
    jsonl,
    baseline_ref,
    candidate_ref,
    mode,
    runs_raw,
    require_improvement_raw,
    contaminated_raw,
    contamination_note,
    host_os,
    host_arch,
    min_runs_raw,
    write_required_raw,
    point_required_raw,
    protected_limit_raw,
    spread_limit_raw,
) = sys.argv[1:16]

runs = int(runs_raw)
min_runs = int(min_runs_raw)
write_required = float(write_required_raw)
point_required = float(point_required_raw)
protected_limit = float(protected_limit_raw)
spread_limit = float(spread_limit_raw)
require_improvement = require_improvement_raw == "true"
contaminated = contaminated_raw == "true"

records = []
with open(jsonl, "r", encoding="utf-8") as fh:
    for line in fh:
        line = line.strip()
        if line:
            records.append(json.loads(line))

expected = runs * 2 * 2
invalid = []
quality_blockers = []
not_publishable_reasons = []

if baseline_ref == candidate_ref:
    invalid.append("baseline and candidate labels must be distinct")
if len(records) != expected:
    invalid.append(f"expected {expected} raw records, got {len(records)}")
if runs < min_runs:
    not_publishable_reasons.append(f"run count {runs} is below required minimum {min_runs}")
if contaminated:
    not_publishable_reasons.append(f"run marked contaminated: {contamination_note or 'no note provided'}")

profile_values = set()
mode_values = set()
settings_values = set()
fixture_values = set()
platform_values = set()
hash_by_ref = defaultdict(set)

for rec in records:
    ref = rec.get("engine_ref")
    engine = rec.get("engine")
    if rec.get("mode") != mode:
        invalid.append(f"mode mismatch for {ref} {engine}: {rec.get('mode')}")
    profile_values.add(rec.get("profile"))
    mode_values.add(rec.get("mode"))
    settings_values.add(json.dumps(rec.get("settings", {}), sort_keys=True))
    fixture_values.add(json.dumps(rec.get("fixture", {}), sort_keys=True))
    hash_value = rec.get("engine_hash")
    if not hash_value or hash_value == "unknown":
        not_publishable_reasons.append(f"missing binary hash for {ref} {engine}")
    else:
        hash_by_ref[ref].add(hash_value)
    platform = rec.get("platform")
    if platform:
        platform_values.add(json.dumps(platform, sort_keys=True))
        if platform.get("os") not in (None, host_os) or platform.get("arch") not in (None, host_arch):
            invalid.append(f"platform mismatch for {ref} {engine}: {platform} vs host {host_os}/{host_arch}")
    failed = [c["name"] for c in rec.get("checks", []) if not c.get("passed")]
    if failed:
        invalid.append(f"{ref} {engine} failed checks: {failed}")

if len(profile_values) != 1 or None in profile_values:
    invalid.append(f"profile mismatch across records: {sorted(str(v) for v in profile_values)}")
if mode_values != {mode}:
    invalid.append(f"mode mismatch across records: {sorted(str(v) for v in mode_values)}")
if len(settings_values) != 1:
    invalid.append("settings mismatch across records")
if len(fixture_values) != 1:
    invalid.append("fixture mismatch across records")
if len(platform_values) > 1:
    invalid.append("driver platform mismatch across records")

baseline_hashes = hash_by_ref.get(baseline_ref, set())
candidate_hashes = hash_by_ref.get(candidate_ref, set())
if len(baseline_hashes) != 1:
    not_publishable_reasons.append(f"baseline hash provenance is not singular: {sorted(baseline_hashes)}")
if len(candidate_hashes) != 1:
    not_publishable_reasons.append(f"candidate hash provenance is not singular: {sorted(candidate_hashes)}")
if baseline_hashes and candidate_hashes and baseline_hashes == candidate_hashes:
    not_publishable_reasons.append("baseline and candidate binary hashes are identical")

samples = defaultdict(list)
provenance = {}
for rec in records:
    ref = rec["engine_ref"]
    engine = rec["engine"]
    provenance[ref] = {
        "engine_hashes": sorted(hash_by_ref.get(ref, [])),
        "storage_by_engine": {
            **provenance.get(ref, {}).get("storage_by_engine", {}),
            engine: rec["storage"],
        },
    }
    for workload in rec["workloads"]:
        samples[(ref, engine, workload["name"])].append(workload["mean_ns_per_op"])

summary = []
too_noisy = []
for (ref, engine, workload), values in sorted(samples.items()):
    values = sorted(values)
    median = statistics.median(values)
    spread = values[-1] - values[0] if values else 0.0
    relative_spread = (spread / median) if median else None
    row = {
        "engine_ref": ref,
        "engine": engine,
        "workload": workload,
        "runs": len(values),
        "median_ns_per_op": median,
        "min_ns_per_op": values[0],
        "max_ns_per_op": values[-1],
        "spread_ns_per_op": spread,
        "relative_spread": relative_spread,
        "raw_mean_ns_per_op": values,
    }
    summary.append(row)
    if engine == "powdb" and relative_spread is not None and relative_spread > spread_limit:
        too_noisy.append({
            "engine_ref": ref,
            "workload": workload,
            "relative_spread": relative_spread,
            "limit": spread_limit,
        })

if too_noisy:
    quality_blockers.append("relative spread exceeded policy")
    not_publishable_reasons.append("relative spread exceeded policy")

def median_for(ref, workload):
    vals = samples.get((ref, "powdb", workload), [])
    if not vals:
        return None
    return statistics.median(vals)

point_base = median_for(baseline_ref, "point_read_indexed")
point_cand = median_for(candidate_ref, "point_read_indexed")
write_workloads = ["point_update_changed_value", "prepared_insert_single", "prepared_insert_bounded_batch"]
write_improvements = []
for workload in write_workloads:
    base = median_for(baseline_ref, workload)
    cand = median_for(candidate_ref, workload)
    if base and cand is not None:
        write_improvements.append({
            "workload": workload,
            "baseline_median_ns_per_op": base,
            "candidate_median_ns_per_op": cand,
            "improvement": (base - cand) / base,
        })

point_improvement = None
if point_base and point_cand is not None:
    point_improvement = (point_base - point_cand) / point_base

protected_regressions = []
for workload in ["point_read_indexed", "protected_scan_filter_count", "protected_aggregate_sum"]:
    base = median_for(baseline_ref, workload)
    cand = median_for(candidate_ref, workload)
    if base and cand is not None:
        regression = (cand - base) / base
        if regression > protected_limit:
            protected_regressions.append({
                "workload": workload,
                "baseline_median_ns_per_op": base,
                "candidate_median_ns_per_op": cand,
                "regression": regression,
            })

write_pass = any(item["improvement"] >= write_required for item in write_improvements)
point_pass = point_improvement is not None and point_improvement >= point_required
checks_pass = not invalid
fixed_threshold_pass = checks_pass and (write_pass or point_pass) and not protected_regressions
durable_publishable = (
    mode == "full"
    and checks_pass
    and fixed_threshold_pass
    and runs >= min_runs
    and not too_noisy
    and not contaminated
    and not not_publishable_reasons
)

if mode == "off" and fixed_threshold_pass:
    status = "diagnostic_pass"
elif durable_publishable:
    status = "durable_publishable"
elif not checks_pass:
    status = "invalid"
else:
    status = "not_publishable"

result = {
    "schema": "powdb.compare.paired.aggregate.v2",
    "mode": mode,
    "runs": runs,
    "profile": next(iter(profile_values)) if len(profile_values) == 1 else None,
    "baseline_ref": baseline_ref,
    "candidate_ref": candidate_ref,
    "host": {"os": host_os, "arch": host_arch},
    "provenance": provenance,
    "valid": checks_pass,
    "invalid_reasons": invalid,
    "quality_blockers": quality_blockers,
    "not_publishable_reasons": not_publishable_reasons,
    "evaluation": {
        "status": status,
        "engineering_contract_pass": fixed_threshold_pass,
        "durable_publishable": durable_publishable,
        "contaminated": contaminated,
        "contamination_note": contamination_note,
        "criteria": {
            "min_runs": min_runs,
            "candidate_write_improvement_required": write_required,
            "candidate_point_read_improvement_required": point_required,
            "protected_regression_limit": protected_limit,
            "relative_spread_limit": spread_limit,
        },
        "write_pass": write_pass,
        "point_read_pass": point_pass,
        "point_read_improvement": point_improvement,
        "write_improvements": write_improvements,
        "protected_regressions": protected_regressions,
        "too_noisy": too_noisy,
        "note": "Off mode can satisfy the engineering improvement contract but is never a durable performance claim.",
    },
    "summary": summary,
    "raw_records": records,
}
json.dump(result, sys.stdout, indent=2, sort_keys=True)
sys.stdout.write("\n")

if not checks_pass:
    sys.exit(1)
if require_improvement and not fixed_threshold_pass:
    sys.exit(3)
PY
}

make_synthetic_jsonl() {
  local destination=$1
  local scenario=$2
  python3 - "$destination" "$scenario" <<'PY'
import json
import sys

destination, scenario = sys.argv[1], sys.argv[2]
runs = 4 if scenario == "under5" else 5
records = []
settings = {
    "point_read_ops": 5000,
    "write_ops": 1000,
    "scan_ops": 200,
    "batch_size": 100,
    "protected_regression_limit": 0.10,
    "candidate_write_improvement_required": 0.25,
    "candidate_point_read_improvement_required": 0.30,
}
fixture = {
    "rows": 20000,
    "key_strategy": "deterministic-varied-mix64",
    "correctness_outside_timing": True,
}

def values(ref, workload):
    base = {
        "point_read_indexed": 100.0,
        "point_update_changed_value": 100.0,
        "prepared_insert_single": 100.0,
        "prepared_insert_bounded_batch": 100.0,
        "protected_scan_filter_count": 100.0,
        "protected_aggregate_sum": 100.0,
    }[workload]
    if ref == "candidate":
        if scenario in ("win", "offwin", "under5", "settings_mismatch"):
            if workload == "point_update_changed_value":
                base = 70.0
        elif scenario == "regression":
            if workload == "point_update_changed_value":
                base = 70.0
            if workload == "protected_scan_filter_count":
                base = 112.0
        elif scenario == "point_regression":
            if workload == "point_update_changed_value":
                base = 70.0
            if workload == "point_read_indexed":
                base = 112.0
        elif scenario == "noise" and workload == "point_update_changed_value":
            return [60.0, 100.0, 60.0, 100.0, 60.0][:runs]
    return [base, base * 1.01, base * 0.99, base * 1.005, base * 0.995][:runs]

for round_index in range(runs):
    for ref, hash_value in [("baseline", "basehash"), ("candidate", "candhash")]:
        for engine in ["powdb", "sqlite"]:
            rec_settings = dict(settings)
            if scenario == "settings_mismatch" and ref == "candidate" and engine == "powdb" and round_index == 0:
                rec_settings["write_ops"] = 999
            workloads = []
            for workload in [
                "point_read_indexed",
                "point_update_changed_value",
                "prepared_insert_single",
                "prepared_insert_bounded_batch",
                "protected_scan_filter_count",
                "protected_aggregate_sum",
            ]:
                sample = values(ref, workload)[round_index]
                workloads.append({
                    "name": workload,
                    "operations": 1,
                    "mean_ns_per_op": sample,
                    "protected": workload.startswith("protected_"),
                    "timed": True,
                })
            mode = "off" if scenario == "offwin" else "full"
            records.append({
                "schema": "powdb.compare.paired.v1",
                "profile": "value-v1",
                "engine": engine,
                "engine_ref": ref,
                "engine_hash": hash_value,
                "mode": mode,
                "storage": "file-backed-wal-full" if mode == "full" else "memory-diagnostic",
                "round": round_index,
                "order_index": 0,
                "fixture": fixture,
                "settings": rec_settings,
                "workloads": workloads,
                "checks": [{"name": "synthetic", "passed": True, "detail": "ok"}],
            })

with open(destination, "w", encoding="utf-8") as fh:
    for rec in records:
        fh.write(json.dumps(rec, sort_keys=True) + "\n")
PY
}

assert_json_field() {
  local file=$1
  local expr=$2
  python3 - "$file" "$expr" <<'PY'
import json
import sys
with open(sys.argv[1], "r", encoding="utf-8") as fh:
    data = json.load(fh)
if not eval(sys.argv[2], {"data": data}):
    raise SystemExit(f"assertion failed: {sys.argv[2]}")
PY
}

run_selftest() {
  local tmp_dir
  tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/powdb-paired-selftest.XXXXXX")
  trap "rm -rf '$tmp_dir'" EXIT

  make_synthetic_jsonl "$tmp_dir/win.jsonl" win
  aggregate_jsonl "$tmp_dir/win.jsonl" baseline candidate full 5 true false "" "$tmp_dir/win.json"
  assert_json_field "$tmp_dir/win.json" 'data["evaluation"]["durable_publishable"] is True'

  make_synthetic_jsonl "$tmp_dir/offwin.jsonl" offwin
  aggregate_jsonl "$tmp_dir/offwin.jsonl" baseline candidate off 5 true false "" "$tmp_dir/offwin.json"
  assert_json_field "$tmp_dir/offwin.json" 'data["evaluation"]["engineering_contract_pass"] is True and data["evaluation"]["durable_publishable"] is False and data["evaluation"]["status"] == "diagnostic_pass"'

  make_synthetic_jsonl "$tmp_dir/regression.jsonl" regression
  if aggregate_jsonl "$tmp_dir/regression.jsonl" baseline candidate full 5 true false "" "$tmp_dir/regression.json"; then
    echo "expected regression selftest to fail --require-improvement" >&2
    exit 1
  fi
  assert_json_field "$tmp_dir/regression.json" 'data["evaluation"]["engineering_contract_pass"] is False and data["evaluation"]["protected_regressions"]'

  make_synthetic_jsonl "$tmp_dir/point-regression.jsonl" point_regression
  if aggregate_jsonl "$tmp_dir/point-regression.jsonl" baseline candidate full 5 true false "" "$tmp_dir/point-regression.json"; then
    echo "expected point-read regression selftest to fail --require-improvement" >&2
    exit 1
  fi
  assert_json_field "$tmp_dir/point-regression.json" 'data["evaluation"]["engineering_contract_pass"] is False and any(item["workload"] == "point_read_indexed" for item in data["evaluation"]["protected_regressions"])'

  make_synthetic_jsonl "$tmp_dir/noise.jsonl" noise
  aggregate_jsonl "$tmp_dir/noise.jsonl" baseline candidate full 5 false false "" "$tmp_dir/noise.json"
  assert_json_field "$tmp_dir/noise.json" 'data["evaluation"]["engineering_contract_pass"] is True and data["evaluation"]["durable_publishable"] is False and data["evaluation"]["too_noisy"]'

  make_synthetic_jsonl "$tmp_dir/under5.jsonl" under5
  aggregate_jsonl "$tmp_dir/under5.jsonl" baseline candidate full 4 false false "" "$tmp_dir/under5.json"
  assert_json_field "$tmp_dir/under5.json" 'data["evaluation"]["engineering_contract_pass"] is True and data["evaluation"]["durable_publishable"] is False'

  make_synthetic_jsonl "$tmp_dir/settings.jsonl" settings_mismatch
  if aggregate_jsonl "$tmp_dir/settings.jsonl" baseline candidate full 5 false false "" "$tmp_dir/settings.json"; then
    echo "expected settings mismatch selftest to fail validity" >&2
    exit 1
  fi
  assert_json_field "$tmp_dir/settings.json" 'data["valid"] is False and any("settings mismatch" in reason for reason in data["invalid_reasons"])'

  aggregate_jsonl "$tmp_dir/win.jsonl" baseline candidate full 5 false true "manual contamination test" "$tmp_dir/contaminated.json"
  assert_json_field "$tmp_dir/contaminated.json" 'data["evaluation"]["engineering_contract_pass"] is True and data["evaluation"]["durable_publishable"] is False and data["evaluation"]["contaminated"] is True'

  echo "paired-bench selftest passed"
}

if [[ "$selftest" == "true" ]]; then
  run_selftest
  exit 0
fi

if [[ -z "$baseline_bin" || -z "$candidate_bin" ]]; then
  echo "--baseline-bin and --candidate-bin are required" >&2
  usage
  exit 2
fi
if [[ ! -x "$baseline_bin" ]]; then
  echo "baseline binary is not executable: $baseline_bin" >&2
  exit 2
fi
if [[ ! -x "$candidate_bin" ]]; then
  echo "candidate binary is not executable: $candidate_bin" >&2
  exit 2
fi
if [[ "$baseline_ref" == "$candidate_ref" ]]; then
  echo "--baseline-ref and --candidate-ref must be distinct" >&2
  exit 2
fi
if [[ "$mode" != "full" && "$mode" != "off" ]]; then
  echo "--mode must be full or off" >&2
  exit 2
fi
if ! [[ "$runs" =~ ^[0-9]+$ ]] || [[ "$runs" -lt "$MIN_RUNS" ]]; then
  echo "--runs must be an integer >= $MIN_RUNS" >&2
  exit 2
fi

baseline_hash=$(hash_file "$baseline_bin")
candidate_hash=$(hash_file "$candidate_bin")
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/powdb-paired-bench.XXXXXX")
jsonl="$tmp_dir/raw.jsonl"
trap "rm -rf '$tmp_dir'" EXIT

run_one() {
  local bin=$1
  local ref=$2
  local hash=$3
  local engine=$4
  local round=$5
  local order_index=$6
  "$bin" \
    --engine "$engine" \
    --mode "$mode" \
    --profile value-v1 \
    --engine-ref "$ref" \
    --engine-hash "$hash" \
    --round "$round" \
    --order-index "$order_index" \
    --fixture-rows "$fixture_rows" \
    --point-read-ops "$point_read_ops" \
    --write-ops "$write_ops" \
    --scan-ops "$scan_ops" \
    --batch-size "$batch_size" >> "$jsonl"
}

for ((round=0; round<runs; round++)); do
  if (( round % 2 == 0 )); then
    refs=("$baseline_ref" "$candidate_ref")
    bins=("$baseline_bin" "$candidate_bin")
    hashes=("$baseline_hash" "$candidate_hash")
  else
    refs=("$candidate_ref" "$baseline_ref")
    bins=("$candidate_bin" "$baseline_bin")
    hashes=("$candidate_hash" "$baseline_hash")
  fi

  if (( round % 2 == 0 )); then
    engines=(powdb sqlite)
  else
    engines=(sqlite powdb)
  fi

  order_index=0
  for slot in 0 1; do
    for engine in "${engines[@]}"; do
      echo "round=$round ref=${refs[$slot]} engine=$engine mode=$mode" >&2
      run_one "${bins[$slot]}" "${refs[$slot]}" "${hashes[$slot]}" "$engine" "$round" "$order_index"
      order_index=$((order_index + 1))
    done
  done
done

aggregate_jsonl "$jsonl" "$baseline_ref" "$candidate_ref" "$mode" "$runs" \
  "$require_improvement" "$contaminated" "$contamination_note" "$output"
