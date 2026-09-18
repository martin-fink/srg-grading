# Student assignment grading

A Rust 2024 application for GitHub-based, individual course assignments. PostgreSQL
stores application state and durable tasks; Axum and Askama serve a small website.
The interface uses TUM blue (`#0065BD`), plain typography, local CSS, and ordinary
forms. It has no frontend build step, CDN, or JavaScript dependency.

This repository builds the application and its container images. Deployment lives
in [doctor-cluster-config](https://github.com/TUM-DSE/doctor-cluster-config/blob/master/docs/grading-infrastructure.md).
Live GitHub App permissions, Kubernetes/gVisor isolation, and the instructor-account
pilot require deployment acceptance testing.

## Try the website

```sh
nix develop
cargo fetch --locked
just preview
```

Open **http://127.0.0.1:8080**. Preview mode uses clearly marked example data, binds
only to loopback, and exposes no login or mutation endpoints. For the real service,
GitHub login and Secure cookies require HTTPS behind nginx.

## Build and validate

```sh
just build
just check
just test
```

`just check` runs `cargo fmt --all -- --check` and Clippy with warnings denied.
`just test` creates a disposable PostgreSQL cluster with SCRAM authentication and
distinct owner, web, operator, and admin credentials. It runs the tests, dumps the
database, restores it into a second isolated database, copies the artifacts, and
checks their hashes. Temporary test credentials and databases are removed on exit.
No existing database or GitHub account is contacted by these tests.

`cargo test --workspace` alone skips DB/HTTP integration tests when their test
environment is absent. Run `just test` for those checks. Tests cover strict config,
integrity changes, unsafe paths/modes, repository allocation races, username
identity, admin revocation, DB permissions, CSRF, unknown accounts, report ownership,
webhook signatures/replays, leases, result provenance/replays, deadline selection,
overrides, GitHub creation recovery, rate limiting, and sandbox Job restrictions.

```sh
just sqlx-prepare
nix build .#grading-portal
nix build .#web-image .#cli-image .#postgres-image .#executor-image
```

The flake and Rust toolchain are pinned; Cargo dependencies are locked. Checked-in
`.sqlx` metadata supports the query macro without a build-time database. Parameterized
runtime queries are exercised by PostgreSQL integration tests. When adding query
macros, regenerate metadata with `just sqlx-prepare` against the disposable DB.
Git-based Nix flakes include tracked files: add new source files to Git before using
the `.#` build commands.

## Components

| Crate | Responsibility |
| --- | --- |
| `core` | Strict config, bounded Git snapshots, SHA-256 manifests, worker protocol |
| `store` | Migrations, sessions, imports, score views, durable leases, artifact metadata |
| `github` | App JWTs/tokens, PKCE authorization, repository APIs, Checks |
| `web` | Login, assignment dashboard, forms, plain-text reports, worker listener |
| `executor` | Runner approval, gVisor Jobs, trusted script scoring |
| `cli` | Imports, admin changes, queue processing, synchronization, exports |

The three binaries are `grading-web`, `gradingctl`, and `grading-executor`.
See [operations](docs/operations.md), [hardening rollout](docs/hardening.md), [grading protocol](docs/grading.md), and
[pilot acceptance](docs/acceptance.md) before connecting external systems.

## Administration

Credentials are read from runtime files, not flags containing passwords. The
`GRADING_DATABASE_URL_FILE` and `GRADING_GITHUB_CONFIG` environment variables may
point to those files for `gradingctl`. Every command also supports explicit paths.

```sh
gradingctl --database-url-file /run/secrets/owner-url migrate
gradingctl --database-url-file /run/secrets/admin-url admin grant --github-username martin-fink --reason 'Course administrator'
gradingctl --database-url-file /run/secrets/admin-url admin list
gradingctl --database-url-file /run/secrets/admin-url admin revoke --github-username martin-fink --reason 'Role ended'

gradingctl course apply /courses/systems/course.toml --dry-run
gradingctl course apply /courses/systems/course.toml
gradingctl roster import --course systems-2026 /courses/systems/students.csv --dry-run
gradingctl roster import --course systems-2026 /courses/systems/students.csv
gradingctl grades export --course systems-2026 --output /exports/grades.csv

gradingctl extension --repository REPOSITORY_UUID --deadline 2026-11-01T12:00:00Z --reason 'Approved extension'
gradingctl grades override --repository REPOSITORY_UUID --points 18 --reason 'Reviewed correction'
gradingctl regrade --submission SUBMISSION_UUID --reason 'Infrastructure recovery'
gradingctl select-submission --event EVENT_UUID --reason 'Reviewed delayed webhook evidence'
gradingctl sync
```

Admin management uses a dedicated DB credential unavailable to the web and
operator roles. Grant/revoke take GitHub handles and resolve the numeric account ID
through GitHub before changing membership; `--github-id` is not accepted. `admin list`
resolves stored IDs to current handles. These commands need GitHub network access,
but no GitHub App credential. A failed lookup makes no membership change. Student
imports likewise require `github_username`, resolve it through GitHub, and reject
a supplied `github_id`. Numeric IDs remain internal identity keys so a renamed
account does not transfer access to the next owner of its old handle.

There is no HTTP grant endpoint. The last admin cannot be revoked
without `--recovery-override`. Audit records include the operator, immutable target
ID, reason, and timestamp. Host-root provisioning supplies the operator identity.

Course imports read the supplied local TOML file directly, including uncommitted
edits. No Git repository is needed; the import records a SHA-256 content digest.
Course files contain metadata only and need no GitHub credential. See [course.toml](examples/course.toml).
Roster imports preserve omitted enrollments and history.

Apply the complete exercise list from one or more local files:

```sh
gradingctl exercise apply exercises.toml --reason 'Course setup' --dry-run
gradingctl exercise apply exercises.toml --reason 'Course setup'
gradingctl exercise apply first-half.toml second-half.toml --reason 'Course update' --dry-run
```

See [exercises.toml](examples/exercises.toml). Files for the same course are merged;
duplicate exercise names are rejected. Template/grader refs accept branch names or
full commit SHAs. Omitted refs mean `main`, resolved to exact commits on each apply.
Exercises omitted from the combined list are proposed for retirement. Apply prints
a prominent removal list and requires typing `REMOVE EXERCISES` in a terminal.
Dry run never prompts or changes records. Retired exercises stop accepting new
repository allocations; existing repositories, grading and history remain intact.
All database changes commit together after validation/confirmation. Concurrent
catalog changes reject the stale plan. Listing a retired exercise restores it.

The initial schema implements the exact receipt-time cutoff policy. Extensions are
explicit and must precede closure. After closure, use the audited event-selection
override; neither timestamps nor existing event records are rewritten.

## Register and update exercises

Instructors apply template and private grader repositories with
`gradingctl exercise apply`, then apply catalog changes to advance pinned hashes.
Individual `exercise add/update` commands are also available.
Schema version 3 uses a prebuilt shared runner image and mounts the pinned grader
snapshot only in the trusted controller. Registration builds no images; adding
exercises needs no per-exercise cluster profile. The root flake provides a baseline
`runner-image` with Python/GCC/Bash, built and published separately.

Public grading scripts define the score. Private scripts run only after the effective
deadline, manually queued with `gradingctl exercise private-grade`, and receive the
original public points. Student code runs in separate sandboxes without private test
files or credentials. Private reports expose only scores/status to students.

See [the exercise workflow](docs/exercises.md) and [shared-runner example](examples/shared-grader/).
Template updates affect future repositories only. `--existing` rolls the new grader
into subsequent runs for existing repositories, preserving historical runs.

## Instructor configuration

Use [course.toml](examples/course.toml) for course metadata and
[exercises.toml](examples/exercises.toml) for the complete exercise catalog.
Both local files use schema version 1; the private grader's `exercise.toml` uses
schema version 3. Earlier grader schemas and local execution profiles are unsupported.
CSV roster columns are exactly `student_id,name,github_username`; equivalent TOML
uses `[[students]]` entries.

The platform generates integrity manifests from pinned templates. All files outside
explicitly editable directory prefixes are protected. `.github/` and `tests/` cannot
be editable. Reference solutions and private tests belong in the private grader
repository. Student repository Actions remain disabled; public feedback comes from
the portal. Pin dependencies in the approved runner image.

## Design references

- [TUM corporate design](https://portal.mytum.de/corporatedesign/folder_listing)
- [GitHub App authorization and PKCE](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-user-access-token-for-a-github-app)
- [GitHub repository APIs](https://docs.github.com/en/rest/repos/repos)
- [SQLx offline mode](https://docs.rs/sqlx/latest/sqlx/macro.query.html)
