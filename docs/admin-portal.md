# Administration in the browser

After signing in as an administrator, open **Administration** (`/admin`). Routine
course setup and grading management no longer require shell access. The CLI remains
available for initial bootstrap, background services and recovery.

## Validate, review, confirm

Choose an action. Course TOML, exercise catalog TOML and student CSV/TOML can be
uploaded as UTF-8 files or pasted into a monospace text field. Choose one input
method per request. Files and pasted text have the same 1 MiB limit. Roster imports
require GitHub handles, not supplied numeric GitHub IDs; omitted students remain
registered. Worker tokens also support upload/paste and are hashed before storage.

1. Complete the fields and audit reason, then choose **Validate and preview**.
2. The operation page shows validation progress and diagnostics. Course and roster
   imports run their database import transactions and roll them back. Exercise
   validation resolves immutable source commits, validates definitions and cache
   requirements, and lists every archive/removal. Other actions check their inputs,
   current targets and applicable constraints without applying changes.
3. Only successful validation enables **Confirm and apply**. Review the resolved
   GitHub identities, dates, points, pinned revisions, scope and removals first.
4. Confirm within 15 minutes using the same browser session. The worker checks
   relevant records again before applying. Changed inputs require another validation.
5. The operation page shows the result, error diagnostics, or a CSV download.
   **Edit input and validate again** creates a fresh request; it does not alter the
   reviewed operation. Closing the browser does not stop work.

A confirmation is single-use and bound to its operation, administrator and session.
Changing a field requires a new validation. Revocation prevents further confirmations
and execution. Students cannot access these routes, create operations or view their
output. Reports and form values are HTML-escaped; worker tokens and database/App
credentials are not echoed into reports. All pages and downloads disable caching.

## Available actions

| Area | Portal actions |
| --- | --- |
| Courses | Create/update from TOML; inspect course records |
| Students | Import CSV/TOML; inspect enrolled GitHub identities |
| Exercises | Apply catalog; add; update; inspect definition and cache pins; private grading |
| Grades | Export CSV; override points; extend deadlines; regrade; select final submission; retry private grading |
| Operations | Inspect repositories, submissions, events, runs and task errors; retry failed tasks; synchronize; process queued work once |
| Access | Register/rotate/revoke workers; list/grant/revoke administrators |
| Maintenance | Inspect and apply pending migrations and refresh database grants |

The records links show up to 1,000 rows with the UUIDs needed by grading and retry
forms. Exports include separate final/provisional grade columns and escape spreadsheet
formula prefixes. Read-only actions show data during preview as well; exports produce
the downloadable file after confirmation.

Private grading and synchronization operate on eligible records when executed. Their
preview explains this scope; time-dependent eligibility and queued tasks may advance
while awaiting confirmation. A one-shot work action processes queued work; it does
not launch an unbounded daemon from a browser request. The normal control service
continues to process background tasks independently.

Exercise publication uses the reviewed commit SHAs even if remote branches move.
Ready cache seeds are pinned too. A missing cache is built after confirmation, before
publishing the revision; bulk catalog updates remain atomic. The operation page includes
cache keys and resulting artifact digests. For a new preparation attempt it also includes
the Kubernetes Job/namespace and up to 64 KiB of its retained build log. No cache code
or GitHub mutation runs during exercise validation.

## Deploy the administration worker

This repository implements the portal and worker. The service definition, runtime
credentials, mounts and peer-authentication mappings belong in `doctor-cluster-config`.
The existing repository task worker and grading executor are still required.

Apply migration `0008_admin_portal.sql` and current grants as the database owner
before starting the new web binary. Initial installation still needs an operator to
bootstrap the database and first administrator: an unauthenticated browser cannot
create its own administrator or initialize an absent database.

Run one supervised administration worker alongside the existing services:

```sh
gradingctl \
  --database-url-file /etc/grading/operator.url \
  --github-config /run/grading-github/github.json \
  --artifact-dir /var/lib/grading/artifacts \
  admin-work \
  --cache-config /etc/grading/executor.toml \
  --admin-database-url-file /etc/grading/admin.url \
  --migration-database-url-file /etc/grading/owner.url
```

Use service-specific paths. `--once` processes one validation or confirmed operation
and exits, which is useful for testing. Corresponding environment variables are
`GRADING_DATABASE_URL_FILE`, `GRADING_GITHUB_CONFIG`, `GRADING_CACHE_CONFIG`,
`GRADING_ADMIN_DATABASE_URL_FILE`, and `GRADING_MIGRATION_DATABASE_URL_FILE`.

The primary connection uses `grading_operator`. Administrator membership changes
use the separate `grading_admin` connection. Migrations use the separate owner
connection. Those two optional connections are only opened for their respective
actions; if absent or insufficient, validation fails with an actionable message and
confirmation stays unavailable. With native PostgreSQL peer authentication, configure
explicit identity mappings for the administration service; merely making connection
files readable does not grant access to another database role.

For cached exercises the worker must also run with UID 10004, see the executor's
staging PVC at the configured `staging_root`, and have the sandbox Kubernetes
permissions described in [exercise caching](exercises.md#optional-prepared-caches-schema-3).
It also needs the application artifact volume. None of these privileged credentials
or writable mounts are added to the public web service or student Pods. The web role
can insert requests and call a narrowly scoped confirmation function, but cannot
write validation plans, approval state or application results.

## Diagnostics and recovery

The operation UUID appears in the page and worker logs. Validation failures retain
readable parser/constraint messages plus stage/category/upstream status. Database and
HTTP transport errors use sanitized messages instead of raw connection details.
The task and repository pages expose existing per-repository failure diagnostics.
The administration worker sends a heartbeat every five seconds. A queued page warns
if no heartbeat was seen for 30 seconds; restarting a browser does not repair a stopped
worker. Operations remain queued until that worker is available.

On worker restart, interrupted validation can run again safely. An interrupted apply
is marked **uncertain** and is never automatically replayed: a database commit or
GitHub operation might already have completed before the process stopped. Inspect
current records and task state before validating another request. Ordinary application
failures similarly report that multi-step/external work may have partly completed.
Course/roster transactions and catalog publication retain their existing atomicity.

Requests are limited to four active operations and 60 submissions/hour per administrator.
Input, reports and exports are bounded. Operation history (including roster data) is
retained in PostgreSQL and included in its normal backups; no automatic retention purge
is implemented. Protect database backups as instructor data.

The automated suite covers validation rollback, failed previews, stale records, expiry,
double confirmation, session binding, revoked access, restart behavior, role permissions,
CSV output, uploads/pasted input, input limits, and escaped HTML. Live GitHub permissions,
peer mappings, Kubernetes preparation and storage quota behavior still require deployment
acceptance testing.
