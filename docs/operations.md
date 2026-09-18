# Operating the grading application

Production deployment is defined in
[doctor-cluster-config](https://github.com/TUM-DSE/doctor-cluster-config/blob/master/docs/grading-infrastructure.md).
That repository owns Kubernetes resources, nginx, certificates, secret provisioning,
storage, service timers, backups, and monitoring.
This repository supplies Rust binaries, migrations, and Nix-built container images.

## Native services and runtime credentials

The cluster installs the Rust binaries from the public HTTPS flake input. PostgreSQL,
web/tasks, migrations, synchronization and administration run as native services or
commands on Astrid. Mickey runs the executor as a systemd service; Kubernetes runs
student sandboxes and trusted grading controllers. The optional image outputs and PostgreSQL image entrypoint
remain in this repository, but are not used by the cluster deployment.
`nix/database-grants.sql` remains embedded in the CLI and used by database tests.

The dedicated PostgreSQL 18 service uses a private Unix socket and peer authentication,
with OS accounts mapped to the owner/web/operator/admin database roles. It has no TCP
listener or database passwords and is separate from Astrid's shared PostgreSQL instance.
Connection files under `/etc/grading/*.url` contain role names and socket paths.
The executor receives no database or App credentials. Sandboxes receive no credentials.

Application credentials are runtime files. The GitHub configuration JSON is:

```json
{
  "app_id": "APP_ID",
  "installation_id": 123,
  "client_id": "CLIENT_ID",
  "client_secret": "RUNTIME_SECRET",
  "private_key_file": "/run/grading-github/github.pem",
  "callback_url": "https://grading.dos.cit.tum.de/auth/callback"
}
```

The cluster's SOPS keys are `grading-github-config`, `grading-github-private-key`,
and `grading-github-webhook-secret`. The webhook secret must contain at least 32
bytes and match GitHub exactly. Keep credentials out of images and the Nix store.

Callback warnings contain fixed stage/reason codes and numeric HTTP statuses only.
`login_state` means the browser/state pair is missing, expired (five minutes), or
already consumed; start again from `/login` instead of refreshing the callback.
`login_state_store` indicates a database failure. At `token_response`, `status` is
the portal response and `upstream_status` is GitHub's response. GitHub may return an
OAuth error with HTTP 200. The allowlisted `reason` distinguishes
`incorrect_client_credentials`, `redirect_uri_mismatch`, `bad_verification_code`,
and other safe categories. Check client ID/secret pairing, registered callback URL,
or retry a fresh login accordingly. Unknown error strings, descriptions, response
bodies and all authentication values are discarded, never logged.

Migrations apply schema and grants using the owner OS/database account before
web/tasks start. Administration uses the real application binary directly on Astrid:

```sh
sudo -u grading-admin gradingctl --database-url-file /etc/grading/admin.url \
  admin grant --github-username martin-fink --reason 'Course administrator'
sudo -u grading-admin gradingctl --database-url-file /etc/grading/admin.url admin list
```

Use `grading-operator` and `/etc/grading/operator.url` for course/worker commands,
adding `--github-config /run/grading-github/github.json` when needed. The CLI records
`SUDO_USER`. Admin commands resolve GitHub handles without needing the App key.
Web/operator roles cannot grant admins or assume the admin role.

## GitHub App pilot configuration

Use organization installation coverage that includes newly created repositories.
Verify these API needs in a disposable organization before enrolling anyone:

- Repository administration/write for private creation, permission management,
  branch defaults, and organization policy inspection where required.
- Contents/write for exact tree seeding and source reads; workflows/write for
  provisioning protected workflow files; Actions/write for disabling
  workflows and setting read-only token defaults.
- Checks/write for official results, repository metadata/read, and the required
  collaborator/invitation APIs. Organization members/read may be needed for the
  organization policy/permission checks; confirm the minimal set empirically.
- Push webhook subscription, an exact HTTPS callback, and a high-entropy webhook
  secret. GitHub does not automatically redeliver failed webhooks.

The organization must have base repository access `none`. Keep general-purpose
self-hosted runner groups inaccessible to student repositories. The prototype
rejects team grants but cannot configure organization runner policy on your behalf.
Confirm that students have only write access and no organization role granting
additional repository rights. The private marker and repository numeric ID are
checked during recovery; moving or replacing a repository is not silently adopted.

GitHub Actions is disabled before students are invited and remains disabled after
seeding. Synchronization disables Actions on existing repositories too; run
`gradingctl sync` when upgrading. Students receive public-test feedback in the portal.
Keep organization secrets and self-hosted runner groups inaccessible as additional
protection. Provisioning starts from a pinned tree without private solution history.

## Executor and sandbox contract

Use the [exercise catalog workflow](exercises.md) with schema-3 private graders
and a separately published shared runner. Registration retains pinned grader
snapshots without building images. The executor accepts only `registered-v1` and
requires an approved runner digest in its registry policy.

The executor runs as a native systemd service on Mickey. Its runtime configuration
combines connection settings, registry policy and resource limits; see
[executor.toml.example](executor.toml.example). Restart it after policy changes.
The service uses systemd credentials for the worker token, CA trust and
namespace-scoped Kubernetes kubeconfig. These credentials remain outside grading Pods.

The executor stages source and grader snapshots on the sandbox PVC. Only the trusted
controller mounts the private grader and writable control channel. Student sandboxes
receive read-only source/input subpaths and ephemeral workspace volumes. The cluster
supplies gVisor, network denial, admission rules, restricted service accounts, PID and
resource limits, and log rotation. Validate these on the actual nodes.

The worker API is separate from browser routes. Astrid nginx uses the existing CA
for server TLS and restricts the endpoint to Mickey. The executor verifies that CA;
the application authenticates the worker bearer token against its stored hash.
The public proxy must never expose `/internal/` or the worker listener.

Register the shared-runner worker on Astrid:

```sh
sudo -u grading-operator gradingctl --database-url-file /etc/grading/operator.url \
  worker register --id mickey-1 --token-file /var/lib/grading/worker-token \
  --profile registered-v1 --cpu 10 --memory-gib 32 --storage-gib 64
```

Use `worker revoke --id mickey-1` under the same account/connection to revoke it.
See the cluster documentation for initial credential transfer and executor startup.

## Synchronization, metrics, and backups

`gradingctl work` continuously processes provisioning, snapshots, Check publishing,
and locks. `gradingctl sync` is a separate daily command with a PostgreSQL advisory
lock. The cluster defines its persistent timer and missed-run catch-up behavior.

The authenticated `/internal/metrics` endpoint exposes aggregate queue age,
pending/failed tasks, stale leases, pending locks, failed provisioning, and
integrity-failed runs. The infrastructure collects these for Telegraf/Prometheus.
Logs avoid request bodies, OAuth tokens, raw external error responses, and rosters.
Private CLI preview/export output should be handled as student data.

The cluster backs up the dedicated database, immutable artifacts, course checkouts,
and credentials to a restricted directory on the existing Borg-protected share.
The staging timestamp and successful encrypted Borg archive are separate checks.
Because accepted artifacts are immutable and have no automatic garbage collection,
copying them after the transactional database dump includes every referenced blob.

Restore into separate database/artifact storage, reapply grants, and verify every
referenced digest before starting an isolated restored application. `just test`
exercises local PostgreSQL dump/restore and artifact hashes; it does not prove the
external Borg archive can be recovered. Perform that restore drill before the pilot.

## Diagnosing failures

The web service, `gradingctl work`/`sync`, and executor write structured diagnostics
to stderr. Failures include a static `stage`, a safe `reason`, and an upstream HTTP
status when available. Background tasks include task IDs and attempt numbers;
grading executions include run IDs. Provisioning stages distinguish template
fetching, repository creation/seeding, permission checks, and invitations. Inspect
the service running `gradingctl work` when repository creation needs attention.

HTTP requests receive a generated `x-request-id` response header. At the default
`info` level, request spans correlate this ID with handler errors and the final
status/latency. Routes are logged as patterns, never actual URLs or query strings.
For more detail, set `RUST_LOG=grading_web=debug,gradingctl=debug,grading_executor=debug,grading_github=debug,grading_store=debug`
on the relevant service. HTTP/SQL dependency tracing is disabled to prevent
verbose library logs from exposing credentials or data.

Logs do not include raw error chains, upstream response bodies, tokens, cookies,
rosters, source files, or private grader output. Interactive CLI validation errors
and preview/export output remain operator-facing and may contain private data.
After deploying logging changes, restart the web service, background worker, and
executor. No database migration is needed for these logging changes.

## Source-fetch budgets

Student snapshots are preflighted before any blob download: at most 512 files and
8 MiB total, with a 120-second overall fetch deadline. Trusted template and grader
imports retain the platform limits. Keep course templates within the student
budget. The API client caches up to 512 blobs / 16 MiB of encoded data in memory;
identical repository/commit submissions reuse retained source artifacts. Deadline
lock tasks have a separate processing loop, so a slow snapshot does not occupy
the only lock worker. GitHub outages/rate limits can still delay remote locks;
receipt-time cutoff remains authoritative.
