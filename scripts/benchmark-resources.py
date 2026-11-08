#!/usr/bin/env python3
"""Bounded, reproducible Linux resource benchmark for an isolated PreppyDB process."""

from __future__ import annotations

import argparse
import json
import math
import os
import random
import re
import resource
import shutil
import signal
import socket
import string
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


MAX_DOCUMENTS = 100_000
MAX_BATCH = 1_000
DEFAULT_SAFE_DOCUMENTS = 1_000
MAX_WALL_SECONDS = 30 * 60
MAX_ADDRESS_SPACE_BYTES = 1536 * 1024 * 1024
MAX_FILE_BYTES = 2 * 1024 * 1024 * 1024
MAX_CPU_SECONDS = 15 * 60
DEFAULT_PAYLOAD_BYTES = 900
MAX_JSON_STRING_BYTES = 1 << 20
TINY_HTTP_BODY_BYTES = 512 * 1024
STANDARD_HTTP_BODY_BYTES = 4 * 1024 * 1024
SAMPLE_INTERVAL_SECONDS = 0.05
BOOTSTRAP_TOKEN_LINE = re.compile(r"^[A-Za-z0-9_-]{40,}$")


def extract_bootstrap_token(output: str) -> str | None:
    """Extract exactly one URL-safe token line, never a long status message."""
    tokens = [
        line.strip()
        for line in output.splitlines()
        if BOOTSTRAP_TOKEN_LINE.fullmatch(line.strip())
    ]
    return tokens[0] if len(tokens) == 1 else None


def proc_status(pid: int) -> tuple[int | None, int | None, int | None, int | None]:
    try:
        values = Path(f"/proc/{pid}/status").read_text(encoding="ascii")
        rss_fields = {
            line.split(":", 1)[0]: int(line.split()[1]) * 1024
            for line in values.splitlines()
            if line.startswith(("VmRSS:", "VmHWM:"))
        }
        rss = rss_fields.get("VmRSS")
        peak_rss = rss_fields.get("VmHWM")
        thread_count = next(
            (int(line.split(":", 1)[1].strip()) for line in values.splitlines() if line.startswith("Threads:")),
            None,
        )
        stat = Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
        fields = stat[stat.rfind(")") + 2 :].split()
        clock_ticks = os.sysconf("SC_CLK_TCK")
        cpu_seconds = (int(fields[11]) + int(fields[12])) / clock_ticks
        return rss, peak_rss, int(cpu_seconds * 1_000_000), thread_count
    except (FileNotFoundError, IndexError, OSError, ValueError):
        return None, None, None, None


def system_memory_available() -> int | None:
    try:
        for line in Path("/proc/meminfo").read_text(encoding="ascii").splitlines():
            if line.startswith("MemAvailable:"):
                return int(line.split()[1]) * 1024
    except (FileNotFoundError, OSError, ValueError):
        pass
    return None


def system_cpu_ticks() -> tuple[int, int] | None:
    try:
        fields = Path("/proc/stat").read_text(encoding="ascii").splitlines()[0].split()[1:]
        values = [int(value) for value in fields]
        idle = values[3] + (values[4] if len(values) > 4 else 0)
        return sum(values), idle
    except (FileNotFoundError, IndexError, OSError, ValueError):
        return None


