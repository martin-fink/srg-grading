# Operating the grading application

Production deployment is defined in
[doctor-cluster-config](https://github.com/TUM-DSE/doctor-cluster-config/blob/master/docs/grading-infrastructure.md).
That repository owns Kubernetes resources, nginx, certificates, secret provisioning,
storage, service timers, backups, monitoring, and legacy Autolab/Tango removal.
This repository supplies Rust binaries, migrations, and Nix-built container images.

## Native services and runtime credentials

The cluster installs the Rust binaries from the public HTTPS flake input. PostgreSQL,
web/tasks, migrations, synchronization and administration run as native services or
commands on Astrid. Mickey runs the executor as a systemd service; Kubernetes runs
only student submissions. The optional image outputs and PostgreSQL image entrypoint
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

For the new central registration workflow, see [exercises](exercises.md). It uses
`gradingctl exercise add/update`, a dedicated Nix builder, and one `[registry]`
executor policy for the `registered-v1` worker profile. The cluster must pin the application revision supporting that workflow and
provide the dedicated builder environment. The profile-file workflow below remains
available for legacy exercises. Apply migration 0002 and upgrade all processes
before registering exercises with the new application version.

The infrastructure runs the executor as a native systemd service on Mickey. Install
reviewed course-owned profile tables in the root-owned runtime file
`/etc/grading/profiles.toml` there and restart `grading-executor`. The service combines
that file with cluster-owned connection settings into `/run/grading-executor/config.toml`.
It uses systemd credentials for the worker token, CA trust and namespace-scoped
Kubernetes kubeconfig. None of these files enter the student staging volume or Nix store.
Register the corresponding worker profile names and resource caps using the real CLI
on Astrid. No cluster rebuild is needed when legacy exercise profiles change.
[executor.toml.example](executor.toml.example) describes the complete configuration.

Profiles independently approve image digests, commands, timeouts, and resource
caps. The executor stages files in a host directory backing the student Jobs' local PVC. Student mounts
are read-only per-lease subpaths; credentials never belong on that volume. The
cluster configures the `gvisor` RuntimeClass, namespace policies, admission rules,
restricted ServiceAccount, PID/resource limits, and log rotation. Test enforcement
on the actual nodes before running student submissions.

The worker API is separate from browser routes. Astrid nginx uses the existing CA
for server TLS and restricts the endpoint to Mickey. The executor verifies that CA;
the application authenticates the worker bearer token against its stored hash.
The public proxy must never expose `/internal/` or the worker listener.

Register the worker on Astrid (replace the profile name with the approved profile):

```sh
sudo -u grading-operator gradingctl --database-url-file /etc/grading/operator.url \
  worker register --id mickey-1 --token-file /var/lib/grading/worker-token \
  --profile YOUR_PROFILE --cpu 10 --memory-gib 32 --storage-gib 64
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
