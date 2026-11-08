#!/usr/bin/env python3
"""Run isolation qualification, then a bounded synthetic client ladder in Docker."""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import subprocess
import sys
import time
from datetime import datetime, timezone
from typing import Any, Callable


IMAGE = "python:3.12-slim-bookworm"
CLIENT_LEVELS = (1, 10, 25, 50, 100)
TOTAL_WATCHDOG_SECONDS = 170
STAGE_DURATION_SECONDS = 15
DOCKER_PULL_TIMEOUT_SECONDS = 90
DOCKER_CREATE_TIMEOUT_SECONDS = 20
DOCKER_INSPECT_TIMEOUT_SECONDS = 10
DOCKER_START_TIMEOUT_SECONDS = 15
ISOLATION_WAIT_TIMEOUT_SECONDS = 35
STAGE_WAIT_TIMEOUT_SECONDS = 55
DOCKER_LOGS_TIMEOUT_SECONDS = 5
DOCKER_CLEANUP_TIMEOUT_SECONDS = 10
DOCUMENTS = 500
PAYLOAD_BYTES = 512
MEMORY_BYTES = 2 * 1024**3
PIDS_LIMIT = 512
DATA_BYTES = 512 * 1024**2
ENABLE_PHRASE = "I_APPROVE_BOUNDED_PREPPYDB_STRESS"
TEST_STARTED_AT: float | None = None


def log_event(event: str, **details: Any) -> None:
    elapsed = None if TEST_STARTED_AT is None else round(time.monotonic() - TEST_STARTED_AT, 3)
    payload = {
        "timestamp_utc": datetime.now(timezone.utc).isoformat(),
        "elapsed_seconds": elapsed,
        "event": event,
        **details,
    }
    print("STRESS_EVENT " + json.dumps(payload, sort_keys=True), flush=True)


def remaining_seconds(deadline: float, phase: str) -> float:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError(f"overall watchdog expired before {phase}")
    return remaining


def run_docker(args: list[str], *, timeout: float, phase: str) -> subprocess.CompletedProcess[str]:
    log_event("docker_command_started", phase=phase, command=args, timeout_seconds=round(timeout, 3))
    started = time.monotonic()
    try:
        result = subprocess.run(args, capture_output=True, text=True, check=False, timeout=max(0.1, timeout))
    except subprocess.TimeoutExpired as error:
        stdout = error.stdout or ""
        stderr = error.stderr or ""
        if isinstance(stdout, bytes):
            stdout = stdout.decode("utf-8", errors="replace")
        if isinstance(stderr, bytes):
            stderr = stderr.decode("utf-8", errors="replace")
        log_event("docker_command_timed_out", phase=phase, command=args,
                  elapsed_seconds=round(time.monotonic() - started, 3), stdout_tail=stdout[-2000:],
                  stderr_tail=stderr[-2000:])
        raise TimeoutError(f"{phase} timed out after {timeout:.1f}s") from error
    log_event("docker_command_finished", phase=phase, return_code=result.returncode,
              elapsed_seconds=round(time.monotonic() - started, 3),
              stdout_tail=result.stdout[-1000:], stderr_tail=result.stderr[-1000:])
    return result


def docker_config(run_name: str, stage: str, server: pathlib.Path, scripts: pathlib.Path, load: bool,
                  *, phase: str) -> str:
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
    else:
        args += ["--env", "PREPPYDB_DIAGNOSTICS=1"]
        for key in ("PREPPYDB_RUSTC", "PREPPYDB_CARGO", "PREPPYDB_DOCKER_CGROUP"):
            if os.environ.get(key):
                args += ["--env", f"{key}={os.environ[key]}"]
    try:
        result = run_docker(args + [IMAGE] + command, timeout=DOCKER_CREATE_TIMEOUT_SECONDS,
                            phase=f"{phase}-container-create")
    except Exception:
        # The daemon may have created the named container even if its CLI timed out.
        try:
            cleanup_container(run_name, phase)
        except Exception as cleanup_error:
            log_event("container_cleanup_failed", phase=phase, container_id=run_name, error=str(cleanup_error))
        raise
    if result.returncode != 0:
        try:
            cleanup_container(run_name, phase)
        except Exception as cleanup_error:
            log_event("container_cleanup_failed", phase=phase, container_id=run_name, error=str(cleanup_error))
        raise RuntimeError(f"docker create failed with exit code {result.returncode}: {result.stderr[-2000:]}")
    container_id = result.stdout.strip()
    if not container_id:
        raise RuntimeError(f"{phase}: docker create returned an empty container ID")
    return container_id


def inspect_limits(container_id: str) -> dict[str, Any]:
    result = run_docker(["docker", "inspect", container_id], timeout=DOCKER_INSPECT_TIMEOUT_SECONDS,
                        phase="container-limit-inspect")
    if result.returncode != 0:
        raise RuntimeError(f"docker inspect failed with exit code {result.returncode}: {result.stderr[-2000:]}")
    info = json.loads(result.stdout)[0]
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


