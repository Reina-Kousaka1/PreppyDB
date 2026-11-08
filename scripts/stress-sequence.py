#!/usr/bin/env python3
"""Run isolation qualification, then a bounded synthetic client ladder in Docker."""

from __future__ import annotations

import json
import os
import pathlib
import re
import subprocess
import sys
import time
from typing import Any


IMAGE = "python:3.12-slim-bookworm"
CLIENT_LEVELS = (1, 10, 25, 50, 100)
TOTAL_WATCHDOG_SECONDS = 170
STAGE_DURATION_SECONDS = 15
DOCUMENTS = 500
PAYLOAD_BYTES = 512
MEMORY_BYTES = 2 * 1024**3
PIDS_LIMIT = 512
DATA_BYTES = 512 * 1024**2
ENABLE_PHRASE = "I_APPROVE_BOUNDED_PREPPYDB_STRESS"


def docker_config(run_name: str, stage: str, server: pathlib.Path, scripts: pathlib.Path, load: bool) -> str:
    command = (
        ["python3", "-B", "/opt/stress-load.py", "--server", "/usr/local/bin/preppydb-server",
         "--clients", stage, "--mode", "mixed", "--documents", str(DOCUMENTS),
         "--payload-bytes", str(PAYLOAD_BYTES), "--duration-seconds", str(STAGE_DURATION_SECONDS)]
        if load
        else ["python3", "-B", "/opt/stress-isolation-check.py", "--server",
              "/usr/local/bin/preppydb-server", "--load-script", "/opt/stress-load.py"]
    )
    args = [
        "docker", "create", "--name", run_name, "--init", "--cgroupns", "private", "--network", "none",
        "--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges:true",
        "--memory", "2g", "--memory-swap", "2g", "--cpus", "2", "--pids-limit", str(PIDS_LIMIT),
        "--ulimit", "nofile=512:512", "--shm-size=16m",
        "--tmpfs", "/stress-data:rw,size=512m,mode=1777,nosuid,nodev,noexec",
        "--tmpfs", "/tmp:rw,size=64m,mode=1777,nosuid,nodev",
        "--user", "10001:10001",
        "--env", f"PREPPYDB_COMMIT={os.environ['PREPPYDB_COMMIT']}",
        "--env", f"PREPPYDB_RUNNER={os.environ.get('RUNNER_NAME', 'unknown')}",
        "--env", f"PREPPYDB_TEST_IMAGE={IMAGE}",
        "--env", "PYTHONDONTWRITEBYTECODE=1",
        "--volume", f"{server}:/usr/local/bin/preppydb-server:ro",
        "--volume", f"{scripts / 'stress-isolation-check.py'}:/opt/stress-isolation-check.py:ro",
        "--volume", f"{scripts / 'stress-load.py'}:/opt/stress-load.py:ro",
    ]
    if load:
        args += [
            "--env", f"PREPPYDB_ISOLATION_QUALIFIED_COMMIT={os.environ['PREPPYDB_COMMIT']}",
            "--env", f"PREPPYDB_ENABLE_STRESS={ENABLE_PHRASE}",
        ]
    return subprocess.check_output(args + [IMAGE] + command, text=True).strip()


def inspect_limits(container_id: str) -> dict[str, Any]:
    info = json.loads(subprocess.check_output(["docker", "inspect", container_id], text=True))[0]
    host = info["HostConfig"]
    assert host["Memory"] == MEMORY_BYTES, host["Memory"]
    assert host["MemorySwap"] == MEMORY_BYTES, host["MemorySwap"]
    assert host["NanoCpus"] == 2_000_000_000, host["NanoCpus"]
    assert host["PidsLimit"] == PIDS_LIMIT, host["PidsLimit"]
    assert host["NetworkMode"] == "none", host["NetworkMode"]
    assert host["ReadonlyRootfs"] is True
    assert host["ShmSize"] == 16 * 1024**2
    assert host["CgroupnsMode"] == "private", host.get("CgroupnsMode")
    assert "ALL" in host["CapDrop"], host.get("CapDrop")
    assert "no-new-privileges:true" in host["SecurityOpt"], host.get("SecurityOpt")
    assert info["Config"]["User"] == "10001:10001", info["Config"]["User"]
    nofile = [limit for limit in host["Ulimits"] if limit["Name"] == "nofile"]
    assert len(nofile) == 1 and nofile[0]["Soft"] == 512 and nofile[0]["Hard"] == 512, host["Ulimits"]
    for mountpoint, wanted in (("/stress-data", DATA_BYTES), ("/tmp", 64 * 1024**2)):
        options = host["Tmpfs"].get(mountpoint, "")
        match = re.search(r"(?:^|,)size=(\d+)([kKmMgG]?)", options)
        if not match:
            raise RuntimeError(f"{mountpoint} has no explicit tmpfs size limit")
        factor = {"": 1, "k": 1024, "m": 1024**2, "g": 1024**3}[match.group(2).lower()]
        if int(match.group(1)) * factor != wanted:
            raise RuntimeError(f"unexpected {mountpoint} tmpfs size: {options}")
        for option in ("nosuid", "nodev"):
            if option not in options.split(","):
                raise RuntimeError(f"{mountpoint} is missing tmpfs mount option {option}")
        if mountpoint == "/stress-data" and "noexec" not in options.split(","):
            raise RuntimeError("/stress-data must be mounted noexec")
    mounts = {mount["Destination"]: mount for mount in info["Mounts"]}
    for destination in ("/usr/local/bin/preppydb-server", "/opt/stress-isolation-check.py", "/opt/stress-load.py"):
        if destination not in mounts or mounts[destination]["Type"] != "bind" or mounts[destination]["RW"]:
            raise RuntimeError(f"expected read-only bind mount at {destination}")
    return {
        "image": info["Config"]["Image"],
        "memory_bytes": host["Memory"],
        "memory_swap_bytes": host["MemorySwap"],
        "cpu_nano_cpus": host["NanoCpus"],
        "pids_limit": host["PidsLimit"],
        "network": host["NetworkMode"],
        "read_only_root": host["ReadonlyRootfs"],
        "tmpfs": host["Tmpfs"],
    }


