# Pilot acceptance record

The prototype is implemented locally. No production deployment, GitHub repository
creation, invitation, cluster workload, or infrastructure migration is part of this work.

| Area | Local validation | External acceptance still required |
| --- | --- | --- |
| Build | Rust workspace, fmt, Clippy, locked dependencies, SQLx offline metadata | Target-platform OCI loading and host provisioning |
| Identity | Hashed sessions, rotation/expiry, CSRF, unknown users, numeric-ID enrollment, live admin revocation | Real App authorization callback and renamed test account |
| Role separation | SCRAM DB, runtime denial of admin changes/role assumption, owner-only migrations | Container mount isolation and DB network reachability |
| Repository lifecycle | Concurrent allocation, creation-response failure recovery fixture, marker rejection, rate-limit backoff | Real template seeding, invitation acceptance, org/team/base permissions, runner-group restrictions |
| Integrity | Changed/added/deleted/mode-changed files, traversal and symlink/submodule rejection | Representative instructor template, external Action/dependency pin review |
| Grading | Worker authentication/leases, result provenance and replay rejection, bounded points, sandbox Job shape | Actual gVisor resource/network/PID limits, OOM/timeouts, worker crash recovery, one representative course exercise |
| Closure | Receipt cutoff, immutable events/final selection, late receipts, extensions/overrides | Missed timer, delayed/missing webhook, force push before capture, GitHub outage, failed effective lock |
| Recovery | Isolated PostgreSQL dump/restore and artifact digest validation | Existing encrypted Borg destination restore, monitoring and backup alerts |

Use instructor-controlled accounts in a test organization for the live cases.
Review exact points and privacy with a small assignment before inviting students.
Validate the deployment and its backups before enrolling real students.

## Hardening regression coverage

Local tests now exercise supervisor descriptor protection and forged-result rejection,
concurrent stdout/stderr floods, command timeouts with closed output descriptors,
child descriptor limits, staging cleanup after Pod disappearance, source preflight
and blob caching, per-student API budgets, receipt deduplication and queue coalescing,
private-run retry authorization/provenance, final export classification, HTTP admission,
session retention, runtime database timeouts, and database/artifact restore.

The separate deployment acceptance cases and upgrade sequence are in
[hardening rollout](hardening.md). This local record does not certify the deployed
network policy, gVisor kernel semantics, storage isolation, or backup destination.
