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

For legacy profiles, before fetching/extracting source, the executor checks its local, instructor-owned
allowlist of profile commands, image digests, time limits, and resource caps. It then
verifies the source digest, SHA, and private manifest. No server-supplied command is
executed. The local execution profile is a separate trust boundary from course data.

For each public functional test, a fresh gVisor Job receives read-only source and
stdin files via per-lease PVC subpaths, plus ephemeral workspace/tmp volumes. The
Job has no secrets, service-account token, host mounts, or network access. It runs
as a separate non-root UID with all capabilities dropped, no privilege escalation,
a read-only root filesystem, and bounded CPU/memory/storage/time. The namespace's
NetworkPolicy, node runtime, PID limit, and log rotation must be enforced by the
cluster; the application cannot establish those host properties by itself.

The trusted executor uses the Pod's exit status and compares bounded stdout against
the approved expected output. Stderr is redirected to bounded ephemeral storage,
not treated as a score. Each test starts fresh, including any profile build step.
This avoids trusting a student-written result file or a `passed` marker. It is a
functional-test harness, not a universal secure harness for in-process unit tests.
Completed test failures score normally; timeout, OOM/infrastructure failure, and
integrity failure do not produce a fabricated zero-point total.

Results are accepted only from the owning worker and current unexpired lease.
The web service independently verifies SHA, revision, image, resource profile,
test completeness/uniqueness, and bounds, then calculates integer points from the
approved cases. Identical retries are idempotent; conflicting replays fail. A new
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
pins template and grader commits, student/grader image digests, script commands and
resource caps. The executor independently checks the configured registry namespace
and resource limits. Integrity verification precedes all script/student execution.

Schema versions 2 and 3 run an instructor-owned controller with arbitrary test logic.
Version 3 uses a shared runner image and stages the pinned grader snapshot through
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
