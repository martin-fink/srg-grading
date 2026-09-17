# Cluster integration ownership

The deployment is maintained in
[doctor-cluster-config](https://github.com/TUM-DSE/doctor-cluster-config/blob/master/docs/grading-infrastructure.md),
under `modules/grading/`. That is the source of truth for Kubernetes resources,
nginx on the public VM and Astrid, the existing internal CA, SOPS keys, storage,
worker isolation, synchronization/backup timers, administration, and cutover.
The old prototype Compose stack, systemd units, nginx fragment, backup script,
and Kubernetes sample have been removed to avoid maintaining two deployments.

This repository retains:

- The root flake and lock file, Rust application and migrations.
- `nix/images.nix` and `nix/postgres-entrypoint.sh` remain optional container packaging;
  the NixOS cluster deployment uses native binaries and its own dedicated PostgreSQL service.
- `nix/database-roles.sql` and `nix/database-grants.sql`, used by tests/the CLI.
- `docs/executor.toml.example`, documenting the executor's configuration format.

The cluster fetches the public repository over HTTPS through its `srg-grading`
flake input. Commit and push application changes, then run
`nix flake update srg-grading` in the infrastructure repository. NixOS installs the
Rust binaries directly. PostgreSQL, web, tasks, migrations and administration run on
Astrid; the executor runs as a systemd service on Mickey. Student execution and trusted grading-controller
Jobs run in Kubernetes. There is no image build/load step or Python CLI wrapper.

See [operations](operations.md) for the application's runtime contracts,
[grading](grading.md) for the execution protocol, and [acceptance](acceptance.md)
for validation still required against real GitHub, gVisor, networking, and backups.
Course repositories own exercise profiles, images, tests and solutions. Operators
install reviewed runtime profiles on Mickey and restart `grading-executor`; no
cluster rebuild is needed when exercises change. Register worker capabilities using
the real CLI under the operator account on Astrid. The cluster supplies connection
settings, credentials and sandbox limits. No sample image is approved by default.

## Shared runner and central exercise registration

Schema version 3 removes image builds from `gradingctl exercise add/update`.
Build/publish the root flake's `runner-image` separately, then list its immutable
digest in `registry.runner_images` and pass `--runner-image` when registering an
exercise. The image contains only runtimes/tools; private grader files must never
be baked into it. No per-exercise profile file is needed. Schema 1/2 remain supported.

Run registration on Astrid with the operator credential, GitHub App configuration,
and the application's artifact directory. Registration pins and retains the grader
snapshot there. Mickey fetches accepted student and grader snapshots over the
lease-authenticated internal API; it receives no GitHub token. The source gateway
preserves the existing App credential boundary rather than minting tokens for Jobs.

Apply migrations through 0005 and updated grants; upgrade web/tasks/executor together.
Include grader artifacts in the existing application-volume backups. Existing student
Git trees stay pinned; `--existing` changes only subsequent grading runs.

Controller Pods and student Jobs use the same runner image in separate environments.
Only the controller mounts `/grader` read-only and its per-lease control channel
writable. Private staging/control directories are owned by executor UID/GID 10004,
mode 0700. Student Jobs use UID/GID 10003 and have neither mount nor credentials.
Retain gVisor, deny-network policy, bounded logs and restricted Pod-log RBAC. Budget
the controller overhead specified in [exercises](exercises.md). Existing manifests
need no new resource kind or Kubernetes permission, but must permit the controller's
restricted mounts and identity.

`gradingctl exercise private-grade` remains instructor-only and post-deadline.
Student private reports now show scores/status only; detailed findings are available
to current administrators. Private test inputs seen by a student program cannot be
made inherently secret, so never publish its private-run output to students.

Local `course apply` reads current file contents without Git. The new
`exercise apply FILE... --reason TEXT [--dry-run]` reads a complete TOML catalog
for each named course and needs the same operator/App/artifact access as registration.
Allow terminal stdin for typed removal confirmation; noninteractive removals are
refused. Migration 0005 supports retirement without deleting repositories or grades.
