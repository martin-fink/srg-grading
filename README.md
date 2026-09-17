# Repository-first grading prototype

A Rust 2024 prototype for GitHub-based, individual course assignments. PostgreSQL
stores application state and durable tasks; Axum and Askama serve a small website.
The interface uses TUM blue (`#0065BD`), plain typography, local CSS, and ordinary
forms. It has no frontend build step, CDN, or JavaScript dependency.

This repository does not deploy anything or migrate Autolab/Tango. Live GitHub App
permissions, a real Kubernetes/gVisor workload, VM container isolation, and the
instructor-account pilot remain deployment acceptance work.

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
| `executor` | Independent profile approval, gVisor Jobs, trusted output comparison |
| `cli` | Imports, admin changes, queue processing, reconciliation, exports |

The three binaries are `grading-web`, `gradingctl`, and `grading-executor`.
See [operations](docs/operations.md), [grading protocol](docs/grading.md), and
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
gradingctl reconcile
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

Course imports read **committed HEAD files** from the supplied Git repository,
including the private manifest; working-tree changes are not applied. Dry runs
resolve GitHub identities and validate changes inside a rolled-back transaction.
Imports preserve omitted enrollments and all history. Existing repositories keep
their original assignment revision when a new course version is imported. The
prototype intentionally has no automatic template rollout to existing repositories.

The initial schema implements the exact receipt-time cutoff policy. Extensions are
explicit and must precede closure. After closure, use the audited event-selection
override; neither timestamps nor existing event records are rewritten.

## Instructor configuration

[The fixture](tests/fixtures/course.toml) illustrates the strict TOML shape, including
the required timezone, public test path, and execution profile. Its names and image
digest are placeholders, not a deployable course. CSV columns are exactly
`student_id,name,github_username`; equivalent TOML uses `[[students]]` entries.

Generate a private manifest from a real pinned template:

```sh
gradingctl manifest generate \
  --template ORGANIZATION/TEMPLATE --revision FULL_COMMIT_SHA \
  --editable src/ --output /courses/systems/integrity/assignment.toml
```

Commit the manifest and course definition in the private course repository. All
files outside explicitly editable directory prefixes are protected, and additions
outside those prefixes fail integrity. `.github/` and `tests/` cannot be editable.
Reference solutions and private manifests must never be in the student template.
Pin workflow Actions and image/dependency inputs in the approved template.

The functional prototype uses public stdin/stdout test cases; see
[the sample suite](tests/fixtures/cases.toml). The independently configured worker
command executes the student's program, and the trusted executor compares its
output with those same public expectations. Programs cannot submit official points
or supply a score file. LLVM, FPGA simulation, and SimBricks need exercise-specific
profile review and real workload validation before support can be claimed.

## Design references

- [TUM corporate design](https://portal.mytum.de/corporatedesign/folder_listing)
- [GitHub App authorization and PKCE](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-user-access-token-for-a-github-app)
- [GitHub repository APIs](https://docs.github.com/en/rest/repos/repos)
- [SQLx offline mode](https://docs.rs/sqlx/latest/sqlx/macro.query.html)
