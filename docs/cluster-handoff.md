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

## Central exercise registration integration

The application now supports `gradingctl exercise add/update/show`; see
[exercises](exercises.md) for the complete builder, image, scoring and update
contract. Add one `[registry]` executor policy and register `registered-v1` once.
Expose registration on a dedicated instructor Nix/Skopeo builder with operator
credentials. Do not add a Nix socket to public application services. Per-exercise
profile-file loading is no longer required for centrally registered exercises.
Apply migrations 0002 and 0003 plus updated database grants, and upgrade
web/tasks/executor together. Existing student
Git trees stay pinned; `--existing` changes only subsequent grading runs.

Schema version 2 supports arbitrary grading scripts; see the execution helper and
resource overhead in [exercises](exercises.md). Controller Pods need their per-lease
control PVC subpath writable by UID/GID 10004 (the executor's staging identity).
Student Pods use UID/GID 10003 and cannot mount that channel or private tests.
Budget one controller alongside the student Job. The instructor-only
`gradingctl exercise private-grade` schedules additional private grading after
effective deadlines; neither pushes nor the daily sync schedule private runs.
