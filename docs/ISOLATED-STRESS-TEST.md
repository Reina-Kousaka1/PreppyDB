# Isolated stress-test infrastructure

## Current state

The GitHub Actions workflow has a manually triggered isolation qualification job.
It runs only for a manual dispatch on `arena/146caad2-preppydb`. Push and pull
request events skip this job. The job builds the release server and performs a
short container-isolation self-test; it does **not** start PreppyDB with a data
load, and it does not invoke the load generator.

The prepared generator at `scripts/stress-load.py` requires the exact source
commit, a matching qualification value, and an explicit approval phrase. The
isolation-only mode supplies none of the approval values. The bounded-stress
mode is separately manual, requires an unchecked-by-default approval input and
the successful isolation-only run ID for the same commit, and repeats the
isolation probe before starting load. A regular CI pass alone never enables load.

## Manual isolation qualification

1. Open the repository's **Actions** tab and select **CI**.
2. Choose **Run workflow**, select `arena/146caad2-preppydb`, and select
   `isolation-only`.
3. Start the workflow manually. The regular build, benchmark, and Docker jobs
   are skipped; only the bounded isolation qualification runs.
4. Wait for **Stress test isolation qualification (no load)** to complete.
5. Download the `preppydb-stress-isolation-<commit>-<run>` artifact and review
   both the JSON isolation report and the Docker configuration report.

The existing `CI` workflow already supports `workflow_dispatch` on the
default branch. Select the PR branch so the run uses that branch's workflow
version and the branch guard allows only its isolation-only job to start.
The PR itself must remain open and unmerged. Push workflow changes only when the
resulting PR preview is explicitly authorized.

## First controlled load (separate approval)

The workflow has a separate `bounded-stress` mode and an unchecked approval
box. Leaving the box unchecked starts no load. When deliberately approved in a
later dispatch, the job verifies the successful isolation-only report for that
exact commit, then reruns the isolation checker for the same commit.
Only after that check passes does it run the serial client ladder. The current
ladder is 1, 10, 25, 50, and 100 clients; 250 is excluded from this first test.
Each level gets a fresh, independently capped container and synthetic database,
with a 15-second measurement window and a 170-second overall watchdog. A
threshold stop ends the ladder and still triggers a document-integrity and
restart/recovery check for that stage. The approval box must remain unchecked
for an isolation-only dispatch.

## Isolation contract

The job uses a disposable GitHub-hosted `ubuntu-22.04` runner. Before creating
the container it records the runner and Docker/cgroup versions, and requires at
least 4 GiB of available host memory and 5 GiB free in the temporary filesystem.
The release build occurs on that disposable runner; no local Windows machine or
Raspberry Pi is involved.

The test container is created with:

| Resource or boundary | Limit |
| --- | ---: |
| Memory | 2 GiB, with swap disabled for the container |
| CPU | 2 cores |
| Processes and threads | 512 tasks through the cgroup pids controller |
| Temporary database and test data | 512 MiB tmpfs |
| Other temporary files | 64 MiB tmpfs |
| Shared memory | 16 MiB |
| Network | Container network disabled; API binds to loopback only |
| Root filesystem | Read-only |
| File descriptors | 512 |
| Container watchdog | 180 seconds, followed by forced container removal |
| Actions job timeout | 20 minutes |

The checker fails closed unless it sees cgroup v2 and the effective
`memory.max`, `cpu.max`, and `pids.max` values match the contract. It verifies
that a small touched allocation is charged to the memory cgroup, briefly runs
four CPU workers and requires the kernel throttle counter to increase, observes
temporary children in `pids.current`, checks the tmpfs size, writes and removes
a small synthetic file, and verifies that the watchdog stops its child. The
container also runs the release binary's bounded `--help` check. None of these
probes deliberately fills memory or reaches a resource limit.

The host inspects Docker's effective configuration before starting the
container. Any mismatch, unsupported cgroup setup, insufficient host reserve,
missing watchdog behavior, or incomplete report fails the job before any load
test could begin. Cleanup removes the container on normal exit, failure, or
cancellation; GitHub destroys the hosted runner after the job.

## Prepared load generator

`scripts/stress-load.py` supports client levels 1, 10, 25, 50 and 100; read,
write, mixed and indexed-query operations; deterministic synthetic documents;
request rate, p50/p95/p99 latency, error-rate, cgroup memory, CPU throttling,
task counts and temporary-disk measurements. The first controlled ladder uses
mixed operations against 500 synthetic documents with 512-byte payloads. After
each stage it verifies every document, restarts the server on the same temporary
database, and verifies the documents again. Its safety stops include:

- 80% of the 2 GiB memory cap.
- 80% of the 512-task cgroup cap.
- Less than 128 MiB free in the bounded data tmpfs.
- p95 latency over one second.
- Error rate over 1%.
- No completed request for ten seconds.
- CPU above 95% of the two-core quota for ten seconds.
- The configured per-run duration or outer watchdog.

The generator refuses to start unless the source and qualification commits
match and the manual workflow supplies its approval gate. The host-side ladder
runner also checks the approval input, inspects each Docker configuration before
starting it, and refuses to continue if the initial isolation-only probe fails.
The normal isolation-only path never supplies the approval gate or invokes the
ladder. Qualification proves containment for that runner execution; it does not
establish production capacity or justify an unbounded soak.

## Artifacts and interpretation

The isolation artifact records the exact commit, observed cgroup version and
controllers, configured and effective memory/CPU/pids limits, CPU throttling
evidence, temporary filesystem size, watchdog outcome, and release-binary
smoke result. It explicitly records `load_test_started: false`. A failed or
missing artifact means isolation was not qualified; no load conclusions may be
drawn.

The future load report is intended to distinguish the highest completed client
step from a threshold-triggered stop. It must not label a transient request-rate
peak as a stable limit. No benchmark values are claimed by this infrastructure
change.
