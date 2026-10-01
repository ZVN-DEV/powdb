#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: scripts/paired-bench.sh \
  --baseline-bin /path/to/baseline/powdb-compare-paired \
  --candidate-bin /path/to/candidate/powdb-compare-paired \
  [--baseline-ref LABEL] [--candidate-ref LABEL] [--mode full|off] [--runs 5] \
  [--fixture-rows N] [--point-read-ops N] [--write-ops N] [--scan-ops N] [--batch-size N] \
  [--output result.json]

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
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

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
if [[ "$mode" != "full" && "$mode" != "off" ]]; then
  echo "--mode must be full or off" >&2
  exit 2
fi
if ! [[ "$runs" =~ ^[0-9]+$ ]] || [[ "$runs" -lt 1 ]]; then
  echo "--runs must be a positive integer" >&2
  exit 2
fi

hash_file() {
  shasum -a 256 "$1" | awk '{print $1}'
}

baseline_hash=$(hash_file "$baseline_bin")
candidate_hash=$(hash_file "$candidate_bin")
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/powdb-paired-bench.XXXXXX")
jsonl="$tmp_dir/raw.jsonl"
trap 'rm -rf "$tmp_dir"' EXIT

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

python3 - "$jsonl" "$baseline_ref" "$candidate_ref" "$mode" "$runs" <<'PY' > "${output:-/dev/stdout}"
import json
import statistics
import sys
from collections import defaultdict

jsonl, baseline_ref, candidate_ref, mode, runs = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5])
records = []
with open(jsonl, "r", encoding="utf-8") as fh:
    for line in fh:
        line = line.strip()
        if line:
            records.append(json.loads(line))

expected = runs * 2 * 2
invalid = []
if len(records) != expected:
    invalid.append(f"expected {expected} raw records, got {len(records)}")
for rec in records:
    if rec.get("mode") != mode:
        invalid.append(f"mode mismatch for {rec.get('engine_ref')} {rec.get('engine')}: {rec.get('mode')}")
    failed = [c["name"] for c in rec.get("checks", []) if not c.get("passed")]
    if failed:
        invalid.append(f"{rec.get('engine_ref')} {rec.get('engine')} failed checks: {failed}")

samples = defaultdict(list)
provenance = {}
for rec in records:
    ref = rec["engine_ref"]
    engine = rec["engine"]
    provenance[ref] = {
        "engine_hash": rec["engine_hash"],
        "storage_by_engine": {
            **provenance.get(ref, {}).get("storage_by_engine", {}),
            engine: rec["storage"],
        },
    }
    for workload in rec["workloads"]:
        samples[(ref, engine, workload["name"])].append(workload["mean_ns_per_op"])

summary = []
for (ref, engine, workload), values in sorted(samples.items()):
    values = sorted(values)
    median = statistics.median(values)
    spread = values[-1] - values[0] if values else 0.0
    summary.append({
        "engine_ref": ref,
        "engine": engine,
        "workload": workload,
        "runs": len(values),
        "median_ns_per_op": median,
        "min_ns_per_op": values[0],
        "max_ns_per_op": values[-1],
        "spread_ns_per_op": spread,
        "raw_mean_ns_per_op": values,
    })

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
for workload in ["protected_scan_filter_count", "protected_aggregate_sum"]:
    base = median_for(baseline_ref, workload)
    cand = median_for(candidate_ref, workload)
    if base and cand is not None:
        regression = (cand - base) / base
        if regression > 0.10:
            protected_regressions.append({
                "workload": workload,
                "baseline_median_ns_per_op": base,
                "candidate_median_ns_per_op": cand,
                "regression": regression,
            })

write_pass = any(item["improvement"] >= 0.25 for item in write_improvements)
point_pass = point_improvement is not None and point_improvement >= 0.30
checks_pass = not invalid
publishable = mode == "full" and checks_pass and (write_pass or point_pass) and not protected_regressions
status = "pass" if publishable else "not_publishable"

result = {
    "schema": "powdb.compare.paired.aggregate.v1",
    "mode": mode,
    "runs": runs,
    "profile": "value-v1",
    "baseline_ref": baseline_ref,
    "candidate_ref": candidate_ref,
    "provenance": provenance,
    "valid": checks_pass,
    "invalid_reasons": invalid,
    "evaluation": {
        "status": status,
        "publishable": publishable,
        "criteria": {
            "candidate_write_improvement_required": 0.25,
            "candidate_point_read_improvement_required": 0.30,
            "protected_regression_limit": 0.10,
        },
        "write_pass": write_pass,
        "point_read_pass": point_pass,
        "point_read_improvement": point_improvement,
        "write_improvements": write_improvements,
        "protected_regressions": protected_regressions,
        "note": "Full mode is publishable; off mode is a labeled diagnostic and is never publishable.",
    },
    "summary": summary,
    "raw_records": records,
}
json.dump(result, sys.stdout, indent=2, sort_keys=True)
sys.stdout.write("\n")
if not checks_pass:
    sys.exit(1)
PY