def container_diagnostics(container_id: str) -> dict[str, Any]:
    try:
        state_result = run_docker(["docker", "inspect", "--format", "{{json .State}}", container_id],
                                  timeout=DOCKER_INSPECT_TIMEOUT_SECONDS, phase="container-state-diagnostic")
        state = state_result.stdout.strip()[-2000:]
        state_error = state_result.stderr.strip()[-1000:] or None
    except Exception as error:
        state, state_error = "", str(error)
    try:
        logs_result = run_docker(["docker", "logs", "--tail", "100", container_id],
                                 timeout=DOCKER_LOGS_TIMEOUT_SECONDS, phase="container-log-diagnostic")
        logs = logs_result.stdout[-8000:]
        log_error = logs_result.stderr[-1000:] or None
    except Exception as error:
        logs, log_error = "", str(error)
    return {
        "state": state,
        "state_error": state_error,
        "logs": logs,
        "log_error": log_error,
    }


def run_container(container_id: str, deadline: float, *, phase: str, max_wait_seconds: float,
                  on_started: Callable[[], None] | None = None) -> tuple[int, str]:
    start_timeout = min(DOCKER_START_TIMEOUT_SECONDS, remaining_seconds(deadline, f"{phase} container start"))
    start_result = run_docker(["docker", "start", container_id], timeout=start_timeout, phase=f"{phase}-container-start")
    if start_result.returncode != 0:
        diagnostics = container_diagnostics(container_id)
        raise RuntimeError(f"{phase}: docker start failed ({start_result.returncode}); diagnostics={diagnostics}")
    if on_started is not None:
        on_started()

    wait_timeout = min(max_wait_seconds, remaining_seconds(deadline, f"{phase} container completion"))
    log_event("container_wait_started", phase=phase, container_id=container_id, timeout_seconds=round(wait_timeout, 3))
    started = time.monotonic()
    try:
        wait_result = run_docker(["docker", "wait", container_id], timeout=wait_timeout, phase=f"{phase}-container-wait")
    except TimeoutError as error:
        diagnostics = container_diagnostics(container_id)
        raise TimeoutError(
            f"{phase}: container did not exit within {wait_timeout:.1f}s; "
            f"elapsed={time.monotonic() - started:.3f}s; diagnostics={json.dumps(diagnostics, sort_keys=True)}"
        ) from error
    if wait_result.returncode != 0:
        diagnostics = container_diagnostics(container_id)
        raise RuntimeError(f"{phase}: docker wait failed ({wait_result.returncode}); diagnostics={diagnostics}")
    try:
        container_exit_code = int(wait_result.stdout.strip())
    except ValueError as error:
        diagnostics = container_diagnostics(container_id)
        raise RuntimeError(f"{phase}: docker wait returned an invalid exit code; diagnostics={diagnostics}") from error
    logs_result = run_docker(["docker", "logs", container_id], timeout=DOCKER_LOGS_TIMEOUT_SECONDS,
                             phase=f"{phase}-container-logs")
    if logs_result.returncode != 0:
        raise RuntimeError(f"{phase}: docker logs failed ({logs_result.returncode}): {logs_result.stderr[-2000:]}")
    return container_exit_code, logs_result.stdout


def cleanup_container(container_id: str, phase: str) -> None:
    try:
        result = run_docker(["docker", "rm", "--force", container_id], timeout=DOCKER_CLEANUP_TIMEOUT_SECONDS,
                            phase=f"{phase}-container-cleanup")
    except TimeoutError as error:
        log_event("container_cleanup_failed", phase=phase, container_id=container_id, error=str(error))
        raise
    if result.returncode != 0:
        # Docker reports a missing container as an error; that is already cleaned up.
        if "no such container" not in result.stderr.lower():
            raise RuntimeError(f"{phase}: container cleanup failed: {result.stderr[-2000:]}")


def parse_report(output: str) -> dict[str, Any]:
    reports = []
    for line in output.splitlines():
        try:
            candidate = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(candidate, dict) and ("status" in candidate) and ("test" in candidate or "clients" in candidate):
            reports.append(candidate)
    if not reports:
        raise RuntimeError(f"container did not emit a recognizable JSON report; output_tail={output[-4000:]}")
    return reports[-1]