def run_container(container_id: str, deadline: float) -> tuple[int, str]:
    try:
        result = subprocess.run(
            ["docker", "start", "--attach", container_id],
            capture_output=True,
            text=True,
            check=False,
            timeout=max(1, deadline - time.monotonic()),
        )
        return result.returncode, result.stdout
    finally:
        subprocess.run(["docker", "rm", "--force", container_id], capture_output=True, check=False, timeout=10)


def parse_report(output: str) -> dict[str, Any]:
    lines = [line for line in output.splitlines() if line.strip()]
    if len(lines) != 1:
        raise RuntimeError("container did not emit exactly one JSON report")
    report = json.loads(lines[0])
    if not isinstance(report, dict):
        raise RuntimeError("container report is not a JSON object")
    return report


def main() -> int:
    repo = pathlib.Path.cwd()
    server = repo / "target" / "release" / "preppydb-server"
    scripts = repo / "scripts"
    if not server.is_file() or not os.access(server, os.X_OK):
        raise RuntimeError("release server binary is missing")
    if not os.environ.get("PREPPYDB_COMMIT"):
        raise RuntimeError("PREPPYDB_COMMIT is required")
    if os.environ.get("PREPPYDB_ENABLE_STRESS") != ENABLE_PHRASE:
        raise RuntimeError("bounded stress requires the explicitly approved manual workflow input")

    subprocess.run(["docker", "pull", IMAGE], check=True, timeout=90)
    deadline = time.monotonic() + TOTAL_WATCHDOG_SECONDS
    run_id = os.environ.get("GITHUB_RUN_ID", str(os.getpid()))
    report: dict[str, Any] = {
        "test": "first-controlled-stress",
        "commit": os.environ["PREPPYDB_COMMIT"],
        "runner": os.environ.get("RUNNER_NAME", "unknown"),
        "container_image": IMAGE,
        "limits": {"memory_bytes": MEMORY_BYTES, "swap_bytes": 0, "cpu_cores": 2,
                   "pids": PIDS_LIMIT, "data_tmpfs_bytes": DATA_BYTES, "watchdog_seconds": TOTAL_WATCHDOG_SECONDS},
        "client_levels": list(CLIENT_LEVELS),
        "stage_duration_seconds": STAGE_DURATION_SECONDS,
        "documents": DOCUMENTS,
        "payload_bytes": PAYLOAD_BYTES,
        "isolation": None,
        "stages": [],
        "highest_stable_clients": 0,
    }

    check_name = f"preppydb-check-{run_id}-{os.getpid()}"
    check_id = docker_config(check_name, "", server, scripts, load=False)
    try:
        inspect_limits(check_id)
        check_code, check_output = run_container(check_id, deadline)
    finally:
        subprocess.run(["docker", "rm", "--force", check_id], capture_output=True, check=False, timeout=10)
    isolation = parse_report(check_output)
    if (check_code != 0 or isolation.get("status") != "passed" or
            isolation.get("test") != "isolation-only" or isolation.get("load_test_started") is not False):
        raise RuntimeError(f"isolation did not qualify for this exact commit: {isolation}")
    report["isolation"] = isolation

    stop_reason = None
    for clients in CLIENT_LEVELS:
        if time.monotonic() >= deadline:
            stop_reason = "170-second overall watchdog expired"
            break
        name = f"preppydb-load-{run_id}-{clients}-{os.getpid()}"
        container_id = docker_config(name, str(clients), server, scripts, load=True)
        try:
            limits = inspect_limits(container_id)
            code, output = run_container(container_id, deadline)
        finally:
            subprocess.run(["docker", "rm", "--force", container_id], capture_output=True, check=False, timeout=10)
        stage = parse_report(output)
        stage["container_limits"] = limits
        report["stages"].append(stage)
        if stage.get("clients") != clients:
            raise RuntimeError(f"stage report mismatch: expected {clients}, got {stage.get('clients')}")
        if stage.get("status") == "completed" and code == 0:
            if not stage.get("integrity_after_recovery", {}).get("passed"):
                raise RuntimeError(f"integrity/recovery check failed at {clients} clients")
            report["highest_stable_clients"] = clients
            continue
        if (stage.get("status") == "aborted_at_threshold" and code == 2 and
                stage.get("integrity_after_recovery", {}).get("passed")):
            stop_reason = stage.get("abort_reason")
            break
        raise RuntimeError(f"load stage failed at {clients} clients: {stage}")

    report["status"] = "stopped_at_safety_threshold" if stop_reason else "completed"
    report["stop_reason"] = stop_reason
    report["total_elapsed_seconds"] = round(TOTAL_WATCHDOG_SECONDS - max(0, deadline - time.monotonic()), 3)
    report["load_test_started"] = bool(report["stages"])
    output_path = pathlib.Path(os.environ.get("RUNNER_TEMP", ".")) / "preppydb-first-stress-report.json"
    output_path.write_text(json.dumps(report, sort_keys=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(json.dumps({"test": "first-controlled-stress", "status": "failed", "load_test_started": False,
                          "error": str(error)}, sort_keys=True), file=sys.stderr)
        raise SystemExit(1)
