# Centrally registered exercises

Instructors register GitHub template and private grader repositories with the
application CLI. No per-exercise executor profile is required. The cluster operator
configures one approved registry namespace and resource/time caps.

## Instructor workflow

Run registration using the operator CLI on the host with the application's artifact
volume (currently Astrid). Schema version 3 does not build or push images. Set the
operator DB and GitHub App configuration as described in [operations](operations.md).
Use `--artifact-dir` if the shared application artifact volume is not at the default
`/var/lib/grading/artifacts`. Registration must write the same volume the web service
reads and backs up.

Build/publish the reusable `runner-image` once separately, and approve its immutable
digest in the executor's `registry.runner_images`. The example runtime contains
Python, GCC, Bash and coreutils. Extend `nix/images.nix` and publish a new runner digest
when exercises require additional dependencies. No dependencies are fetched inside
grading Pods. Changing scripts/tests needs no image build or executor profile edit.

Import the course first with `gradingctl course apply course.toml`. A course can
now start with no assignments, just schema_version and its `[course]` table.
The command reads the supplied file directly, including uncommitted edits, with
no Git requirement. See [the minimal course file](../examples/course.toml).

### Apply exercise files

The primary bulk workflow is:

```sh
gradingctl exercise apply exercises.toml --reason 'Initial course exercises' --dry-run
gradingctl exercise apply exercises.toml --reason 'Initial course exercises'
```

[Example catalog](../examples/exercises.toml):

```toml
schema_version = 1
course = "systems-2026"
runner_image = "REGISTRY/grading/runner@sha256:FULL_DIGEST"

[exercises.echo]
template = "COURSE/echo-template"
template_ref = "main"
grader = "COURSE/echo-grader"
grader_ref = "FULL_COMMIT_SHA"
opens_at = 2026-10-12T08:00:00+02:00
deadline = 2026-10-26T23:59:00+01:00
existing = false
```

The local catalog defines repository URLs, refs, dates and runner selection. Each
private grader repository still owns its schema-3 `exercise.toml` with points,
resources, editable paths and commands. The catalog itself has schema version 1.
It requires no local Git repository. URLs accept GitHub HTTPS or `owner/repository`.
Refs accept branches (including `main`) or full lowercase commit SHAs; omitted refs
default to `main`, resolved afresh on each apply. Entry-level `runner_image` overrides
the file default. Omitting both retains an existing shared runner; new exercises
require one. `existing = true` explicitly updates future grading for existing
repositories, preserving their original template and historical results.

Multiple files may describe different courses or split one course's exercises:

```sh
gradingctl exercise apply first-half.toml second-half.toml --reason 'Course update' --dry-run
```

The **union of supplied files is the complete desired list for each named course**.
Other courses are untouched. Duplicate course/exercise pairs and unknown keys are
errors. Passing only one part of a split catalog proposes removing exercises from
its omitted parts. An empty list proposes retiring all active exercises in that course.

Every exercise is fetched/validated and its resolved commits shown before application.
The preview lists ADD, UPDATE, RESTORE and UNCHANGED entries. Removals print a prominent
warning and each affected course/exercise. Actual removal requires a terminal and
the exact phrase `REMOVE EXERCISES`; declining, EOF or noninteractive input aborts
without changing exercises. Dry runs show removals without prompting and store no
artifacts or database changes.

Removal means retirement from new repository allocations, not deletion of student
repositories, runs or grades. Existing repositories continue grading/closure under
their recorded policy and remain visible to their students. A later file application
can restore a retired exercise. The prepared catalog commits in one DB transaction;
a concurrent course/exercise change rejects the stale preview before any catalog
mutation. Source snapshots are retained before publication and may remain unreferenced
if publication fails. Audit records include operator, reason and input file digests.

### Individual registration (also supported)

```sh
gradingctl exercise add --course systems-2026 --name echo \
  --runner-image REGISTRY/grading/runner@sha256:FULL_DIGEST \
  --template https://github.com/COURSE/echo-template \
  --grader https://github.com/COURSE/echo-grader \
  --template-ref main --grader-ref main \
  --opens-at 2026-10-12T08:00:00+02:00 \
  --deadline 2026-10-26T23:59:00+01:00 \
  --reason 'Initial exercise' --dry-run
```

Remove `--dry-run` to retain the grader snapshot and publish the definition atomically.
Dry run fetches sources and validates configuration without retaining artifacts or
changing the DB. `--runner-image` is required on first schema-3 registration and is
retained on updates if omitted. Both public and private repositories use the
configured GitHub App; give it read access to the private grader repository.
Refs may be simple branch names or full lowercase commit SHAs. Omitted refs on
add default to `main`. URLs must be GitHub HTTPS URLs or `owner/repository`.

Registration records exact source commits, the retained grader snapshot digest and
the runner image digest. Snapshot/publication failures do not activate partial
revisions. Concurrent updates fail rather than overwrite newer revisions. Uploaded
but unpublished snapshots may remain; storage quotas and retention apply. Keep all
runner digests used by historical runs for reproducibility. Accepted student source
snapshots are also retained, so later force pushes do not destroy captured sources.

