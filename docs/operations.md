# Operating the grading application

Production deployment is defined in
[doctor-cluster-config](https://github.com/TUM-DSE/doctor-cluster-config/blob/master/docs/grading-infrastructure.md).
That repository owns Kubernetes resources, nginx, certificates, secret provisioning,
storage, service timers, backups, monitoring, and legacy Autolab/Tango removal.
This repository supplies Rust binaries, migrations, and Nix-built container images.

## Images and runtime credentials

Build `web-image`, `cli-image`, `postgres-image`, and `executor-image` using the
root flake. `nix/images.nix` and `nix/postgres-entrypoint.sh` are image build inputs.
`nix/database-grants.sql` is embedded by the CLI; `nix/database-roles.sql` and the
grants file are also used by the database tests. These files remain here.

The cluster pins the application source over SSH, builds for x86_64 Linux, and
loads immutable image references into containerd. Deployment commands and source
updates are documented in the infrastructure repository, not a Compose stack.

PostgreSQL runs as UID 10002 with dedicated persistent storage. The image reads
`/run/secrets/postgres-password`, `grading_owner-password`, `grading_web-password`,
`grading_operator-password`, and `grading_admin-password` from that same directory.
All roles have distinct passwords and use SCRAM. Web/task containers (UID 10001)
receive only the web database URL; migration, operator, and admin Jobs receive
their respective role URLs. The executor (UID 10004) receives no database or App
credentials. Sandboxes run as UID 10003 without any credentials.

Application credentials are runtime files. The GitHub configuration JSON is:

```json
{
  "app_id": "APP_ID",
  "installation_id": 123,
  "client_id": "CLIENT_ID",
  "client_secret": "RUNTIME_SECRET",
  "private_key_file": "/run/secrets/github/github.pem",
  "callback_url": "https://grading.dos.cit.tum.de/auth/callback"
}
```

The cluster's SOPS keys are `grading-github-config`, `grading-github-private-key`,
and `grading-github-webhook-secret`. The webhook secret must contain at least 32
bytes and match GitHub exactly. Keep credentials out of images and the Nix store.

Migrations apply schema and grants using the owner credential before web/tasks
start. The runtime never migrates at startup. Administration is launched from
Astrid through the infrastructure wrapper, for example:

```sh
sudo gradingctl admin grant --github-username martin-fink --reason 'Course administrator'
sudo gradingctl admin list
```

That wrapper runs the application CLI in a short-lived Kubernetes Job and supplies
the host operator identity. Admin grant/revoke resolve current GitHub handles
before changing membership; failed lookups make no changes. Admin Jobs have GitHub
network access but no App secrets. Web and operator DB roles cannot grant admins
or assume the admin role. See the README for the underlying CLI commands.

## GitHub App pilot configuration

Use organization installation coverage that includes newly created repositories.
Verify these API needs in a disposable organization before enrolling anyone:

- Repository administration/write for private creation, permission management,
  branch defaults, and organization policy inspection where required.
- Contents/write for exact tree seeding and source reads; workflows/write for
  provisioning protected workflow files; Actions/write for disabling/enabling
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

Public Actions templates must run the same test cases, use pinned external Actions,
and have no official-grade, App, manifest, or cluster credentials. Provisioning
starts from a pinned tree without carrying private solution history.

## Executor and sandbox contract

The infrastructure runs the executor on Mickey. It generates
`/etc/grading/executor.toml` on Astrid from
`services.grading-infrastructure.executor.profiles` and mounts it into the executor
Pod with runtime credentials. [executor.toml.example](executor.toml.example)
documents the application's configuration format; it is not a deployable profile.
Empty approved profiles prevent the cluster wrapper from starting the executor.

Profiles independently approve image digests, commands, timeouts, and resource
caps. The executor and student Jobs share a dedicated staging PVC. Student mounts
are read-only per-lease subpaths; credentials never belong on that volume. The
cluster configures the `gvisor` RuntimeClass, namespace policies, admission rules,
restricted ServiceAccount, PID/resource limits, and log rotation. Test enforcement
on the actual nodes before running student submissions.

The worker API is separate from browser routes. The cluster terminates mTLS using
the existing internal CA and restricts the endpoint to the executor network.
The application verifies a bearer token whose hash is stored in the database.
Register and start it from Astrid using the infrastructure wrapper:

```sh
sudo gradingctl register-worker --profile functional-v1 --cpu 10 --memory-gib 32 --storage-gib 64
sudo gradingctl executor
```

Use a real profile matching the declarative allowlist. The worker ID is `mickey-1`;
`sudo gradingctl operator worker revoke --id mickey-1` rejects further leases and
reports. The public proxy must never expose `/internal/` or the worker listener.

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
