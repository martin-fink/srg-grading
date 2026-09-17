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
- `nix/images.nix` and `nix/postgres-entrypoint.sh` for the four Nix-built images.
- `nix/database-roles.sql` and `nix/database-grants.sql`, used by tests/the CLI.
- `docs/executor.toml.example`, documenting the executor's configuration format.

The cluster fetches a pinned SSH revision from
`git@github.com:martin-fink/srg-grading.git`. Commit and push application changes
before updating `modules/grading/source.json` in the infrastructure repository.
Build/load the images for the actual node architecture; NixOS deployment alone
does not update the image bundle or run migrations.

See [operations](operations.md) for the application's runtime contracts,
[grading](grading.md) for the execution protocol, and [acceptance](acceptance.md)
for validation still required against real GitHub, gVisor, networking, and backups.
The cluster generates executor configuration from explicitly approved profiles;
no sample image or course configuration is suitable for production by default.