Inspect the current pinned definition:

```sh
gradingctl exercise show --course systems-2026 --name echo
```

## Updates and existing repositories

Update just the grader commit, without changing the template:

```sh
gradingctl exercise update --course systems-2026 --name echo \
  --grader-ref main --existing --reason 'Fix private source check'
```

On update, omitted repository URLs and refs retain their current values. Specifying
`--grader-ref main` resolves the branch again. Changing repository URLs requires
an explicit corresponding ref. `--opens-at` and `--deadline` are optional on update.

Update the template for newly allocated repositories:

```sh
gradingctl exercise update --course systems-2026 --name echo \
  --template-ref NEW_FULL_COMMIT_SHA --reason 'Correct template instructions'
```

Every publication creates an immutable definition and advances the assignment's
current pointer. Existing repositories retain their original template, branch,
opening/deadline and integrity manifest. No Git pushes or template merges are made
to their repositories. A repository already allocated/provisioning counts as
existing even if GitHub creation has not finished yet.

Without `--existing`, both template and grader updates apply only to future
repositories. With `--existing`, existing repositories use the new workflow,
grader snapshot, runner image and grading limits for subsequently queued runs, but keep
original template integrity checks. Previously queued/running jobs keep their
original grading revision. Existing scores are never silently rewritten.

If public tests change, update their public template file, then register that
commit. Existing repositories retain their original protected public files. If a new
grader needs revised test data for those repositories, include that data in the
grader repository and publish the revised public rubric to students explicitly.
Private checks are not the source of base scoring points. To recompute a prior
submission with the newly selected grader, use the existing audited command:

```sh
gradingctl regrade --submission SUBMISSION_UUID --reason 'Regrade after test correction'
```

This creates another run. Historical run definitions, source SHAs and results remain.

## Script-based grader repository contract

Use [the shared-runner grader](../examples/shared-grader/) and
[student template](../examples/scripted-template/) as a starting point. Its `exercise.toml` contains:

```toml
schema_version = 3
title = "C echo exercise"
branch = "main"
public_tests = "tests"
private_tests = "private"
editable = ["src/"]
max_points = 20
timeout_seconds = 120

[resources]
cpu = 1
memory_gib = 1
storage_gib = 1

[workflow]
public_command = ["/bin/python3", "/grader/public.py"]
private_command = ["/bin/python3", "/grader/private.py"]
```

`public_tests` names a file or directory in the template; `private_tests` names a
file or directory in the grader repository and is optional. The platform checks
these paths exist, but does not parse their contents. They may contain any testing
framework or data format. `private_command` is optional too. Opening time and
deadline come from registration. All template files outside `editable` are protected
by the platform's automatically generated integrity manifest. The executor checks
this before running scripts. Private grader files never enter student repositories.

The grader repository no longer needs `flake.nix` or image outputs. Its scripts are
mounted read-only at `/grader` in a trusted controller Pod. The controller and
student Jobs use the same approved runner digest, but only the controller receives
the private repository. The image supplies `/bin/sh`, `cp`, `/bin/python3` (needed
by the execution helper), and whatever compilers/runtimes the exercise requires.
The grading scripts can use any language available in that image.

The runner image must contain **no course tests, solutions, credentials or private
Nix closures**. Student code can inspect all image files. The operator explicitly
approves shared-runner digests in addition to the registry-prefix policy.

The App-authenticated service fetches exact commits and retains immutable snapshots.
Mickey downloads those through the authenticated, lease-scoped source API, verifies
their digests and commits, and stages them before starting Pods. No GitHub token is
issued to Mickey or a grading Pod, placed in a URL/environment, or persisted in a
Git checkout. This deliberately preserves the existing credential boundary; there
is no token inside the runner that must be erased after fetching. The executor's
internal-API and Kubernetes credentials remain outside all grading Pods too.

## Running custom code and returning points

The trusted grading script runs in its own Pod with:

- `/submission`: the immutable student source, read-only.
- `/grader`: the pinned grader repository, read-only; also the controller working directory.
- `/grading/input.json`: phase, maximum points, submission SHA, and (for private
  grading) the original `public_points` and `public_run_id`, read-only.
- `/platform/grading-run`: the platform's helper for isolated student execution.
- `/tmp`: writable temporary space. `/control` is the helper's private file channel.

The script may implement arbitrary test orchestration and scoring logic. To execute
student code, use the helper, for example:

```sh
/platform/grading-run --stdin /tmp/input --output /tmp/execution.json -- \
  /bin/sh -c '/bin/cc src/main.c -o /workspace/program && /workspace/program'
```

This starts a fresh sandbox using the pinned **shared runner image**, with a writable
copy of source in `/workspace`. It returns JSON with `exit_code`, `stdout` and
`truncated`. The script inspects this response and determines points itself. Each
call has a fresh workspace: combine compilation and execution in one command when
necessary. A nonzero command exit is returned to the script for interpretation.
Timeouts, OOM and infrastructure failures abort the grading run without inventing
a zero score. Stderr stays in bounded sandbox storage unless the command redirects
it into stdout. Current transport limits are 64 KiB each for stdin/stdout, 1 MiB per
request, and 1,000 calls per grading run; all calls share the exercise timeout.

