#!/usr/bin/env python3
"""Small fail-closed qualification check for the isolated stress-test container."""

from __future__ import annotations

import argparse
import gc
import json
import os
import pathlib
import re
import signal
import subprocess
import sys
import time
from typing import Any


MEMORY_LIMIT = 2 * 1024**3
CPU_LIMIT = 2.0
PID_LIMIT = 512
DATA_LIMIT = 512 * 1024**2


def cgroup_v2_dir() -> pathlib.Path:
    cgroup_path = None
    for line in pathlib.Path("/proc/self/cgroup").read_text().splitlines():
        hierarchy, controllers, path = line.split(":", 2)
        if hierarchy == "0" and controllers == "":
            cgroup_path = path
            break
    if cgroup_path is None:
        raise RuntimeError("unified cgroup v2 membership is not visible")

    for line in pathlib.Path("/proc/self/mountinfo").read_text().splitlines():
        left, right = line.split(" - ", 1)
        if right.split()[0] != "cgroup2":
            continue
        fields = left.split()
        mount_root = fields[3]
        mount_point = pathlib.Path(fields[4].replace("\\040", " "))
        if mount_root == "/":
            relative = cgroup_path.lstrip("/")
        elif cgroup_path == mount_root:
            relative = ""
        elif cgroup_path.startswith(mount_root.rstrip("/") + "/"):
            relative = cgroup_path[len(mount_root.rstrip("/")) :].lstrip("/")
        else:
            continue
        candidate = mount_point.joinpath(*relative.split("/")) if relative else mount_point
        if (candidate / "memory.max").is_file():
            return candidate
    raise RuntimeError("cannot resolve this process's cgroup v2 control directory")


def read_kv(path: pathlib.Path) -> dict[str, int]:
    result: dict[str, int] = {}
    for line in path.read_text().splitlines():
        key, value = line.split()
        result[key] = int(value)
    return result


def assert_data_tmpfs(path: pathlib.Path) -> dict[str, int | str]:
    mount = None
    for line in pathlib.Path("/proc/mounts").read_text().splitlines():
        fields = line.split()
        if fields[1] == str(path):
            mount = fields
            break
    if mount is None or mount[2] != "tmpfs":
        raise RuntimeError("/stress-data is not a dedicated tmpfs mount")
    match = re.search(r"(?:^|,)size=(\d+)([kKmMgG]?)", mount[3])
    if not match:
        raise RuntimeError("/stress-data tmpfs does not expose an explicit size limit")
    amount = int(match.group(1))
    factor = {"": 1, "k": 1024, "m": 1024**2, "g": 1024**3}[match.group(2).lower()]
    size_bytes = amount * factor
    if size_bytes <= 0 or size_bytes > DATA_LIMIT:
        raise RuntimeError(f"/stress-data tmpfs limit is unexpected: {size_bytes} bytes")
    stat = os.statvfs(path)
    usable = stat.f_blocks * stat.f_frsize
    if usable > DATA_LIMIT:
        raise RuntimeError(f"/stress-data reports {usable} bytes, above its configured cap")
    return {"filesystem": "tmpfs", "configured_bytes": size_bytes, "reported_bytes": usable}


