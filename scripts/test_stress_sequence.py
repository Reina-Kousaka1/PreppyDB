from __future__ import annotations

import importlib.util
import json
import os
import pathlib
import subprocess
import sys
import time
import unittest
from contextlib import redirect_stderr
from io import StringIO
from unittest import mock


SCRIPT = pathlib.Path(__file__).with_name("stress-sequence.py")
SPEC = importlib.util.spec_from_file_location("stress_sequence", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
stress_sequence = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = stress_sequence
SPEC.loader.exec_module(stress_sequence)
CHECKER_PATH = SCRIPT.with_name("stress-isolation-check.py")
CHECKER_SPEC = importlib.util.spec_from_file_location("stress_isolation_check", CHECKER_PATH)
assert CHECKER_SPEC is not None and CHECKER_SPEC.loader is not None
stress_isolation_check = importlib.util.module_from_spec(CHECKER_SPEC)
sys.modules[CHECKER_SPEC.name] = stress_isolation_check
CHECKER_SPEC.loader.exec_module(stress_isolation_check)


class ContainerLifecycleTests(unittest.TestCase):
    def test_create_timeout_attempts_cleanup_by_container_name(self) -> None:
        with mock.patch.dict(os.environ, {"PREPPYDB_COMMIT": "test-commit"}, clear=False):
            with mock.patch.object(stress_sequence, "run_docker", side_effect=TimeoutError("create timeout")):
                with mock.patch.object(stress_sequence, "cleanup_container") as cleanup:
                    with self.assertRaisesRegex(TimeoutError, "create timeout"):
                        stress_sequence.docker_config(
                            "test-container", "", pathlib.Path("server"), pathlib.Path("scripts"),
                            load=False, phase="repeat-isolation",
                        )

        cleanup.assert_called_once_with("test-container", "repeat-isolation")

    def test_isolation_only_phase_logging_stays_silent_by_default(self) -> None:
        output = StringIO()
        with mock.patch.dict(os.environ, {"PREPPYDB_DIAGNOSTICS": ""}, clear=False):
            with redirect_stderr(output):
                stress_isolation_check.phase_event("startup", time.monotonic())
        self.assertEqual(output.getvalue(), "")

    def test_repeat_qualification_phase_logging_has_timestamp(self) -> None:
        output = StringIO()
        with mock.patch.dict(os.environ, {"PREPPYDB_DIAGNOSTICS": "1"}, clear=False):
            with redirect_stderr(output):
                stress_isolation_check.phase_event("cpu-throttling", time.monotonic())
        event = json.loads(output.getvalue().removeprefix("ISOLATION_PHASE "))
        self.assertEqual(event["phase"], "cpu-throttling")
        self.assertIn("timestamp_utc", event)

    def test_detached_start_waits_then_reads_logs(self) -> None:
        results = iter(
            [
                subprocess.CompletedProcess([], 0, "container-id\n", ""),
                subprocess.CompletedProcess([], 0, "0\n", ""),
                subprocess.CompletedProcess([], 0, 'ISOLATION_PHASE {"phase":"complete"}\n{"status":"passed","test":"isolation-only"}\n', ""),
            ]
        )
        with mock.patch.object(stress_sequence, "run_docker", side_effect=lambda *args, **kwargs: next(results)) as run:
            code, output = stress_sequence.run_container(
                "container-id", time.monotonic() + 30, phase="repeat-isolation", max_wait_seconds=10
            )

        self.assertEqual(code, 0)
        self.assertIn('"status":"passed"', output)
        self.assertEqual(run.call_args_list[0].args[0], ["docker", "start", "container-id"])
        self.assertEqual(run.call_args_list[1].args[0], ["docker", "wait", "container-id"])
        self.assertEqual(run.call_args_list[2].args[0], ["docker", "logs", "container-id"])
        self.assertEqual(stress_sequence.parse_report(output)["test"], "isolation-only")

    def test_wait_timeout_preserves_container_state_and_logs(self) -> None:
        results = iter(
            [
                subprocess.CompletedProcess([], 0, "container-id\n", ""),
                TimeoutError("repeat-isolation-container-wait timed out after 2.0s"),
                subprocess.CompletedProcess([], 0, '{"Status":"running"}\n', ""),
                subprocess.CompletedProcess([], 0, "ISOLATION_PHASE {\"phase\":\"cpu-throttling\"}\n", ""),
            ]
        )

        def fake_run(*args, **kwargs):
            result = next(results)
            if isinstance(result, Exception):
                raise result
            return result

        with mock.patch.object(stress_sequence, "run_docker", side_effect=fake_run):
            with self.assertRaisesRegex(TimeoutError, "cpu-throttling") as caught:
                stress_sequence.run_container(
                    "container-id", time.monotonic() + 30, phase="repeat-isolation", max_wait_seconds=2
                )

        self.assertIn("running", str(caught.exception))
        self.assertIn("cpu-throttling", str(caught.exception))

    def test_cleanup_has_force_remove_and_a_finite_timeout(self) -> None:
        with mock.patch.object(stress_sequence, "run_docker", return_value=subprocess.CompletedProcess([], 0, "container-id\n", "")) as run:
            stress_sequence.cleanup_container("container-id", "repeat-isolation")

        self.assertEqual(run.call_args.args[0], ["docker", "rm", "--force", "container-id"])
        self.assertEqual(run.call_args.kwargs["timeout"], stress_sequence.DOCKER_CLEANUP_TIMEOUT_SECONDS)

    def test_command_timeout_has_a_phase_specific_error(self) -> None:
        timeout = subprocess.TimeoutExpired(["docker", "wait", "container-id"], 2, output=b"partial", stderr=b"busy")
        with mock.patch.object(stress_sequence.subprocess, "run", side_effect=timeout):
            with self.assertRaisesRegex(TimeoutError, "container-wait timed out after 2.0s"):
                stress_sequence.run_docker(["docker", "wait", "container-id"], timeout=2, phase="container-wait")


if __name__ == "__main__":
    unittest.main()