def main() -> int:
    global TEST_STARTED_AT
    TEST_STARTED_AT = time.monotonic()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--isolation-only", action="store_true",
                        help="qualify the capped container and exit before creating any load container")
    args = parser.parse_args()
    repo = pathlib.Path.cwd()
    server = repo / "target" / "release" / "preppydb-server"
    scripts = repo / "scripts"
    deadline = TEST_STARTED_AT + TOTAL_WATCHDOG_SECONDS
    run_id = os.environ.get("GITHUB_RUN_ID", str(os.getpid()))
    report: dict[str, Any] = {
        "test": "isolation-only" if args.isolation_only else "first-controlled-stress",
        "commit": os.environ.get("PREPPYDB_COMMIT", "unknown"),
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
        "load_test_started": False,
    }
    report_path = pathlib.Path(os.environ.get("RUNNER_TEMP", ".")) / (
        "preppydb-isolation-report.json" if args.isolation_only else "preppydb-first-stress-report.json"
    )
    phase = "preflight"

    def save_report() -> None:
        report_path.write_text(json.dumps(report, sort_keys=True, indent=2) + "\n", encoding="utf-8")

    try:
        if not server.is_file() or not os.access(server, os.X_OK):
            raise RuntimeError("release server binary is missing")
        if not os.environ.get("PREPPYDB_COMMIT"):
            raise RuntimeError("PREPPYDB_COMMIT is required")
        if not args.isolation_only and os.environ.get("PREPPYDB_ENABLE_STRESS") != ENABLE_PHRASE:
            raise RuntimeError("bounded stress requires the explicitly approved manual workflow input")

        phase = "image-pull"
        pull_timeout = min(DOCKER_PULL_TIMEOUT_SECONDS, remaining_seconds(deadline, phase))
        pull_result = run_docker(["docker", "pull", IMAGE], timeout=pull_timeout, phase=phase)
        if pull_result.returncode != 0:
            raise RuntimeError(f"docker image pull failed ({pull_result.returncode}): {pull_result.stderr[-2000:]}")

        phase = "repeat-isolation"
        check_name = f"preppydb-check-{run_id}-{os.getpid()}"
        check_id = docker_config(check_name, "", server, scripts, load=False, phase=phase)
        try:
            report["isolation_container_limits"] = inspect_limits(check_id)
            check_code, check_output = run_container(
                check_id, deadline, phase=phase, max_wait_seconds=ISOLATION_WAIT_TIMEOUT_SECONDS
            )
        finally:
            cleanup_container(check_id, phase)
        isolation = parse_report(check_output)
        if (check_code != 0 or isolation.get("status") != "passed" or
                isolation.get("test") != "isolation-only" or isolation.get("load_test_started") is not False):
            raise RuntimeError(f"isolation did not qualify for this exact commit: {isolation}")
        report["isolation"] = isolation
        if args.isolation_only:
            report["status"] = "passed"
            report["checks"] = isolation.get("checks", {})
            report["load_test_started"] = False
            report["total_elapsed_seconds"] = round(time.monotonic() - TEST_STARTED_AT, 3)
            config_path = pathlib.Path(os.environ.get("RUNNER_TEMP", ".")) / "preppydb-docker-config.json"
            config_path.write_text(
                json.dumps(report["isolation_container_limits"], sort_keys=True, indent=2) + "\n",
                encoding="utf-8",
            )
            save_report()
            log_event("run_finished", status="passed", load_test_started=False, stages=0)
            print("STRESS_REPORT " + json.dumps(report, sort_keys=True), flush=True)
            return 0

        stop_reason = None
        for clients in CLIENT_LEVELS:
            if time.monotonic() >= deadline:
                stop_reason = "170-second overall watchdog expired"
                break
            phase = f"load-{clients}-clients"
            name = f"preppydb-load-{run_id}-{clients}-{os.getpid()}"
            container_id = docker_config(name, str(clients), server, scripts, load=True, phase=phase)
            try:
                limits = inspect_limits(container_id)
                code, output = run_container(
                    container_id, deadline, phase=phase, max_wait_seconds=STAGE_WAIT_TIMEOUT_SECONDS,
                    on_started=lambda: report.__setitem__("load_test_started", True),
                )
            finally:
                cleanup_container(container_id, phase)
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
        report["total_elapsed_seconds"] = round(time.monotonic() - TEST_STARTED_AT, 3)
        save_report()
        log_event("run_finished", status=report["status"], load_test_started=report["load_test_started"],
                  stages=len(report["stages"]))
        print("STRESS_REPORT " + json.dumps(report, sort_keys=True), flush=True)
        return 0
    except Exception as error:
        report["status"] = "failed"
        report["failure_phase"] = phase
        report["error"] = str(error)
        report["total_elapsed_seconds"] = round(time.monotonic() - TEST_STARTED_AT, 3)
        report["load_test_started"] = bool(report["load_test_started"] or report["stages"])
        try:
            save_report()
        except Exception as report_error:
            log_event("report_write_failed", error=str(report_error))
        log_event("run_failed", phase=phase, error=str(error), load_test_started=report["load_test_started"])
        print("STRESS_REPORT " + json.dumps(report, sort_keys=True), file=sys.stderr, flush=True)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
