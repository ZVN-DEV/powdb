#!/usr/bin/env python3
"""Validate observations before reporting profile-specific relative scores.

Unsupported/rejected cases remain visible. Wrong results, missing observations,
insufficient rounds, unstable measurements and failed identical-binary controls
cannot become successful timing scores. Unknown memory is never zero memory.
"""

import argparse
import hashlib
import json
import math
from pathlib import Path
import statistics
import re

from cases import CASES, PROFILE, id_for, parameters


COHORTS = {
    "embedded_native": ["sqlite", "duckdb", "powdb-powql"],
    "embedded_sql": ["sqlite", "duckdb", "powdb-sql"],
    "server_native": ["postgres", "mysql", "powdb-server"],
    "server_sql": ["postgres", "mysql", "powdb-server-sql"],
}
MIN_ROUNDS = 5
SPREAD_LIMIT = 0.20
AA_LATENCY_LIMIT = 0.10


def finite_positive(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value) and value > 0


def geometric_mean(values):
    return math.exp(statistics.mean(math.log(v) for v in values))


def quantile(values, fraction):
    # Actual per-request observations only. Never compute a tail from five
    # round medians or from a sample count too small for a useful tail estimate.
    if len(values) < 1000:
        return None
    return sorted(values)[math.ceil(len(values)*fraction)-1]


def oracle_invariants(data, records):
    """Independent seed checks keep a shared loader/query bug from self-validating."""
    rows = data["rows"]
    cycles, remainder = divmod(rows, 1000)
    total = -500*cycles + remainder*(remainder-1)//2 - 500*remainder
    scalar = {"sum": total, "avg": total/rows,
              "null_count": (rows+6)//7,
              "filtered_count": cycles*25+sum((i*37)%1000 >= 975 for i in range(remainder))}
    for (engine, name, delivery, round_number), record in records.items():
        if engine != "sqlite" or "error" in record:
            continue
        params = parameters(rows, round_number)
        for index, sample in enumerate(record.get("samples", [])):
            expected = None
            expected_count = None
            if name in ("point_indexed", "point_bound"):
                value = id_for(index, params)
                expected = [[value, f"user_{value}", value % 1000-500]]
                expected_count = 1
            elif name == "point_missing":
                expected, expected_count = [], 0
            elif name == "int64_exact":
                expected, expected_count = [[9007199254740993]], 1
            elif name in scalar:
                expected, expected_count = [[scalar[name]]], 1
            elif name == "distinct":
                expected, expected_count = [[0], [1], [2]], 64
            elif name == "all_rows_narrow":
                expected, expected_count = [[i, i-500] for i in range(3)], rows
            elif name == "all_rows_wide":
                expected, expected_count = [[i, f"user_{i}", "x"*128] for i in range(3)], rows
            if expected is not None:
                valid = sample.get("preview") == expected and sample.get("rows") == expected_count
                if name == "avg":
                    actual = (sample.get("preview") or [[]])[0]
                    valid = sample.get("rows") == 1 and len(actual) == 1 and math.isclose(actual[0], scalar[name],rel_tol=1e-12,abs_tol=1e-12)
                if not valid:
                    raise ValueError(f"SQLite oracle fails independent fixture invariant: {name}, round {round_number}, sample {index}")


def equivalent(sample, expected, case):
    if (sample.get("digest"), sample.get("rows")) == (expected.get("digest"), expected.get("rows")):
        return True
    if case["id"] == "avg" and sample.get("rows") == expected.get("rows") == 1:
        # This scalar's complete result is retained. Avoid classifying normal
        # decimal-to-binary rounding differences as query correctness defects.
        try:
            a, b = sample["preview"][0][0], expected["preview"][0][0]
            return type(a) in (int,float) and type(b) in (int,float) and math.isclose(a,b,rel_tol=1e-12,abs_tol=1e-12)
        except (IndexError, KeyError, TypeError):
            return False
    return False


