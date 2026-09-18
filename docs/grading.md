# Submission and execution protocol

## Stored execution logs

The executor captures public student Jobs' stdout, stderr, exit codes, and execution
status before deleting Pods. Reports retain at most 256 KiB of logs per run,
including failed runs. They are content-addressed artifacts backed up with the
application volume; the dashboard links to the latest run's escaped, plain-text
log viewer. Completed historical reports are retained as well. Older runs created
before log capture was deployed may have no transcript.

Students can view only their own public execution output. Controller logs and
all post-deadline private grading output are restricted to current administrators.
The private run's log page links back to its public baseline's logs. Raw report
downloads enforce the same visibility rules. Scripts should execute student builds
through `grading-run` so compiler diagnostics are included in public Job logs;
controller stderr is retained for instructor diagnostics. Abruptly killed processes
may not flush their output, so a timeout can have only an execution-status message.

Deploy both `grading-web` and `grading-executor` for the viewer and capture support.
Schema migrations are not required. The executor also installs the Rustls ring
crypto provider before initializing HTTPS/Kubernetes clients.

## Repository creation

A CSRF-protected form checks the numeric GitHub account ID against enrollment and
assignment availability. An advisory lock and unique constraint allocate one
repository per enrollment and assignment. New repository names use
`template-name-student-id-uuid`, where the student ID is the roster's `student_id`.
Unsupported characters become hyphens; long descriptive components are shortened
to fit 100 characters while retaining the full UUID. Existing allocations keep
their names. Provisioning runs separately in `gradingctl work`.

The adapter creates a private, organization-owned repository with an unpredictable
provisioning marker. After a failed response it recovers only a matching private
repository owned by the expected organization and carrying that marker. Organization
base access must be `none`; repositories with team grants are rejected. A test
organization must confirm App installation coverage for newly created repositories.

The adapter disables Actions, copies the exact approved Git tree through the Git
Data API, verifies all file bytes and modes, sets read-only workflow-token defaults,
and enables Actions. It copies a single template tree, not template history or
other branches. Only then does it invite the immutable student account, resolving
its current username immediately before the API call. Invitation status is polled
by a delayed task. Repository lifecycle mutations use per-repository advisory locks.

## Receipts, snapshots, and deadlines

HMAC-verified webhooks are deduplicated by delivery UUID and raw-body digest. A
push on the configured branch records its SHA and service receipt time; commit
author and committer dates are never used. Explicit registration reads the branch
SHA from GitHub and records the server time **after** that read completes. Requests
still waiting for the GitHub read at the cutoff have not been accepted.

Every accepted receipt is retained. At most 32 source-fetch tasks per enrollment
are active; additional receipts remain durable and are scheduled as capacity frees.
This queue delay does not change eligibility. Source snapshots are fetched by exact
SHA and stored with SHA-256 digests. A force push before initial snapshot capture
can make the object unavailable; such failures require review and must not be
reported as successful retention. Once captured, source is independent of GitHub.

Each snapshot is limited to 64 MiB of decoded blobs, 8 MiB per file, and 10,000
files. Tree truncation, path traversal, file/directory collisions, and unsupported
Git modes are rejected. Symlink/submodule substitutions become integrity findings
before extraction. Paths and exact Git blob bytes are used, without checkout or
line-ending conversion. These limits target small assignment repositories; large
dependencies belong in the pinned grading image.

Daily synchronization records observations and marks unseen branch SHAs for review,
without creating historical receipt evidence. It freezes the latest eligible
registered submission, queues missing final work, downgrades access, verifies the
effective permission, and records the actual lock timestamp. Failed locks retain
their pending state. A webhook processed after closure, even with an earlier
receipt timestamp, is recorded for review rather than silently changing the frozen
selection. An audited event-selection override resolves that case.

## Leases and grading

The executor can lease grading work only. It has no PostgreSQL or GitHub credential.
Each lease includes a task ID, random lease token, run ID, commit SHA, immutable
revision digest, image digest, resources, and source digest. Leases last 120 seconds
and heartbeat every 30 seconds. Each worker has one live lease, and a repository
has at most one active grading lease. Queue acquisition uses transactional locking
and `SKIP LOCKED`; transactions end before external work starts.

Before fetching source, the executor validates the shared-runner revision against
its approved registry namespace, explicit runner digest allowlist and resource caps.
It verifies student/grader snapshot digests, commits and the integrity manifest.

The trusted controller mounts the private grader at `/grader` and orchestrates
isolated student Jobs through `/platform/grading-run`. Student Jobs receive no
private grader, expected answers, control channel or credentials. They run as a
separate non-root UID with gVisor, read-only source, ephemeral workspace volumes,
dropped capabilities and bounded resources. The cluster enforces network denial,
PID limits and log rotation. The instructor script interprets bounded student
output and returns a bounded final score. Timeout, OOM and infrastructure failures
produce no fabricated zero-point total.

Results are accepted only from the owning worker and current unexpired lease.
The web service independently verifies SHA, revision, image, resource profile,
score bounds and private-baseline provenance. Identical retries are idempotent; conflicting replays fail. A new
regrade creates a new run rather than overwriting a completed result. Official
GitHub Checks are published from a durable outbox for the exact commit.

## Retention and prototype limits

Application artifacts are content-addressed and fsynced before DB references commit.
There is no automatic deletion of accepted sources or historical results. Supply a
dedicated volume quota and monitoring; retention deletion requires a later audited
policy. Executor staging may remain after interrupted jobs; remove an old lease
directory only after confirming there are no Pods with its `grading-lease` label.

The prototype supports one configured GitHub App installation per deployment,
individual assignments, and the `latest-eligible-submission` policy. It rejects
alternate policies, mutable images, unsupported profiles, symlinks/submodules in
templates, and privileged/device workloads. Cross-organization installations,
closed-assignment reopening, automatic template rollout, and automatic admission
of unregistered SHAs are not implemented.

## Registered scripts and private decisions

Centrally registered exercises use [the script protocol](exercises.md). Each revision
pins template and grader commits, a shared runner digest, script commands and
resource caps. The executor independently checks the configured registry namespace
and resource limits. Integrity verification precedes all script/student execution.

Schema version 3 runs an instructor-owned controller with arbitrary test logic.
It uses a shared runner image and stages the pinned grader snapshot through
the lease-scoped API; only the controller mounts that source. No GitHub credentials
reach the executor or grading Pods.
It requests isolated student Jobs through a private per-lease file channel. Only
the trusted controller's final bounded score is accepted; student outputs are data.
The public script runs normally. A private script runs only when explicitly queued
by `gradingctl exercise private-grade` after the effective deadline, using the final
submission and a pinned completed public result. Extensions delay eligibility.

Private runs can change points or invalidate a score. Detailed reasons remain
instructor-only; student private reports expose scores and status.
Public baselines and run history remain immutable. Runtime web credentials cannot
schedule private runs. Template updates affect future repositories; `--existing`
rolls a grader revision into subsequent runs without changing existing student files.

Each `grading-run` request has its own timeout (30 seconds by default; override with
`--timeout-seconds N`, 1–3600, still bounded by the assignment deadline). Timeout,
output overflow, student OOM and abnormal capture termination return a nonzero
`exit_code` and a `failure` value (`timeout`, `output_limit`, `memory_limit`, or
`execution_failed`) to the trusted grader. They are failed test executions, not
successful output or automatic infrastructure retries. Graders must check the exit
code before comparing stdout. The overall grading deadline and controller failures
still produce an unresolved run requiring instructor attention.
