# Pilot acceptance record

The prototype is implemented locally. No production deployment, GitHub repository
creation, invitation, cluster workload, or Autolab migration is part of this work.

| Area | Local validation | External acceptance still required |
| --- | --- | --- |
| Build | Rust workspace, fmt, Clippy, locked dependencies, SQLx offline metadata | Target-platform OCI loading and host provisioning |
| Identity | Hashed sessions, rotation/expiry, CSRF, unknown users, numeric-ID enrollment, live admin revocation | Real App authorization callback and renamed test account |
| Role separation | SCRAM DB, runtime denial of admin changes/role assumption, owner-only migrations | Container mount isolation and DB network reachability |
| Repository lifecycle | Concurrent allocation, creation-response failure recovery fixture, marker rejection, rate-limit backoff | Real template seeding, invitation acceptance, org/team/base permissions, runner-group restrictions |
| Integrity | Changed/added/deleted/mode-changed files, traversal and symlink/submodule rejection | Representative instructor template, external Action/dependency pin review |
| Grading | Worker authentication/leases, result provenance and replay rejection, bounded points, sandbox Job shape | Actual gVisor resource/network/PID limits, OOM/timeouts, worker crash recovery, one real LLVM/simulation exercise |
| Closure | Receipt cutoff, immutable events/final selection, late receipts, extensions/overrides | Missed timer, delayed/missing webhook, force push before capture, GitHub outage, failed effective lock |
| Recovery | Isolated PostgreSQL dump/restore and artifact digest validation | Existing encrypted Borg destination restore, monitoring and backup alerts |

Use instructor-controlled accounts in a test organization for the live cases.
Review exact points and privacy with a small assignment before inviting students.
Retain Autolab/Tango, existing grades, backups, and rollback deployment until the
pilot and required historical-data preservation are independently verified.