def score(data):
    if data.get("schema") != 1 or data.get("profile") != PROFILE:
        raise ValueError("unsupported result schema/profile")
    expected_cases = hashlib.sha256(json.dumps(CASES,sort_keys=True).encode()).hexdigest()
    if data.get("cases_sha256") != expected_cases:
        raise ValueError("result does not match the registered workload catalog")
    if type(data.get("rows")) is not int or not 1000 <= data["rows"] <= 1000000:
        raise ValueError("invalid row count")
    if type(data.get("rounds")) is not int or not 1 <= data["rounds"] <= 20:
        raise ValueError("invalid round count")
    reasons = []
    if not data["environment"].get("build_manifest", {}).get("artifacts_sha256"):
        reasons.append("missing SDK/library build provenance")
    if data.get("rounds", 0) < MIN_ROUNDS:
        reasons.append("fewer than five rounds")
    if not data.get("isolated_host_assertion"):
        reasons.append("host was not declared isolated")
    manifest = data["environment"].get("build_manifest", {})
    if manifest.get("source_sha") != data["environment"].get("git_sha"):
        reasons.append("worker build source differs from collector source")
    if data["environment"].get("git_dirty", True):
        reasons.append("source working tree was dirty")
    if set(data.get("selected_cases", [])) != {c["id"] for c in CASES}:
        reasons.append("partial workload catalog")
    if data.get("failures"):
        reasons.append("worker/server failures occurred")
    for binary in data["environment"]["binaries"].values():
        if not binary or not re.fullmatch(r"[0-9a-f]{64}", binary.get("sha256", "")):
            reasons.append("missing worker binary provenance")
    for engine, binary in data["environment"]["binaries"].items():
        name = engine.removesuffix("-aa")
        name = "powdb-server" if name.startswith("powdb-server") else "powdb" if name.startswith("powdb-") else name
        if not binary or manifest.get("artifacts_sha256", {}).get("bin/"+name) != binary.get("sha256"):
            reasons.append("worker binary does not match its build manifest")
    records = {}
    for record in data["records"]:
        key = (record["engine"], record["case"], record["delivery"], record["round"])
        if key in records:
            raise ValueError(f"duplicate observation: {key}")
        records[key] = record
    oracle_invariants(data, records)
    engines = sorted(data["environment"]["binaries"])
    cells = {}
    for engine in engines:
        for case in CASES:
            for delivery in case["deliveries"]:
                key = (engine, case["id"], delivery)
                observed = [records.get((*key, r)) for r in range(data["rounds"])]
                cell = dict(engine=engine, case=case["id"], category=case["category"],
                            delivery=delivery, status="correct", reasons=[])
                if any(r is None for r in observed):
                    cell.update(status="missing", reasons=["missing engine/case/delivery/round"])
                    cells[key] = cell
                    continue
                errors = [r["error"] for r in observed if "error" in r]
                if errors:
                    status = "incorrect" if any(e.startswith("incorrect:") for e in errors) else "adapter_error" if any(e.startswith("adapter:") for e in errors) else "rejected"
                    cell.update(status=status,
                                reasons=sorted(set(errors)))
                    cells[key] = cell
                    continue
                timings = []
                round_medians = []
                first_rows = []
                for round_number, record in enumerate(observed):
                    oracle = records.get(("sqlite", case["id"], delivery, round_number))
                    samples = record.get("samples", [])
                    if not samples or len(samples) != len(record["queries"]):
                        cell["status"] = "invalid"
                        cell["reasons"].append("missing or incomplete request samples")
                        continue
                    if len(samples) != case["iterations"]:
                        cell["reasons"].append("fewer requests than the registered workload")
                    if not oracle or "error" in oracle or len(oracle.get("samples", [])) != len(samples):
                        cell["status"] = "invalid"
                        cell["reasons"].append("missing matched oracle observations")
                        continue
                    for sample, expected in zip(samples, oracle["samples"]):
                        if not finite_positive(sample.get("elapsed_ns")):
                            cell["status"] = "invalid"
                            cell["reasons"].append("invalid latency")
                        if (not isinstance(sample.get("rows"),int) or isinstance(sample.get("rows"),bool) or sample["rows"] < 0
                            or not re.fullmatch(r"[0-9a-f]{64}",sample.get("digest", ""))):
                            cell["status"] = "invalid"
                            cell["reasons"].append("invalid row count or digest")
                        if not equivalent(sample,expected,case):
                            cell["status"] = "incorrect"
                            cell["reasons"].append("typed result digest/row count differs from SQLite oracle")
                    if cell["status"] in ("invalid", "incorrect"):
                        continue
                    values = [s["elapsed_ns"] for s in samples]
                    timings.extend(values)
                    round_medians.append(statistics.median(values))
                    first_rows.extend(s["first_row_ns"] for s in samples if s.get("first_row_ns") is not None)
                if cell["status"] != "correct":
                    cell["reasons"] = sorted(set(cell["reasons"]))
                    cells[key] = cell
                    continue
                median = statistics.median(round_medians)
                spread = (max(round_medians)-min(round_medians))/median
                peaks = [r["memory"].get("peak_pss_kib") for r in observed]
                memory_valid = all(p is not None and p > 0 and r["memory"].get("samples", 0) >= 3 and not r["memory"].get("errors")
                                   for p, r in zip(peaks, observed))
                cell.update(median_ns=median, round_medians_ns=round_medians,
                            relative_round_spread=spread, per_request_samples=len(timings),
                            p95_ns=quantile(timings, .95), p99_ns=quantile(timings, .99),
                            median_first_row_ns=statistics.median(first_rows) if first_rows else None,
                            sampled_peak_pss_kib=statistics.median(peaks) if memory_valid else None,
                            producer=observed[0].get("producer"))
                if spread > SPREAD_LIMIT:
                    cell["reasons"].append("round spread exceeds 20%")
                cells[key] = cell
    calibrations = {}
    for oracle in ("sqlite", "postgres"):
        aa = oracle+"-aa"
        same_binary = bool(data["environment"]["binaries"].get(oracle) and data["environment"]["binaries"].get(aa)) and (data["environment"]["binaries"].get(oracle) or {}).get("sha256") == (data["environment"]["binaries"].get(aa) or {}).get("sha256")
        for case in CASES:
            for delivery in case["deliveries"]:
                a, b = cells.get((oracle, case["id"], delivery)), cells.get((aa, case["id"], delivery))
                valid = bool(same_binary and a and b and a["status"] == b["status"] == "correct" and not a["reasons"] and not b["reasons"])
                delta = b["median_ns"]/a["median_ns"]-1 if valid else None
                calibrations[(oracle, case["id"], delivery)] = dict(pass_gate=valid and abs(delta) <= AA_LATENCY_LIMIT,
                                                                    relative_delta=delta,
                                                                    same_binary=same_binary)
    cohorts = {}
    for name, participants in COHORTS.items():
        if not any(e in engines for e in participants):
            continue
        available = [e for e in participants if e in engines]
        calibration_engine = "sqlite" if name.startswith("embedded") else "postgres"
        cohort_reasons = reasons.copy()
        if len(available) != len(participants):
            cohort_reasons.append("one or more registered cohort engines were not measured")
        correctness_failed = {e for e in available if any(c["engine"] == e and c["status"] == "incorrect" for c in cells.values())}
        query_coverage = {e:dict(correct=sum(c["status"] == "correct" for c in cells.values() if c["engine"] == e),
                               required=sum(len(c["deliveries"]) for c in CASES)) for e in available}
        common = []
        relative = {e: [] for e in available}
        for case in CASES:
            for delivery in case["deliveries"]:
                group = [cells[(e, case["id"], delivery)] for e in available]
                if any(c["status"] in ("missing","adapter_error","invalid") for c in group):
                    cohort_reasons.append("incomplete or invalid adapter observations")
                if any(c["status"] != "correct" for c in group):
                    continue
                if any(c["reasons"] for c in group):
                    cohort_reasons.append("a common workload failed request-count or stability gates")
                    continue
                calibration = calibrations[calibration_engine, case["id"], delivery]
                if not calibration["pass_gate"]:
                    cohort_reasons.append("a common workload failed identical-binary A/A calibration")
                    continue
                common.append(dict(case=case["id"], delivery=delivery))
                fastest = min(c["median_ns"] for c in group)
                for c in group:
                    relative[c["engine"]].append(fastest/c["median_ns"])
        scores = {e:(100*geometric_mean(values) if values and not cohort_reasons and e not in correctness_failed else None)
                  for e, values in relative.items()}
        memory_cases = []
        memory_reasons = reasons.copy()
        memory_relative = {e: [] for e in available}
        for case in CASES:
            if case["category"] != "delivery":
                continue
            for delivery in case["deliveries"]:
                group = [cells[(e, case["id"], delivery)] for e in available]
                a = cells.get((calibration_engine, case["id"], delivery), {})
                b = cells.get((calibration_engine+"-aa", case["id"], delivery), {})
                peaks = [c.get("sampled_peak_pss_kib") for c in group]
                aa_peaks = [c.get("sampled_peak_pss_kib") for c in (a,b)]
                if (len(available) != len(participants) or any(c["status"] != "correct" for c in group)
                    or not all(finite_positive(p) for p in peaks+aa_peaks)
                    or abs(aa_peaks[1]/aa_peaks[0]-1) > 0.20):
                    memory_reasons.append("missing/incorrect delivery observations or sampled PSS A/A differs by more than 20%")
                    continue
                memory_cases.append(dict(case=case["id"],delivery=delivery))
                smallest = min(peaks)
                for c,p in zip(group,peaks):
                    memory_relative[c["engine"]].append(smallest/p)
        memory_scores = {e:(100*geometric_mean(v) if v and not memory_reasons and e not in correctness_failed else None)
                         for e,v in memory_relative.items()}
        cohorts[name] = dict(sampled_delivery_pss_score=memory_scores, memory_cases=memory_cases,
                             memory_reasons=sorted(set(memory_reasons)),participants=participants, measured=available, common_cases=common,
                             query_coverage=query_coverage, common_query_latency_score=scores,
                             reasons=sorted(set(cohort_reasons)), correctness_failed=sorted(correctness_failed),
                             excluded_cases=[dict(case=c["id"],delivery=d) for c in CASES for d in c["deliveries"] if dict(case=c["id"],delivery=d) not in common],
                             interpretation="100 equals the fastest observed median in every included common case; no universal or cross-cohort ranking.")
    return dict(schema=1, profile=PROFILE, qualification_reasons=reasons,
                cells=list(cells.values()), cohorts=cohorts,
                calibration=[dict(engine=k[0], case=k[1], delivery=k[2], **v) for k, v in calibrations.items()])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.write_text(json.dumps(score(json.loads(args.input.read_text())), indent=2)+"\n")


if __name__ == "__main__":
    main()
