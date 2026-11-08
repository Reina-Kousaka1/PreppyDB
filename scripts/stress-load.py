#!/usr/bin/env python3
"""Prepared bounded HTTP load generator. Deliberately locked until isolation is qualified."""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import math
import os
import pathlib
import random
import re
import resource
import signal
import shutil
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Any


# The workflow sets this only for the separately approved bounded-stress mode, and
# only after its isolation check has passed for the same exact commit.
QUALIFIED_COMMIT = os.environ.get("PREPPYDB_ISOLATION_QUALIFIED_COMMIT", "")
ENABLE_PHRASE = "I_APPROVE_BOUNDED_PREPPYDB_STRESS"
CLIENT_LEVELS = (1, 10, 25, 50, 100)
MEMORY_LIMIT_BYTES = 2 * 1024**3
MEMORY_STOP_BYTES = int(MEMORY_LIMIT_BYTES * 0.80)
DATA_LIMIT_BYTES = 512 * 1024**2
DISK_RESERVE_BYTES = 128 * 1024**2
P95_STOP_SECONDS = 1.0
ERROR_STOP_RATE = 0.01
MAX_DURATION_SECONDS = 60
MAX_TOTAL_RUNTIME_SECONDS = 10 * 60
MAX_DOCUMENTS = 2_000
MAX_PAYLOAD_BYTES = 4_096
PID_LIMIT = 512
TASKS_STOP_COUNT = int(PID_LIMIT * 0.80)


def cgroup_v2_dir() -> pathlib.Path:
    member = None
    for line in pathlib.Path("/proc/self/cgroup").read_text().splitlines():
        hierarchy, controllers, path = line.split(":", 2)
        if hierarchy == "0" and controllers == "":
            member = path
            break
    if member is None:
        raise RuntimeError("cgroup v2 membership unavailable")
    for line in pathlib.Path("/proc/self/mountinfo").read_text().splitlines():
        left, right = line.split(" - ", 1)
        if right.split()[0] != "cgroup2":
            continue
        fields = left.split()
        root = fields[3]
        mount = pathlib.Path(fields[4].replace("\\040", " "))
        if root == "/":
            relative = member.lstrip("/")
        elif member == root:
            relative = ""
        elif member.startswith(root.rstrip("/") + "/"):
            relative = member[len(root.rstrip("/")) :].lstrip("/")
        else:
            continue
        path = mount.joinpath(*relative.split("/")) if relative else mount
        if (path / "memory.current").is_file():
            return path
    raise RuntimeError("cannot resolve cgroup v2 control directory")


def assert_bounded_data_mount() -> None:
    path = pathlib.Path("/stress-data")
    mount = next(
        (fields for fields in (line.split() for line in pathlib.Path("/proc/mounts").read_text().splitlines()) if fields[1] == str(path)),
        None,
    )
    if mount is None or mount[2] != "tmpfs":
        raise RuntimeError("load requires /stress-data on its isolated tmpfs")
    match = re.search(r"(?:^|,)size=(\d+)([kKmMgG]?)", mount[3])
    if not match:
        raise RuntimeError("load requires an explicit temporary data size limit")
    unit = {"": 1, "k": 1024, "m": 1024**2, "g": 1024**3}[match.group(2).lower()]
    if int(match.group(1)) * unit > DATA_LIMIT_BYTES:
        raise RuntimeError("temporary data mount exceeds the 512 MiB safety cap")
    stat = os.statvfs(path)
    if stat.f_blocks * stat.f_frsize > DATA_LIMIT_BYTES:
        raise RuntimeError("temporary data filesystem reports a size above the safety cap")


def percentile(values: list[float], quantile: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, math.ceil(len(ordered) * quantile) - 1))
    return ordered[index]


