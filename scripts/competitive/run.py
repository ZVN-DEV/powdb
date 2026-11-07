#!/usr/bin/env python3
"""Collect matched query results, timings and sampled process-tree PSS.

This collector never connects to an existing server. Optional servers run in
owned containers with networking disabled and private Unix sockets. Fixture
creation happens in a worker that exits before the measured worker starts.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import queue
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid

from cases import CASES, PROFILE, id_for, parameters, query_for


def process_tree(roots):
    pending = list(roots)
    found = set()
    while pending:
        pid = pending.pop()
        if pid in found:
            continue
        found.add(pid)
        try:
            children = Path(f"/proc/{pid}/task/{pid}/children").read_text()
            pending.extend(map(int, children.split()))
        except (OSError, ValueError):
            pass
    return found


def memory_snapshot(roots):
    pids = process_tree(roots)
    pss = rss = 0
    errors = []
    for pid in pids:
        try:
            fields = {}
            for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines():
                if ":" in line:
                    key, value = line.split(":", 1)
                    if key in ("Pss", "Rss"):
                        fields[key] = int(value.split()[0])
            pss += fields["Pss"]
            rss += fields["Rss"]
        except (OSError, ValueError, KeyError) as error:
            errors.append(f"{pid}: {error}")
    return dict(pss_kib=pss if not errors else None,
                summed_rss_kib=rss if not errors else None,
                pids=sorted(pids), errors=errors)


class MemorySampler:
    def __init__(self, roots, interval=0.01):
        self.roots = roots
        self.interval = interval
        self.stop = threading.Event()
        self.samples = []
        self.thread = threading.Thread(target=self.collect, daemon=True)

    def collect(self):
        while not self.stop.is_set():
            self.samples.append(memory_snapshot(self.roots))
            self.stop.wait(self.interval)

    def __enter__(self):
        self.started = time.monotonic()
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.stop.set()
        self.thread.join()
        self.samples.append(memory_snapshot(self.roots))

    def result(self):
        errors = [e for sample in self.samples for e in sample["errors"]]
        valid = [s for s in self.samples if s["pss_kib"] is not None]
        return dict(method="sampled-process-tree-pss", interval_s=self.interval,
                    samples=len(self.samples), duration_s=time.monotonic()-self.started,
                    peak_pss_kib=max((s["pss_kib"] for s in valid), default=None) if not errors else None,
                    summed_peak_rss_kib=max((s["summed_rss_kib"] for s in valid), default=None) if not errors else None,
                    errors=errors,
                    limits="Sampling can miss short peaks; PSS excludes unmapped filesystem cache. RSS sums double-count shared pages.")


class Worker:
    def __init__(self, binary, engine, directory, rows, phase, cpu, timeout):
        self.responses = queue.Queue()
        self.timeout = timeout
        command = [str(binary), "--engine", engine, "--data-dir", str(directory),
                   "--rows", str(rows), "--phase", phase]
        if cpu is not None:
            command = ["taskset", "-c", str(cpu), *command]
        self.log = open(directory / f"{phase}.stderr.log", "w+")
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=self.log, text=True, start_new_session=True)
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()
        try:
            self.ready = self.receive()
            if not self.ready.get("ready") or self.ready.get("protocol_schema") != 1:
                raise RuntimeError(f"worker not ready: {self.ready}")
        except BaseException:
            self.close(kill=True)
            raise

    def read(self):
        for line in self.process.stdout:
            try:
                self.responses.put(json.loads(line))
            except ValueError as error:
                self.responses.put({"worker_protocol_error": str(error)})
        self.responses.put({"worker_eof": True})

    def receive(self):
        try:
            response = self.responses.get(timeout=self.timeout)
        except queue.Empty as error:
            self.close(kill=True)
            raise TimeoutError("worker request exceeded deadline") from error
        if "worker_eof" in response:
            self.log.flush()
            self.log.seek(0)
            detail = self.log.read()[-8000:]
            raise RuntimeError(f"worker exited: {detail}")
        if "worker_protocol_error" in response:
            raise RuntimeError(str(response))
        return response

    def request(self, queries, delivery, bound_ids=None):
        self.process.stdin.write(json.dumps(dict(queries=queries, delivery=delivery,bound_ids=bound_ids))+"\n")
        self.process.stdin.flush()
        return self.receive()

    def close(self, kill=False):
        if self.process.poll() is None:
            if kill:
                os.killpg(self.process.pid, signal.SIGKILL)
            else:
                self.process.stdin.close()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait()
        if not self.log.closed:
            self.log.close()


class Server:
    def __init__(self):
        self.container = None
        self.pid = None

    def start(self, engine, directory, cpu, image):
        if engine not in ("postgres", "mysql", "powdb-server", "powdb-server-sql"):
            return
        if not image or "@sha256:" not in image:
            raise RuntimeError(f"{engine} requires a digest-pinned owned container image")
        socket_dir = directory / ("pg-socket" if engine == "postgres" else "mysql-socket" if engine == "mysql" else "powdb-socket")
        data_dir = directory / "server-data"
        socket_dir.mkdir()
        data_dir.mkdir()
        uid, gid = os.getuid(), os.getgid()
        self.container = "powdb-competitive-"+uuid.uuid4().hex
        command = ["docker", "run", "--name", self.container, "--detach", "--no-healthcheck", "--network", "none", "--memory", "2g",
                   "--user", f"{uid}:{gid}"]
        if cpu is not None:
            command += ["--cpuset-cpus", str(cpu)]
        command += ["--mount", f"type=bind,src={socket_dir},dst=/bench-socket"]
        if engine == "postgres":
            command += ["--mount", f"type=bind,src={data_dir},dst=/pgdata",
                        "--env", "PGDATA=/pgdata", "--env", "POSTGRES_USER=bench",
                        "--env", "POSTGRES_HOST_AUTH_METHOD=trust", image,
                        "postgres", "-c", "listen_addresses=", "-c",
                        "unix_socket_directories=/bench-socket"]
            socket_file = socket_dir / ".s.PGSQL.5432"
        elif engine == "mysql":
            command += ["--mount", f"type=bind,src={data_dir},dst=/var/lib/mysql",
                        "--env", "MYSQL_ALLOW_EMPTY_PASSWORD=yes", "--env", "MYSQL_DATABASE=bench",
                        image, "mysqld", "--skip-networking", "--socket=/bench-socket/mysql.sock",
                        "--innodb-flush-log-at-trx-commit=1", "--sync-binlog=1"]
            socket_file = socket_dir / "mysql.sock"
        else:
            command += ["--mount", f"type=bind,src={data_dir},dst=/bench-data", image,
                        "/usr/local/bin/powdb-server", "--port", "5433", "--data-dir", "/bench-data",
                        "--socket", "/bench-socket/powdb.sock"]
            socket_file = socket_dir / "powdb.sock"
        subprocess.check_output(command, text=True)
        deadline = time.monotonic()+120
        while time.monotonic() < deadline:
            state = json.loads(subprocess.check_output(
                ["docker", "inspect", self.container, "--format", "{{json .State}}"], text=True))
            if not state["Running"]:
                log = subprocess.check_output(["docker", "logs", self.container], stderr=subprocess.STDOUT, text=True)
                raise RuntimeError(f"owned {engine} container stopped: {log[-8000:]}")
            if socket_file.exists():
                # Entrypoints briefly start a bootstrap server. Wait until their
                # real command replaces the bootstrap shell before connecting.
                command_name = Path(f"/proc/{state['Pid']}/comm").read_text().strip()
                if command_name == ("postgres" if engine == "postgres" else "mysqld" if engine == "mysql" else "tini"):
                    self.pid = state["Pid"]
                    return
            time.sleep(0.2)
        raise TimeoutError(f"owned {engine} server did not become ready")

    def close(self):
        if self.container:
            subprocess.run(["docker", "rm", "--force", self.container], check=False,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def backend_name(engine):
    return engine.removesuffix("-aa")


def binary_name(engine):
    engine = backend_name(engine)
    if engine.startswith("powdb-server"):
        return "powdb-server"
    return "powdb" if engine.startswith("powdb-") else engine


def environment(binary_dir, engines):
    cpuinfo = Path("/proc/cpuinfo").read_text() if Path("/proc/cpuinfo").exists() else ""
    model = next((x.split(":", 1)[1].strip() for x in cpuinfo.splitlines() if x.startswith("model name")), "unknown")
    binaries = {}
    for engine in engines:
        key = binary_name(engine)
        binary = binary_dir / key
        binaries[engine] = dict(path=str(binary), sha256=hashlib.sha256(binary.read_bytes()).hexdigest()) if binary.is_file() else None
    return dict(platform=platform.platform(), machine=platform.machine(), cpu_model=model,
                affinity=sorted(os.sched_getaffinity(0)), logical_cpus=os.cpu_count(),
                meminfo=Path("/proc/meminfo").read_text(), initial_load=os.getloadavg(),
                github_run_id=os.environ.get("GITHUB_RUN_ID"),
                github_repository=os.environ.get("GITHUB_REPOSITORY"),
                git_sha=subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
                git_dirty=bool(subprocess.check_output(["git", "status", "--porcelain"], text=True).strip()),
                rustflags=os.environ.get("RUSTFLAGS", ""),
                build_manifest=json.loads((binary_dir.parent / "build-manifest.json").read_text()),
                storage=subprocess.check_output(["findmnt", "-T", str(binary_dir), "-J", "-o", "FSTYPE,OPTIONS,TARGET"],text=True).strip(),
                binaries=binaries)


def run(args):
    args.output.parent.mkdir(parents=True, exist_ok=True)
    engines = args.engines.split(",")
    if len(engines) != len(set(engines)) or "sqlite" not in engines:
        raise ValueError("unique engines including the SQLite oracle are required")
    result = dict(schema=1, profile=PROFILE, rows=args.rows, rounds=args.rounds,
                  cases_sha256=hashlib.sha256(json.dumps(CASES,sort_keys=True).encode()).hexdigest(),
                  durability="file-backed durable engine settings; see observed metadata",
                  cache="one explicit warmup per workload; OS cache is not dropped or certified cold",
                  timing="Rust client API including typed row conversion; ad-hoc parsing included; bound prepare excluded; setup excluded",
                  source_cases=CASES, environment=environment(args.worker_dir, engines),
                  isolated_host_assertion=args.isolated_host, records=[], failures=[])
    selected = [c for c in CASES if not args.cases or c["id"] in args.cases.split(",")]
    if not selected:
        raise ValueError("no workloads selected")
    result["selected_cases"] = [c["id"] for c in selected]
    for round_number in range(args.rounds):
        params = parameters(args.rows, round_number)
        order = engines[round_number % len(engines):]+engines[:round_number % len(engines)]
        for engine in order:
            backend = backend_name(engine)
            root = Path(tempfile.mkdtemp(prefix=f"{engine}-{round_number}-", dir=args.temp_dir))
            (root / ".competitive-disposable").write_text(PROFILE)
            server = worker = None
            failed = False
            try:
                server = Server()
                image = args.postgres_image if backend == "postgres" else args.mysql_image if backend == "mysql" else args.powdb_image
                server.start(backend, root, args.cpu, image)
                binary = args.worker_dir / binary_name(engine)
                setup = Worker(binary, backend, root, args.rows, "prepare", args.cpu, args.timeout)
                setup.close()
                # The loader process has exited. Allocations used to format and
                # insert fixtures cannot contaminate this process's memory peak.
                worker = Worker(binary, backend, root, args.rows, "query", args.cpu, args.timeout)
                workload_order = selected[round_number % len(selected):]+selected[:round_number % len(selected)]
                for workload in workload_order:
                    for delivery in workload["deliveries"]:
                        if workload["category"] == "delivery":
                            # A fresh client/embedded engine prevents a prior
                            # materialized result's retained heap from being
                            # attributed to bounded consumption. Owned servers
                            # keep their normal resident buffer/cache state.
                            worker.close()
                            worker = Worker(binary, backend, root, args.rows, "query", args.cpu, args.timeout)
                        count = min(workload["iterations"], args.max_iterations)
                        queries = [query_for(workload, backend, i, params) for i in range(count)]
                        ids = [id_for(i,params) for i in range(count)] if workload.get("prepared") else None
                        warmup = worker.request(queries[:1], delivery, ids[:1] if ids else None)
                        roots = [worker.process.pid]+([server.pid] if server.pid else [])
                        idle = memory_snapshot(roots)
                        with MemorySampler(roots) as sampler:
                            response = worker.request(queries, delivery, ids) if "error" not in warmup else warmup
                        record = dict(engine=engine, round=round_number, case=workload["id"],
                                      category=workload["category"], delivery=delivery,
                                      queries=queries, metadata=worker.ready["metadata"],
                                      idle_memory=idle, memory=sampler.result(), **response)
                        result["records"].append(record)
                        args.output.write_text(json.dumps(result, indent=2)+"\n")
                        print(json.dumps(dict(engine=engine, round=round_number, case=workload["id"],
                                              delivery=delivery, status="error" if "error" in response else "collected")), flush=True)
            except (OSError, ValueError, RuntimeError, TimeoutError, subprocess.SubprocessError) as error:
                failed = True
                result["failures"].append(dict(engine=engine, round=round_number, error=str(error)))
            finally:
                if worker:
                    worker.close(kill=failed)
                if server:
                    server.close()
                shutil.rmtree(root)
                args.output.write_text(json.dumps(result, indent=2)+"\n")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worker-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--temp-dir", type=Path, required=True)
    parser.add_argument("--engines", default="sqlite,sqlite-aa,powdb-powql,powdb-sql,duckdb")
    parser.add_argument("--rows", type=int, default=10000)
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--max-iterations", type=int, default=1000)
    parser.add_argument("--timeout", type=float, default=90)
    parser.add_argument("--cpu", type=int)
    parser.add_argument("--cases")
    parser.add_argument("--postgres-image")
    parser.add_argument("--mysql-image")
    parser.add_argument("--powdb-image")
    parser.add_argument("--isolated-host", action="store_true")
    args = parser.parse_args()
    if not (1000 <= args.rows <= 1000000 and 1 <= args.rounds <= 20 and 1 <= args.max_iterations <= 10000):
        parser.error("invalid fixture size, rounds or iteration bound")
    args.worker_dir = args.worker_dir.resolve()
    args.temp_dir = args.temp_dir.resolve()
    args.temp_dir.mkdir(parents=True, exist_ok=True)
    value = run(args)
    return 1 if value["failures"] else 0


if __name__ == "__main__":
    sys.exit(main())