class ResourceSampler:
    def __init__(self, pid: int) -> None:
        self.pid = pid
        self.phase = "startup"
        self.kernel_peak_rss = 0
        self.process_peak_threads = 0
        self.phase_peak: dict[str, int] = {}
        self.phase_peak_hwm: dict[str, int] = {}
        self.initial_available = system_memory_available()
        self.minimum_available = self.initial_available
        self.process_cpu_us: int | None = None
        self.system_cpu_start = system_cpu_ticks()
        self.system_cpu_end = self.system_cpu_start
        self.system_cpu_percent: float | None = None
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._sample, daemon=True)

    def start(self) -> None:
        self._thread.start()

    def set_phase(self, phase: str) -> None:
        with self._lock:
            self.phase = phase

    def observe_process_sample(
        self,
        sample: tuple[int | None, int | None, int | None, int | None],
    ) -> None:
        rss, peak_rss, cpu_us, thread_count = sample
        with self._lock:
            if rss is not None:
                self.phase_peak[self.phase] = max(self.phase_peak.get(self.phase, 0), rss)
                self.kernel_peak_rss = max(self.kernel_peak_rss, rss)
            if peak_rss is not None:
                self.kernel_peak_rss = max(self.kernel_peak_rss, peak_rss)
                self.phase_peak_hwm[self.phase] = max(
                    self.phase_peak_hwm.get(self.phase, 0), peak_rss
                )
            if thread_count is not None:
                self.process_peak_threads = max(self.process_peak_threads, thread_count)
            if cpu_us is not None:
                self.process_cpu_us = cpu_us

    def sample_process(self) -> tuple[int | None, int | None, int | None, int | None]:
        sample = proc_status(self.pid)
        self.observe_process_sample(sample)
        return sample

    def _sample(self) -> None:
        while not self._stop.is_set():
            self.sample_process()
            available = system_memory_available()
            ticks = system_cpu_ticks()
            with self._lock:
                if available is not None:
                    self.minimum_available = (
                        available
                        if self.minimum_available is None
                        else min(self.minimum_available, available)
                    )
                if ticks is not None:
                    self.system_cpu_end = ticks
            self._stop.wait(SAMPLE_INTERVAL_SECONDS)

    def finish(self) -> dict[str, Any]:
        self._stop.set()
        self._thread.join(timeout=1)
        start, end = self.system_cpu_start, self.system_cpu_end
        if start is not None and end is not None and end[0] > start[0]:
            total = end[0] - start[0]
            idle = end[1] - start[1]
            self.system_cpu_percent = round(100 * (total - idle) / total, 2)
        return {
            "sample_interval_ms": int(SAMPLE_INTERVAL_SECONDS * 1000),
            "peak_rss_bytes_kernel_high_watermark": self.kernel_peak_rss,
            "peak_process_threads_sampled": self.process_peak_threads,
            "phase_peak_rss_bytes_sampled": self.phase_peak,
            "phase_peak_hwm_bytes_sampled": self.phase_peak_hwm,
            "available_memory_start_bytes": self.initial_available,
            "available_memory_min_bytes": self.minimum_available,
            "process_cpu_seconds_at_last_sample": (
                round(self.process_cpu_us / 1_000_000, 3)
                if self.process_cpu_us is not None
                else None
            ),
            "system_cpu_percent_sampled": self.system_cpu_percent,
        }


def request(
    base: str,
    token: str,
    method: str,
    path: str,
    payload: Any | None = None,
    deadline: float | None = None,
) -> dict[str, Any]:
    body = None if payload is None else json.dumps(payload, separators=(",", ":")).encode()
    headers = {"Authorization": f"Bearer {token}", "Connection": "close"}
    if body is not None:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(base + path, data=body, headers=headers, method=method)
    timeout = 30.0
    if deadline is not None:
        timeout = min(timeout, deadline - time.perf_counter())
        if timeout <= 0:
            raise TimeoutError("resource benchmark exceeded its 30-minute wall-clock limit")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as response:
            raw = response.read()
    except urllib.error.HTTPError as error:
        message = error.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"{method} {path}: HTTP {error.code}: {message[:500]}") from error
    return json.loads(raw) if raw else {}


def free_loopback_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def apply_process_limits() -> None:
    """Bound only the benchmark child; this does not change host configuration."""
    resource.setrlimit(resource.RLIMIT_AS, (MAX_ADDRESS_SPACE_BYTES, MAX_ADDRESS_SPACE_BYTES))
    resource.setrlimit(resource.RLIMIT_FSIZE, (MAX_FILE_BYTES, MAX_FILE_BYTES))
    resource.setrlimit(resource.RLIMIT_CPU, (MAX_CPU_SECONDS, MAX_CPU_SECONDS + 1))
    resource.setrlimit(resource.RLIMIT_NOFILE, (128, 128))
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))


def wait_ready(process: subprocess.Popen[bytes], base: str, timeout: float = 30) -> float:
    started = time.perf_counter()
    while time.perf_counter() - started < timeout:
        if process.poll() is not None:
            raise RuntimeError(f"PreppyDB exited during startup with status {process.returncode}")
        try:
            with urllib.request.urlopen(base + "/healthz", timeout=1) as response:
                if response.status == 200:
                    return time.perf_counter() - started
        except (OSError, urllib.error.URLError):
            time.sleep(0.1)
    raise TimeoutError("PreppyDB did not become healthy within 30 seconds")


def stop_process(process: subprocess.Popen[bytes]) -> float:
    started = time.perf_counter()
    if process.poll() is None:
        process.send_signal(signal.SIGINT)
        try:
            process.wait(timeout=35)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
            raise TimeoutError("PreppyDB did not shut down within 35 seconds")
    return time.perf_counter() - started