def request(base: str, token: str, method: str, path: str, body: Any | None = None) -> tuple[int, bytes]:
    payload = None if body is None else json.dumps(body, separators=(",", ":")).encode()
    headers = {"Authorization": f"Bearer {token}", "Connection": "close"}
    if payload is not None:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(base + path, data=payload, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=3) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def wait_ready(process: subprocess.Popen[bytes], base: str) -> None:
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"server exited before readiness: {process.returncode}")
        try:
            with urllib.request.urlopen(base + "/healthz", timeout=1) as response:
                if response.status == 200:
                    return
        except (OSError, urllib.error.URLError):
            time.sleep(0.1)
    raise TimeoutError("server did not become healthy within 30 seconds")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", type=pathlib.Path, required=True)
    parser.add_argument("--clients", type=int, choices=CLIENT_LEVELS, required=True)
    parser.add_argument("--mode", choices=("read", "write", "mixed", "index"), required=True)
    parser.add_argument("--documents", type=int, default=500)
    parser.add_argument("--payload-bytes", type=int, default=512)
    parser.add_argument("--duration-seconds", type=int, default=15)
    parser.add_argument("--seed", type=int, default=20261108)
    parser.add_argument("--data-dir", type=pathlib.Path, default=pathlib.Path("/stress-data/database"))
    args = parser.parse_args()

    qualified = os.environ.get("PREPPYDB_ISOLATION_QUALIFIED_COMMIT", "")
    source_commit = os.environ.get("PREPPYDB_COMMIT", "")
    approval = os.environ.get("PREPPYDB_ENABLE_STRESS", "")
    if (
        not QUALIFIED_COMMIT
        or source_commit != QUALIFIED_COMMIT
        or qualified != QUALIFIED_COMMIT
        or approval != ENABLE_PHRASE
    ):
        parser.error(
            "load is disabled; qualify isolation, review the report, and explicitly unlock "
            "this generator before running any load"
        )
    if not 1 <= args.documents <= MAX_DOCUMENTS:
        parser.error(f"--documents must be between 1 and {MAX_DOCUMENTS}")
    if not 128 <= args.payload_bytes <= MAX_PAYLOAD_BYTES:
        parser.error(f"--payload-bytes must be between 128 and {MAX_PAYLOAD_BYTES}")
    if not 5 <= args.duration_seconds <= MAX_DURATION_SECONDS:
        parser.error(f"--duration-seconds must be between 5 and {MAX_DURATION_SECONDS}")
    if not pathlib.Path("/sys/fs/cgroup/cgroup.controllers").is_file():
        parser.error("load generator requires qualified cgroup v2 isolation")
    if os.geteuid() == 0:
        parser.error("load generator must run as an unprivileged container user")
    if set(os.listdir("/sys/class/net")) != {"lo"}:
        parser.error("load generator requires a network-isolated container with loopback only")
    soft_nofile, hard_nofile = resource.getrlimit(resource.RLIMIT_NOFILE)
    if soft_nofile > 512 or hard_nofile > 512:
        parser.error("effective file descriptor limit exceeds 512")

    group = cgroup_v2_dir()
    if int((group / "memory.max").read_text().strip()) != MEMORY_LIMIT_BYTES:
        parser.error("effective memory limit differs from the qualified 2 GiB limit")
    if not (group / "memory.swap.max").is_file() or int((group / "memory.swap.max").read_text().strip()) != 0:
        parser.error("effective container swap limit is not zero")
    cpu_quota, cpu_period = (int(value) for value in (group / "cpu.max").read_text().split())
    if not 1.9 <= cpu_quota / cpu_period <= 2.0:
        parser.error("effective CPU quota differs from the qualified 2-core limit")
    if int((group / "pids.max").read_text().strip()) != PID_LIMIT:
        parser.error("effective process limit differs from the qualified 512-task limit")
    try:
        assert_bounded_data_mount()
    except (OSError, RuntimeError) as error:
        parser.error(str(error))

    server = args.server.resolve()
    if not server.is_file() or not os.access(server, os.X_OK):
        parser.error("--server must be an executable release server binary")
    total_deadline = time.monotonic() + MAX_TOTAL_RUNTIME_SECONDS
    data_root = pathlib.Path("/stress-data").resolve()
    data_dir = args.data_dir.resolve()
    if data_root not in data_dir.parents:
        parser.error("--data-dir must be a new child directory of the bounded /stress-data tmpfs")
    data_dir.mkdir(parents=True, exist_ok=False)
    init = subprocess.run(
        [str(server), "--data-dir", str(data_dir), "--bootstrap-token", "synthetic-stress-only", "--init-only"],
        capture_output=True,
        text=True,
        timeout=10,
        check=False,
    )
    if init.returncode != 0:
        parser.error("synthetic test database initialization failed")
    tokens = [line.strip() for line in init.stdout.splitlines() if re.fullmatch(r"[A-Za-z0-9_-]{40,}", line.strip())]
    if len(tokens) != 1:
        parser.error("server initialization did not yield exactly one bootstrap token")
    token = tokens[0]
    base = "http://127.0.0.1:17077"
    server_process = subprocess.Popen(
        [str(server), "--data-dir", str(data_dir), "--listen", "127.0.0.1:17077", "--profile", "tiny"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    try:
        wait_ready(server_process, base)
        status, _ = request(base, token, "POST", "/v1/collections", {"name": "stress", "max_docs": args.documents})
        if status not in (200, 201):
            raise RuntimeError(f"could not create synthetic collection (HTTP {status})")

        rng = random.Random(args.seed)
        alphabet = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
        payload = "".join(rng.choice(alphabet) for _ in range(args.payload_bytes))
        for offset in range(0, args.documents, 25):
            if time.monotonic() >= total_deadline:
                raise TimeoutError("load generator exceeded its 10-minute total runtime limit")
            operations = [
                {
                    "op": "put",
                    "key": f"doc-{index:06d}",
                    "expected_version": 0,
                    "value": {"group": index % 16, "payload": payload},
                }
                for index in range(offset, min(offset + 25, args.documents))
            ]
            status, _ = request(base, token, "POST", "/v1/collections/stress/batch", {"operations": operations})
            if status not in (200, 201):
                raise RuntimeError(f"synthetic seed batch failed (HTTP {status})")
        status, _ = request(
            base, token, "POST", "/v1/collections/stress/indexes", {"name": "by_group", "field": "group", "unique": False}
        )
        if status not in (200, 201):
            raise RuntimeError(f"synthetic index creation failed (HTTP {status})")

        stop = threading.Event()
        latencies: list[float] = []
        errors: list[str] = []
        counts = {"read": 0, "write": 0, "index": 0}
        lock = threading.Lock()
        last_progress = time.monotonic()
        stop_reason: str | None = None

        def worker(worker_id: int) -> None:
            nonlocal last_progress
            local_rng = random.Random(args.seed + worker_id)
            while not stop.is_set():
                document = local_rng.randrange(args.documents)
                kind = args.mode
                if kind == "mixed":
                    choice = local_rng.random()
                    kind = "read" if choice < 0.5 else "write" if choice < 0.9 else "index"
                key = f"doc-{document:06d}"
                if kind == "read":
                    method, path, body = "GET", f"/v1/collections/stress/documents/{key}", None
                elif kind == "write":
                    method, path, body = "PUT", f"/v1/collections/stress/documents/{key}", {
                        "value": {"group": document % 16, "payload": payload}
                    }
                else:
                    group_value = local_rng.randrange(16)
                    query = urllib.parse.urlencode({"value": json.dumps(group_value)})
                    method, path, body = "GET", f"/v1/collections/stress/indexes/by_group?{query}", None
                started = time.perf_counter()
                error_message: str | None = None
                try:
                    status, response = request(base, token, method, path, body)
                    ok = 200 <= status < 300
                    if ok:
                        try:
                            ok = json.loads(response).get("ok") is True
                        except (UnicodeDecodeError, json.JSONDecodeError, AttributeError):
                            ok = False
                    if not ok:
                        error_message = f"{method} {path.split('?')[0]} returned HTTP {status}"
                    else:
                        error_message = None
                except Exception as error:
                    error_message = type(error).__name__
                elapsed = time.perf_counter() - started
                with lock:
                    if error_message is not None:
                        errors.append(error_message)
                    else:
                        counts[kind] += 1
                    latencies.append(elapsed)
                    last_progress = time.monotonic()

        started = time.monotonic()
        cpu_stats_start = read_kv(group / "cpu.stat")
        cpu_start = int(cpu_stats_start.get("usage_usec", 0))
        cpu_throttled_start = int(cpu_stats_start.get("nr_throttled", 0))
        memory_peak_path = group / "memory.peak"
        peak_memory = int(memory_peak_path.read_text().strip()) if memory_peak_path.is_file() else int((group / "memory.current").read_text().strip())
        peak_tasks = int((group / "pids.current").read_text().strip())
        tasks_peak_path = group / "pids.peak"
        if tasks_peak_path.is_file():
            peak_tasks = max(peak_tasks, int(tasks_peak_path.read_text().strip()))
        if peak_memory >= MEMORY_STOP_BYTES:
            stop_reason = "memory reached 80 percent of cgroup limit before workload"
            stop.set()
        samples: list[dict[str, float | int]] = []
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.clients) as pool:
            futures = [pool.submit(worker, index) for index in range(args.clients)]
            deadline = min(started + args.duration_seconds, total_deadline)
            cpu_hot_since: float | None = None
            previous_sample = started
            previous_cpu = cpu_start
            while time.monotonic() < deadline and not stop.is_set():
                time.sleep(0.2)
                now = time.monotonic()
                current_memory = int((group / "memory.current").read_text().strip())
                peak_memory = max(peak_memory, current_memory)
                if memory_peak_path.is_file():
                    peak_memory = max(peak_memory, int(memory_peak_path.read_text().strip()))
                peak_tasks = max(peak_tasks, int((group / "pids.current").read_text().strip()))
                if tasks_peak_path.is_file():
                    peak_tasks = max(peak_tasks, int(tasks_peak_path.read_text().strip()))
                cpu_now = int(read_kv(group / "cpu.stat").get("usage_usec", 0))
                cpu_pct = 100 * (cpu_now - previous_cpu) / max(1, (now - previous_sample) * 1_000_000 * CPU_LIMIT)
                samples.append({
                    "elapsed_seconds": now - started,
                    "memory_bytes": current_memory,
                    "cpu_percent_of_limit": cpu_pct,
                    "tasks": int((group / "pids.current").read_text().strip()),
                })
                previous_sample, previous_cpu = now, cpu_now
                if peak_memory >= MEMORY_STOP_BYTES:
                    stop_reason = "memory reached 80 percent of cgroup limit"
                elif peak_tasks >= TASKS_STOP_COUNT:
                    stop_reason = "task count reached 80 percent of cgroup pids limit"
                elif shutil.disk_usage("/stress-data").free < DISK_RESERVE_BYTES:
                    stop_reason = "temporary data free space fell below 128 MiB"
                elif latencies and (percentile(latencies[-100:], 0.95) or 0) > P95_STOP_SECONDS:
                    stop_reason = "p95 latency exceeded 1 second"
                elif len(latencies) > 0 and len(errors) / len(latencies) > ERROR_STOP_RATE:
                    stop_reason = "error rate exceeded 1 percent"
                elif now - last_progress > 10:
                    stop_reason = "no request completed for 10 seconds"
                if cpu_pct >= 95:
                    cpu_hot_since = cpu_hot_since or now
                else:
                    cpu_hot_since = None
                if cpu_hot_since is not None and now - cpu_hot_since >= 10:
                    stop_reason = "CPU remained above 95 percent of quota for 10 seconds"
                if stop_reason:
                    stop.set()
            stop.set()
            for future in futures:
                future.result(timeout=5)

        elapsed = time.monotonic() - started
        integrity_before_recovery = verify_documents(base, token, args.documents, payload)
        stop_server(server_process)
        server_process = None
        server_process = subprocess.Popen(
            [str(server), "--data-dir", str(data_dir), "--listen", "127.0.0.1:17077", "--profile", "tiny"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        wait_ready(server_process, base)
        integrity_after_recovery = verify_documents(base, token, args.documents, payload)
        cpu_final = read_kv(group / "cpu.stat")
        if memory_peak_path.is_file():
            peak_memory = max(peak_memory, int(memory_peak_path.read_text().strip()))
        if tasks_peak_path.is_file():
            peak_tasks = max(peak_tasks, int(tasks_peak_path.read_text().strip()))
        total = sum(counts.values()) + len(errors)
        report = {
            "status": "aborted_at_threshold" if stop_reason else "completed",
            "abort_reason": stop_reason,
            "clients": args.clients,
            "mode": args.mode,
            "documents": args.documents,
            "payload_bytes": args.payload_bytes,
            "seed": args.seed,
            "elapsed_seconds": round(elapsed, 3),
            "requests": {**counts, "errors": len(errors), "total": total},
            "seeded_documents": args.documents,
            "integrity_verification_reads": integrity_before_recovery["verified_documents"]
            + integrity_after_recovery["verified_documents"],
            "integrity_before_recovery": integrity_before_recovery,
            "integrity_after_recovery": integrity_after_recovery,
            "confirmed_write_operations": counts["write"],
            "requests_per_second": round(total / max(elapsed, 0.001), 2),
            "error_rate": round(len(errors) / max(total, 1), 5),
            "latency_ms": {
                "p50": None if not latencies else round(percentile(latencies, 0.50) * 1000, 3),
                "p95": None if not latencies else round(percentile(latencies, 0.95) * 1000, 3),
                "p99": None if not latencies else round(percentile(latencies, 0.99) * 1000, 3),
            },
            "resources": {
                "peak_cgroup_memory_mb": round(peak_memory / 1024**2, 2),
                "peak_tasks": peak_tasks,
                "cpu_throttled_periods_delta": cpu_final.get("nr_throttled", 0) - cpu_throttled_start,
                "cpu_percent_of_two_core_limit_samples": samples,
                "data_used_mb": round(shutil.disk_usage("/stress-data").used / 1024**2, 2),
            },
            "sample_errors": errors[:5],
        }
        print(json.dumps(report, sort_keys=True))
        return 0 if stop_reason is None and integrity_after_recovery["passed"] else 2
    finally:
        if server_process is not None and server_process.poll() is None:
            stop_server(server_process)


def stop_server(process: subprocess.Popen[bytes] | None) -> None:
    if process is None or process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=10)
    except (ProcessLookupError, subprocess.TimeoutExpired):
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=2)


def verify_documents(base: str, token: str, documents: int, payload: str) -> dict[str, Any]:
    for index in range(documents):
        key = f"doc-{index:06d}"
        status, raw = request(base, token, "GET", f"/v1/collections/stress/documents/{key}")
        if status != 200:
            raise RuntimeError(f"integrity verification failed for {key}: HTTP {status}")
        try:
            data = json.loads(raw).get("data", {})
            value = data.get("value", {})
        except (UnicodeDecodeError, json.JSONDecodeError, AttributeError):
            raise RuntimeError(f"integrity verification returned invalid JSON for {key}") from None
        if data.get("key") != key or value.get("group") != index % 16 or value.get("payload") != payload:
            raise RuntimeError(f"integrity verification found mismatched content for {key}")
    return {"verified_documents": documents, "passed": True}


def read_kv(path: pathlib.Path) -> dict[str, int]:
    values: dict[str, int] = {}
    for line in path.read_text().splitlines():
        key, value = line.split()
        values[key] = int(value)
    return values


if __name__ == "__main__":
    raise SystemExit(main())
