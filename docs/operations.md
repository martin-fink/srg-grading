# Operating the prototype

These are reviewable deployment definitions, not authorization to deploy. The
existing infrastructure repository must supply hosts, DNS/TLS, SOPS secrets,
storage, backup destinations, budgets, and Astrid-to-VM provisioning. Autolab/Tango
and their backups remain untouched.

## Ubuntu VM

Build and load the Nix images on the appropriate platform. Review
[`nix/compose.yaml`](../nix/compose.yaml) with the host provisioning mechanism;
the file is a deployment input, not an instruction to create a new remote host.

The database runs as UID 10002 on an internal Docker network with no published
port. Web/task containers run as UID 10001 with only the application credential.
Their writable mounts contain application artifacts and temporary files, never
PostgreSQL data. Containers have read-only root filesystems, dropped capabilities,
no privilege escalation, resource/PID limits, bounded Docker logs, and no Docker
socket. Only nginx exposes HTTPS. Both application listeners publish to loopback.

Host-root provisioning supplies:

- `GRADING_STATE/postgres`, owner 10002, mode 0700, on durable storage.
- `GRADING_STATE/artifacts`, owner 10001, mode 0700, on a separate dedicated volume.
- `GRADING_SECRETS`, a root-only **parent directory** populated from SOPS at runtime.
  Bind individual files into the relevant container; never mount the entire secret
  tree into the web container. Mounted files must be readable by the intended
  non-root container UID (0400, owner 10001 or 10002), while the host parent remains
  root-only. This avoids granting the web UID access to other host secrets.
- `GRADING_PUBLIC_URL`, an HTTPS origin; `GRADING_COURSES`, a read-only course checkout
  directory used only by the operator container.

Database secret files under `database/` are `postgres-password`,
`grading_owner-password`, `grading_web-password`, `grading_operator-password`, and
`grading_admin-password`. Choose distinct random passwords. The corresponding
`app/database-url`, `owner/database-url`, `operator/database-url`, and
`admin/database-url` contain role-specific PostgreSQL URLs pointing at
`database:5432/grading`. URL-encode password characters if necessary.

Only PostgreSQL reads its bootstrap passwords. Web and task services read only
`app/database-url`. One-shot migration, operator, and admin services receive their
own credentials. PostgreSQL always uses SCRAM; no `trust` rule is generated.

The App settings file, `app/github.json`, has this shape:

```json
{
  "app_id": "APP_ID",
  "installation_id": 123,
  "client_id": "CLIENT_ID",
  "client_secret": "RUNTIME_SECRET",
  "private_key_file": "/run/secrets/github.pem",
  "callback_url": "https://YOUR_HOST/auth/callback"
}
```

The private key is `app/github.pem`; `app/webhook` contains at least 32 random bytes
matching the App webhook secret **exactly**, including any trailing newline. No
credential is embedded in a Nix derivation, image, course config, or repository.

After reviewing the configuration, the host operator would start the database,
run the one-shot `migrate` service (which applies migrations and role grants), and
then start web and tasks. Imports and grants are separate one-shot commands. For
example, the intended admin invocation is:

```sh
docker compose -f /opt/grading/compose.yaml run --rm \
  -e USER="$SUDO_USER" admin \
  --database-url-file /run/secrets/database-url \
  admin grant --github-id 123456 --reason 'Course administrator'
```

The host operator identity is supplied by root, not accepted from an HTTP request.
The web and operator DB roles cannot change administrators or assume their role.
Migrations require the owner credential; the runtime never migrates at startup.

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

## Astrid executor

Review [`nix/kubernetes.yaml`](../nix/kubernetes.yaml) and independently provision
the `gvisor` RuntimeClass. The sample namespace has default-deny networking,
restricted Pod Security, quotas, and a role limited to Jobs, Pods, and Pod logs.
Bind that role to the executor's restricted identity. Do not hand these credentials
to the VM or to student Pods.

The source PVC must expose exactly the executor staging directory to Pods. Mount
the same storage read-write into the trusted executor at `staging_root`; student
Pods get read-only per-lease subpaths only. Do not put credentials or unrelated
data on that PVC. The executor UID is 10004, sandbox UID/GID 10003; ensure directories
are traversable and source files readable without granting Pods write access.

Configure the dedicated worker node pool with `podPidsLimit = 256`, bounded
container log rotation, sufficient ephemeral-storage enforcement, and gVisor.
The example namespace budget is 20 CPUs/64 GiB and must be replaced with approved
host budgets. NetworkPolicy requires an enforcing CNI. Images must already contain
dependencies because grading Pods cannot reach package registries or license servers.

Copy [`nix/executor.toml.example`](../nix/executor.toml.example) into private runtime
configuration and replace the placeholder image/command with reviewed values.
Register a random token using the local operator credential:

```sh
gradingctl worker register --id astrid-1 --token-file /run/secrets/worker-token \
  --profile functional-v1 --cpu 10 --memory-gib 32 --storage-gib 64
grading-executor --config /run/grading/executor.toml
```

The DB stores only the token hash. `worker revoke --id astrid-1` rejects further
leases and reports. Restrict the internal HTTPS virtual host to the executor
network and require a client certificate; `tls_identity_file` and `tls_ca_file`
configure that client channel. The narrow listener is separate from browser
routes. The provided nginx fragment leaves certificate and allowlist choices to
host infrastructure. Never proxy `/internal/` on the public host.

## Reconciliation, metrics, and backups

`gradingctl work` continuously processes provisioning, snapshots, Check publishing,
and locks. `gradingctl reconcile` is a separate daily command with a PostgreSQL
advisory lock; failures are recorded per repository. Review/install the persistent
systemd timer using the existing host mechanism. Neither script runs automatically
just because it exists in this repository.

The authenticated internal `/internal/metrics` endpoint exposes aggregate queue
age, pending/failed tasks, stale leases, pending locks, failed provisioning, and
integrity-failed runs. Reconciliation observations, integrity findings, and task
errors are also available in PostgreSQL; the admin page shows course counts. Host
monitoring must alert on failed locks/provisioning, queue age, repeated API failures,
disk quotas, stale leases, and backup freshness. Logs deliberately avoid request
bodies, OAuth tokens, raw external error responses, and roster contents. Private
CLI preview/export output should be handled as student data.

[`nix/backup.sh`](../nix/backup.sh) dumps this dedicated DB and backs it up with its
immutable artifacts to the **existing encrypted Borg destination**. It requires
host-supplied `BORG_REPO` and `BORG_PASSCOMMAND`; it does not initialize or replace
an archive. It records a success timestamp only after create/prune/compact succeed.
Configure service failure alerts and monitor the timestamp. No accepted-artifact
garbage collection runs during a backup, so every DB-referenced blob captured by
the dump remains present in the artifact tree.

Restore into a separate database and separate artifact directory, reapply role
grants, and verify every referenced digest before starting an isolated application
instance. `just test` exercises PostgreSQL dump/restore and artifact hashes locally;
it does not prove that the external Borg repository can be recovered. Perform that
restore drill with the real encrypted destination before the pilot.