Do not execute student code directly in the trusted grading Pod: that would let it
forge the final score or read private tests. The student Pod receives only its own
input and source; it cannot access the script, expected answers, control channel,
or worker credentials. A student necessarily sees private inputs supplied to its
program. Parse its output as untrusted data.

On success, the script exits zero and writes only a final JSON object to stdout:

```json
{"schema_version":1,"points":18,"reason":"", "invalidated":false}
```

Diagnostics belong on stderr. Integer points must be in `0..max_points`. Only the
trusted script's output is accepted as a score; student stdout is just data returned
to that script. A failed script or malformed result is an infrastructure failure.
Public grading reasons are student-visible. Private run reports expose only scores
and status to students; detailed reasons remain available to instructors. Scripts
must not use private inputs during public grading or echo sensitive data there.

The public script runs for normal submissions and should implement exactly the
public rubric. The platform does not prescribe a test format or calculate scores
from a platform-defined list of cases.

## Additional private grading after the deadline

Private grading is never scheduled by pushes or daily synchronization. An instructor
explicitly requests it after the deadline:

```sh
gradingctl exercise private-grade --course systems-2026 --name echo \
  --reason 'Run additional checks on final submissions'
```

This freezes due final selections if necessary, and queues private runs only for
repositories whose effective deadline (including extensions) has passed and whose
final submission's latest public run completed successfully. It reports skipped
repositories; repeat the command after pending public grading or extensions finish.
Database and executor checks enforce the deadline too. The runtime web database
role cannot create private runs.

The private script receives the pinned public baseline, for example:

```json
{"schema_version":1,"phase":"private","public_points":18,"public_run_id":"...","max_points":20,"sha":"..."}
```

It can run additional tests through the same helper and use any scoring logic:
`points = public_points // 2`, `points = 0`, or `points = max(0, public_points - 2)`.
Return the **final points**, not a delta. A changed score requires a nonempty reason.
To invalidate a result for instructor review, return `points: 0`, `invalidated: true`
and a reason; the official numeric score is then absent. A failed private test is
not automatically classified as misconduct.

Private runs preserve the original public result and record their own adjustment.
Repeating the command deduplicates by public baseline and grader revision, so it
cannot repeatedly halve an already adjusted score. Publish a corrected grader with
`exercise update --grader-ref main --existing`, then repeat the command to run that
revision. A public regrade creates a fresh baseline. Historical results remain.

The example uses JSON public tests and a custom Python controller to compile C in
isolated Pods; the private script checks additional inputs and halves the public
score if they fail. These are example choices, not platform requirements.

## Cluster integration change

Configure the executor once using [executor.toml.example](executor.toml.example).
Register the worker with `--profile registered-v1`. Match the runner repository to
the registry prefix and list its exact digest in `runner_images`. An empty allowlist
disables shared runners. Only exercise schema version 3 is supported.
Only trusted operators should publish/review reusable runtime images.

Build the example runtime with `nix build .#runner-image`; publishing it to your
registry is a separate deployment step. No course sources are included. Its contents
provide the runtimes for the C/Python example.

Apply migrations through 0006 and updated grants using the owner role. Upgrade
web/tasks/executor together before registering schema 3. Migration 0004 adds a
foreign-key reference from immutable revisions to their retained grader artifacts;
back up those artifacts with accepted student snapshots and reports. Migration 0005
adds the exercise retirement flag used by file imports.

The controller uses UID/GID 10004 and needs a writable per-lease `control` subpath
on the staging PVC. The executor must own this directory and the private grader
staging directory as UID 10004. The grader directory has mode 0700 and is mounted
read-only only in the controller. Student Pods remain UID/GID 10003 with only
source/input subpaths and their own ephemeral workspace. Both use the restricted
runtime and a deny-network policy. Budget additional controller requests (100m CPU,
128 MiB) and limits (1 CPU, 512 MiB memory, 1 GiB ephemeral storage) alongside
student resources. Restrict cluster Pod-log access to operators.

Student programs necessarily observe the private inputs they are given. The platform
cannot promise information-theoretic secrecy: final scores/timing are also observable.
It isolates private files/answers, prevents direct network/file exfiltration under
the enforced sandbox policy, withholds private stdout/stderr from student reports,
and never gives students control-channel or grader mounts. Instructor controllers
are trusted; they must never execute student code directly in their own Pod.

Validate actual gVisor mounts, network denial, image pulls, snapshot staging and the
public/private grading cycle in the cluster before using this with students. Local
unit/integration tests and image builds do not establish those external properties.

Migration 0006 removes the unused fixed-test results table. Earlier grader schemas
and local-profile configurations are unsupported; register exercises with schema 3
and update executor configuration before resuming grading. Historical fixed-test
report artifacts remain retained, but their revisions cannot be executed.