def watchdog_check(work_dir: pathlib.Path) -> dict[str, Any]:
    child_pid_file = work_dir / "watchdog-child.pid"
    command = [
        "timeout",
        "--signal=TERM",
        "--kill-after=1s",
        "2s",
        "sh",
        "-c",
        f"sleep 60 & echo $! > {child_pid_file}; wait",
    ]
    started = time.monotonic()
    result = subprocess.run(command, capture_output=True, text=True, timeout=6, check=False)
    if result.returncode != 124:
        raise RuntimeError(f"GNU timeout returned {result.returncode}, expected 124")
    elapsed = time.monotonic() - started
    if not 1.5 <= elapsed <= 5.5 or not child_pid_file.is_file():
        raise RuntimeError("watchdog did not stop its synthetic child within its deadline")
    child_pid = int(child_pid_file.read_text().strip())
    for _ in range(30):
        if not pathlib.Path(f"/proc/{child_pid}").exists():
            break
        time.sleep(0.05)
    if pathlib.Path(f"/proc/{child_pid}").exists():
        raise RuntimeError("watchdog or container init did not reap its synthetic child")
    return {"exit_code": result.returncode, "elapsed_seconds": round(elapsed, 3), "child_reaped": True}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", type=pathlib.Path, required=True)
    parser.add_argument("--load-script", type=pathlib.Path, required=True)
    args = parser.parse_args()
    checks: dict[str, Any] = {}
    try:
        if not pathlib.Path("/sys/fs/cgroup/cgroup.controllers").is_file():
            raise RuntimeError("cgroup v2 is required; refusing to qualify an unknown controller mode")
        group = cgroup_v2_dir()
        controllers = pathlib.Path("/sys/fs/cgroup/cgroup.controllers").read_text().split()
        for required in ("memory", "cpu", "pids"):
            if required not in controllers:
                raise RuntimeError(f"cgroup v2 controller {required!r} is unavailable")

        memory_max = int((group / "memory.max").read_text().strip())
        if memory_max != MEMORY_LIMIT:
            raise RuntimeError(f"effective memory.max is {memory_max}, expected {MEMORY_LIMIT}")
        swap_max_path = group / "memory.swap.max"
        if not swap_max_path.is_file() or int(swap_max_path.read_text().strip()) != 0:
            raise RuntimeError("container swap is not disabled by the memory cgroup")
        cpu_quota, cpu_period = (int(value) for value in (group / "cpu.max").read_text().split())
        cpu_cores = cpu_quota / cpu_period
        if not 1.9 <= cpu_cores <= CPU_LIMIT:
            raise RuntimeError(f"effective CPU quota is {cpu_cores:.3f} cores, expected at most 2")
        pids_max = int((group / "pids.max").read_text().strip())
        if pids_max != PID_LIMIT:
            raise RuntimeError(f"effective pids.max is {pids_max}, expected {PID_LIMIT}")
        checks["cgroup"] = {
            "version": 2,
            "path": str(group),
            "controllers": controllers,
            "memory_max_bytes": memory_max,
            "swap_max_bytes": 0,
            "cpu_max": {"quota": cpu_quota, "period": cpu_period, "cores": cpu_cores},
            "pids_max": pids_max,
        }

        before = int((group / "memory.current").read_text().strip())
        sample = bytearray(32 * 1024**2)
        for offset in range(0, len(sample), 4096):
            sample[offset] = 1
        after = int((group / "memory.current").read_text().strip())
        charged = after - before
        if charged < 24 * 1024**2:
            raise RuntimeError(f"cgroup did not account for a small touched allocation ({charged} bytes)")
        del sample
        gc.collect()
        checks["memory_charge"] = {"touched_bytes": 32 * 1024**2, "observed_delta_bytes": charged}

        cpu_stat_path = group / "cpu.stat"
        initial_cpu = read_kv(cpu_stat_path)
        worker_code = "import time; end=time.monotonic()+1.5; x=1\nwhile time.monotonic()<end: x=(x*48271+1)%2147483647"
        workers = [
            subprocess.Popen([sys.executable, "-c", worker_code], start_new_session=True)
            for _ in range(4)
        ]
        try:
            for worker in workers:
                worker.wait(timeout=5)
                if worker.returncode != 0:
                    raise RuntimeError("bounded CPU accounting worker failed")
        finally:
            for worker in workers:
                if worker.poll() is None:
                    try:
                        os.killpg(worker.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    worker.wait(timeout=2)
        final_cpu = read_kv(cpu_stat_path)
        throttled = final_cpu.get("nr_throttled", 0) - initial_cpu.get("nr_throttled", 0)
        if throttled <= 0:
            raise RuntimeError("CPU quota is configured but did not throttle a brief four-process probe")
        checks["cpu_enforcement"] = {"probe_processes": 4, "throttled_periods_delta": throttled}

        before_pids = int((group / "pids.current").read_text().strip())
        sleepers = [subprocess.Popen(["sleep", "30"], start_new_session=True) for _ in range(8)]
        try:
            time.sleep(0.15)
            during_pids = int((group / "pids.current").read_text().strip())
            if during_pids - before_pids < 4:
                raise RuntimeError("cgroup pids.current did not account for synthetic child processes")
        finally:
            for child in sleepers:
                if child.poll() is None:
                    try:
                        os.killpg(child.pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                    try:
                        child.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        os.killpg(child.pid, signal.SIGKILL)
                        child.wait(timeout=2)
        checks["process_accounting"] = {"configured_max": pids_max, "synthetic_children": len(sleepers)}

        data_dir = pathlib.Path("/stress-data")
        checks["data_directory"] = assert_data_tmpfs(data_dir)
        probe = data_dir / "quota-probe.bin"
        probe.write_bytes(b"p" * (8 * 1024**2))
        if probe.stat().st_size != 8 * 1024**2:
            raise RuntimeError("temporary data write did not round-trip")
        probe.unlink()
        checks["data_probe_bytes"] = 8 * 1024**2
        checks["watchdog"] = watchdog_check(data_dir)

        if not args.server.is_file() or not os.access(args.server, os.X_OK):
            raise RuntimeError("release server binary is not mounted executable")
        result = subprocess.run([str(args.server), "--help"], capture_output=True, text=True, timeout=10, check=False)
        if result.returncode != 0:
            raise RuntimeError("release server binary failed its bounded --help smoke check")
        checks["release_binary"] = {"path": str(args.server), "help_exit_code": result.returncode}

        if not args.load_script.is_file():
            raise RuntimeError("locked load generator is not mounted read-only")
        gated = subprocess.run(
            [
                sys.executable,
                str(args.load_script),
                "--server",
                str(args.server),
                "--clients",
                "1",
                "--mode",
                "read",
            ],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
        if gated.returncode != 2 or "load is disabled" not in gated.stderr:
            raise RuntimeError("load generator did not fail closed before starting a server or load")
        checks["load_generator_gate"] = {"refused_without_qualification_and_approval": True, "exit_code": gated.returncode}

        payload = {
            "status": "passed",
            "test": "isolation-only",
            "commit": os.environ.get("PREPPYDB_COMMIT", "unknown"),
            "runner": os.environ.get("PREPPYDB_RUNNER", "unknown"),
            "container_image": os.environ.get("PREPPYDB_TEST_IMAGE", "unknown"),
            "docker_host_cgroup_version": os.environ.get("PREPPYDB_DOCKER_CGROUP", "unknown"),
            "toolchain": {
                "rustc": os.environ.get("PREPPYDB_RUSTC", "unknown"),
                "cargo": os.environ.get("PREPPYDB_CARGO", "unknown"),
            },
            "limits": {"memory_bytes": MEMORY_LIMIT, "cpu_cores": CPU_LIMIT, "pids": PID_LIMIT, "data_tmpfs_bytes_max": DATA_LIMIT},
            "checks": checks,
            "load_test_started": False,
        }
        print(json.dumps(payload, sort_keys=True))
        return 0
    except Exception as error:
        print(json.dumps({"status": "failed", "test": "isolation-only", "error": str(error), "checks_completed": checks, "load_test_started": False}, sort_keys=True))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