def crash_process(process: subprocess.Popen[bytes]) -> float:
    """SIGKILL only the isolated benchmark child to measure WAL-tail recovery."""
    started = time.perf_counter()
    if process.poll() is None:
        process.kill()
        process.wait(timeout=5)
    return time.perf_counter() - started


def percentile(values: list[float], percent: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    index = max(0, math.ceil(percent * len(ordered)) - 1)
    return round(ordered[index] * 1000, 3)


def file_sizes(data_dir: Path) -> dict[str, int]:
    wal_bytes = 0
    snapshot_bytes = 0
    for path in data_dir.rglob("*"):
        if not path.is_file():
            continue
        size = path.stat().st_size
        if path.suffix == ".wal":
            wal_bytes += size
        elif path.suffix == ".pdb":
            snapshot_bytes += size
    return {"wal_bytes": wal_bytes, "snapshot_bytes": snapshot_bytes}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", required=True, type=Path, help="Path to the built preppydb-server binary")
    parser.add_argument("--documents", type=int, default=DEFAULT_SAFE_DOCUMENTS)
    parser.add_argument(
        "--payload-bytes",
        type=int,
        default=DEFAULT_PAYLOAD_BYTES,
        help="deterministic ASCII payload size per document (default: 900; maximum: 1 MiB)",
    )
    parser.add_argument("--profile", choices=("standard", "tiny"), default="standard")
    parser.add_argument(
        "--idle-seconds",
        type=float,
        default=10.0,
        help="duration of the no-request CPU observation window (default: 10 seconds)",
    )
    parser.add_argument(
        "--batch-size",
        type=int,
        help="write batch size (default: 500 in tiny profile, otherwise 1,000)",
    )
    parser.add_argument(
        "--index-count",
        type=int,
        default=1,
        help="number of secondary indexes on the synthetic group field (1..16; default: 1)",
    )
    parser.add_argument("--seed", type=int, default=20261108)
    parser.add_argument("--source-commit", default=os.environ.get("PREPPYDB_SOURCE_COMMIT"))
    parser.add_argument("--manifest", type=Path, help="optional key=value build manifest to include")
    parser.add_argument(
        "--allow-large-test",
        action="store_true",
        help="required above 1,000 documents; runs under per-process resource limits",
    )
    parser.add_argument("--output", type=Path, default=Path("-"), help="JSON output path, or - for stdout")
    args = parser.parse_args()
    if args.batch_size is None:
        args.batch_size = 500 if args.profile == "tiny" else MAX_BATCH

    if not sys.platform.startswith("linux"):
        parser.error("this resource sampler currently requires Linux /proc")
    if not args.server.is_file():
        parser.error("--server must point to an existing PreppyDB server binary")
    if not 1 <= args.documents <= MAX_DOCUMENTS:
        parser.error(f"--documents must be between 1 and {MAX_DOCUMENTS}")
    if not 1 <= args.payload_bytes <= MAX_JSON_STRING_BYTES:
        parser.error(f"--payload-bytes must be between 1 and {MAX_JSON_STRING_BYTES}")
    if not 0.1 <= args.idle_seconds <= 300:
        parser.error("--idle-seconds must be between 0.1 and 300")
    if args.documents > DEFAULT_SAFE_DOCUMENTS and not args.allow_large_test:
        parser.error("more than 1,000 documents requires --allow-large-test")
    if not 1 <= args.batch_size <= MAX_BATCH:
        parser.error(f"--batch-size must be between 1 and {MAX_BATCH}")
    if not 1 <= args.index_count <= 16:
        parser.error("--index-count must be between 1 and 16")
    body_limit = TINY_HTTP_BODY_BYTES if args.profile == "tiny" else STANDARD_HTTP_BODY_BYTES
    batch_operations = min(args.batch_size, args.documents)
    sample_operation = {
        "op": "put",
        "key": "doc-99999999",
        "expected_version": 0,
        "value": {"group": 999, "payload": "x" * args.payload_bytes},
    }
    operation_bytes = len(json.dumps(sample_operation, separators=(",", ":")).encode())
    estimated_batch_bytes = (
        len(b'{"operations":[')
        + batch_operations * operation_bytes
        + max(0, batch_operations - 1)
        + len(b"]}")
    )
    if estimated_batch_bytes > body_limit:
        parser.error(
            f"estimated write batch ({estimated_batch_bytes} bytes) exceeds the "
            f"{args.profile} HTTP body limit ({body_limit} bytes); reduce "
            "--payload-bytes or --batch-size"
        )
    if args.manifest and not args.manifest.is_file():
        parser.error("--manifest must point to an existing build manifest")

    binary = args.server.resolve()
    rng = random.Random(args.seed)
    alphabet = string.ascii_letters + string.digits
    payload = "".join(rng.choice(alphabet) for _ in range(args.payload_bytes))
    batch_latencies: list[float] = []
    read_latencies: list[float] = []
    start_times: list[float] = []
    shutdown_times: list[float] = []
    started_at = time.perf_counter()
    deadline = started_at + MAX_WALL_SECONDS
    output: dict[str, Any] = {}

    with tempfile.TemporaryDirectory(prefix="preppydb-resource-bench-") as temporary:
        data_dir = Path(temporary) / "data"
        free_bytes = shutil.disk_usage(temporary).free
        if args.documents > DEFAULT_SAFE_DOCUMENTS and free_bytes < 1024**3:
            raise RuntimeError("large benchmark requires at least 1 GiB free in the temporary directory")
        init = subprocess.run(
            [str(binary), "--data-dir", str(data_dir), "--bootstrap-token", "resource-bench", "--init-only"],
            check=True,
            capture_output=True,
            text=True,
        )
        token = extract_bootstrap_token(init.stdout)
        if token is None:
            raise RuntimeError("PreppyDB initialization did not return the one-time admin token")

        port = free_loopback_port()
        base = f"http://127.0.0.1:{port}"
        process: subprocess.Popen[bytes] | None = None
        sampler: ResourceSampler | None = None

        def start_server(phase: str) -> float:
            nonlocal process, sampler
            command = [str(binary), "--data-dir", str(data_dir), "--listen", f"127.0.0.1:{port}"]
            if args.profile != "standard":
                command.extend(["--profile", args.profile])
            process = subprocess.Popen(
                command,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                preexec_fn=apply_process_limits,
            )
            sampler = ResourceSampler(process.pid)
            sampler.set_phase(phase)
            sampler.start()
            elapsed = wait_ready(process, base)
            start_times.append(elapsed)
            return elapsed

        try:
            start_server("load")
            sampler.set_phase("idle")
            idle_before = sampler.sample_process()
            idle_started = time.perf_counter()
            time.sleep(args.idle_seconds)
            idle_elapsed = time.perf_counter() - idle_started
            idle_after = sampler.sample_process()
            idle_rss = idle_after[0]
            idle_cpu_percent = (
                round(100 * (idle_after[2] - idle_before[2]) / 1_000_000 / idle_elapsed, 2)
                if idle_before[2] is not None and idle_after[2] is not None and idle_elapsed > 0
                else None
            )
            idle_thread_count = idle_after[3]
            sampler.set_phase("load")
            request(
                base,
                token,
                "POST",
                "/v1/collections",
                {"name": "bench", "max_docs": args.documents},
                deadline,
            )

            for offset in range(0, args.documents, args.batch_size):
                if time.perf_counter() >= deadline:
                    raise TimeoutError("resource benchmark exceeded its 30-minute wall-clock limit")
                end = min(offset + args.batch_size, args.documents)
                operations = [
                    {
                        "op": "put",
                        "key": f"doc-{index:08d}",
                        "expected_version": 0,
                        "value": {"group": index % 1000, "payload": payload},
                    }
                    for index in range(offset, end)
                ]
                batch_started = time.perf_counter()
                request(
                    base,
                    token,
                    "POST",
                    "/v1/collections/bench/batch",
                    {"operations": operations},
                    deadline,
                )
                batch_latencies.append(time.perf_counter() - batch_started)

            loaded_sample = sampler.sample_process()
            sampler.set_phase("index_build")
            index_rss_before, index_hwm_before, _, _ = sampler.sample_process()
            index_started = time.perf_counter()
            for index_number in range(args.index_count):
                index_name = "by_group" if args.index_count == 1 else f"by_group_{index_number}"
                request(
                    base,
                    token,
                    "POST",
                    "/v1/collections/bench/indexes",
                    {"name": index_name, "field": "group", "unique": False},
                    deadline,
                )
            index_build_seconds = time.perf_counter() - index_started
            index_rss_after, index_hwm_after, _, _ = sampler.sample_process()

            sampler.set_phase("read")
            for index in range(min(1000, args.documents)):
                if time.perf_counter() >= deadline:
                    raise TimeoutError("resource benchmark exceeded its 30-minute wall-clock limit")
                read_started = time.perf_counter()
                request(
                    base,
                    token,
                    "GET",
                    f"/v1/collections/bench/documents/doc-{index:08d}",
                    deadline=deadline,
                )
                read_latencies.append(time.perf_counter() - read_started)

            read_complete_sample = sampler.sample_process()
            sampler.set_phase("checkpoint")
            checkpoint_rss_before, _, _, _ = sampler.sample_process()
            checkpoint_started = time.perf_counter()
            request(base, token, "POST", "/v1/admin/checkpoint", {}, deadline)
            checkpoint_seconds = time.perf_counter() - checkpoint_started
            checkpoint_rss_after, _, _, _ = sampler.sample_process()
            sizes_after_checkpoint = file_sizes(data_dir)

            tail_before_sample = sampler.sample_process()
            sampler.set_phase("tail_writes")
            tail_count = min(args.batch_size, args.documents)
            tail_ops = [
                {
                    "op": "put",
                    "key": f"doc-{index:08d}",
                    "expected_version": 1,
                    "value": {"group": index % 1000, "payload": payload},
                }
                for index in range(tail_count)
            ]
            request(base, token, "POST", "/v1/collections/bench/batch", {"operations": tail_ops}, deadline)
            tail_after_sample = sampler.sample_process()
            before_crash_sample = sampler.sample_process()
            sampler.set_phase("pre_crash")
            sampler.sample_process()
            resources = sampler.finish()
            crash_recovery_kill_seconds = crash_process(process)

            sampler = None
            recovery_startup = start_server("recovery")
            after_recovery_sample = sampler.sample_process()
            sampler.set_phase("verify")
            verification = request(base, token, "POST", "/v1/admin/verify", {}, deadline)
            after_verify_sample = sampler.sample_process()
            if verification.get("data", {}).get("ok") is not True:
                raise RuntimeError("database verify failed after recovery")
            sampler.set_phase("stats")
            stats = request(base, token, "GET", "/v1/stats", deadline=deadline)
            after_stats_sample = sampler.sample_process()
            recovered_count = stats.get("data", {}).get("total_documents")
            if recovered_count != args.documents:
                raise RuntimeError(f"recovered {recovered_count} documents; expected {args.documents}")
            final_sizes = file_sizes(data_dir)
            sampler.set_phase("shutdown")
            sampler.sample_process()
            recovery_resources = sampler.finish()
            shutdown_times.append(stop_process(process))
            resources["peak_rss_bytes_kernel_high_watermark"] = max(
                resources["peak_rss_bytes_kernel_high_watermark"],
                recovery_resources["peak_rss_bytes_kernel_high_watermark"],
            )
            resources["peak_process_threads_sampled"] = max(
                resources["peak_process_threads_sampled"],
                recovery_resources["peak_process_threads_sampled"],
            )
            resources["available_memory_min_bytes"] = min(
                item
                for item in (
                    resources["available_memory_min_bytes"],
                    recovery_resources["available_memory_min_bytes"],
                )
                if item is not None
            )
            resources["process_cpu_seconds_across_runs"] = round(
                sum(
                    item
                    for item in (
                        resources["process_cpu_seconds_at_last_sample"],
                        recovery_resources["process_cpu_seconds_at_last_sample"],
                    )
                    if item is not None
                ),
                3,
            )
            resources["process_cpu_percent_of_one_core"] = round(
                100 * resources["process_cpu_seconds_across_runs"] / max(time.perf_counter() - started_at, 0.001),
                2,
            )
            resources["system_cpu_percent_sampled_by_run"] = [
                resources.pop("system_cpu_percent_sampled"),
                recovery_resources["system_cpu_percent_sampled"],
            ]
            for phase, peak in recovery_resources["phase_peak_rss_bytes_sampled"].items():
                resources["phase_peak_rss_bytes_sampled"][phase] = peak
            for phase, peak in recovery_resources["phase_peak_hwm_bytes_sampled"].items():
                resources["phase_peak_hwm_bytes_sampled"][phase] = peak

            output = {
                "format": "preppydb-resource-benchmark-v1",
                "synthetic_only": True,
                "seed": args.seed,
                "documents": args.documents,
                "payload_bytes_per_document": args.payload_bytes,
                "approximate_encoded_document_bytes": len(
                    json.dumps({"group": 999, "payload": payload}, separators=(",", ":")).encode()
                ),
                "batch_size": args.batch_size,
                "profile": args.profile,
                "index_count": args.index_count,
                "source_commit": args.source_commit,
                "build_manifest": (
                    dict(
                        line.split("=", 1)
                        for line in args.manifest.read_text(encoding="utf-8").splitlines()
                        if "=" in line
                    )
                    if args.manifest
                    else None
                ),
                "index": {
                    "name": "by_group" if args.index_count == 1 else f"by_group_x{args.index_count}",
                    "field": "group",
                    "count": args.index_count,
                    "build_seconds": round(index_build_seconds, 6),
                    "rss_before_bytes": index_rss_before,
                    "rss_after_bytes": index_rss_after,
                    "high_water_before_bytes": index_hwm_before,
                    "high_water_after_bytes": index_hwm_after,
                    "high_water_increase_bytes": (
                        max(0, index_hwm_after - index_hwm_before)
                        if index_hwm_before is not None and index_hwm_after is not None
                        else None
                    ),
                },
                "stage_boundary_memory_bytes": {
                    "after_load": {
                        "rss": loaded_sample[0],
                        "high_water": loaded_sample[1],
                    },
                    "after_index_build": {
                        "rss": index_rss_after,
                        "high_water": index_hwm_after,
                    },
                    "after_reads": {
                        "rss": read_complete_sample[0],
                        "high_water": read_complete_sample[1],
                    },
                    "before_tail_writes": {
                        "rss": tail_before_sample[0],
                        "high_water": tail_before_sample[1],
                    },
                    "after_tail_writes": {
                        "rss": tail_after_sample[0],
                        "high_water": tail_after_sample[1],
                    },
                    "before_crash": {
                        "rss": before_crash_sample[0],
                        "high_water": before_crash_sample[1],
                    },
                    "after_recovery_startup": {
                        "rss": after_recovery_sample[0],
                        "high_water": after_recovery_sample[1],
                    },
                    "after_verify": {
                        "rss": after_verify_sample[0],
                        "high_water": after_verify_sample[1],
                    },
                    "after_stats": {
                        "rss": after_stats_sample[0],
                        "high_water": after_stats_sample[1],
                    },
                },
                "checkpoint_rss_before_bytes": checkpoint_rss_before,
                "checkpoint_rss_after_bytes": checkpoint_rss_after,
                "idle_rss_after_health_bytes": idle_rss,
                "idle_cpu_percent_of_one_core": idle_cpu_percent,
                "process_threads_at_idle": idle_thread_count,
                "timing": {
                    "startup_seconds": start_times[0],
                    "checkpoint_seconds": round(checkpoint_seconds, 6),
                    "recovery_startup_seconds": recovery_startup,
                    "isolated_process_kill_seconds": round(crash_recovery_kill_seconds, 6),
                    "shutdown_seconds": shutdown_times,
                    "write_batch_p50_ms": percentile(batch_latencies, 0.50),
                    "write_batch_p95_ms": percentile(batch_latencies, 0.95),
                    "read_p50_ms": percentile(read_latencies, 0.50),
                    "read_p95_ms": percentile(read_latencies, 0.95),
                },
                "storage_after_checkpoint": sizes_after_checkpoint,
                "storage_after_recovery": final_sizes,
                "resources": resources,
                "measurement": {
                    "host_os": "Linux",
                    "host_architecture": os.uname().machine,
                    "wall_seconds": round(time.perf_counter() - started_at, 3),
                    "wall_clock_limit_seconds": MAX_WALL_SECONDS,
                    "idle_observation_seconds": round(idle_elapsed, 3),
                    "rss_peak_source": "maximum of Linux /proc/<pid>/status VmHWM and sampled/direct VmRSS observations",
                    "recovery_method": "SIGKILL of this isolated benchmark child after a checkpoint and WAL tail write",
                    "data_directory": "temporary and removed after result is written",
                    "child_process_limits": {
                        "address_space_bytes": MAX_ADDRESS_SPACE_BYTES,
                        "file_size_bytes": MAX_FILE_BYTES,
                        "cpu_seconds": MAX_CPU_SECONDS,
                        "open_files": 128,
                        "core_dumps": 0,
                    },
                    "limits_note": "RSS is sampled; per-process limits apply only to the temporary benchmark server, not to aggregate host or page-cache use.",
                },
            }
        finally:
            if process is not None and process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            if sampler is not None:
                sampler.finish()

    encoded = json.dumps(output, indent=2, sort_keys=True) + "\n"
    if args.output == Path("-"):
        sys.stdout.write(encoded)
    else:
        args.output.write_text(encoded, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
