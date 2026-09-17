# Submission and execution protocol

## Repository creation

A CSRF-protected form checks the numeric GitHub account ID against enrollment and
assignment availability. An advisory lock and unique constraint allocate one
opaque repository name. Provisioning runs separately in `gradingctl work`.

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

Before fetching/extracting source, the executor checks its local, instructor-owned
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
