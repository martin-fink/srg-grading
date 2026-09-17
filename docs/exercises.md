# Centrally registered exercises

Instructors register GitHub template and private grader repositories with the
application CLI. No per-exercise executor profile is required. The cluster operator
configures one approved registry namespace and resource/time caps. Existing legacy
`profiles` remain supported during migration.

## Instructor workflow

Run registration on a dedicated trusted Linux Nix builder with `nix`, `skopeo`,
access to the approved registry, the operator DB credential and GitHub App config.
Do not run builds inside the public web service or a student Pod. Install the updated `gradingctl` binary on that builder; loading an executor
profile file does not register a central exercise.

Create a private builder configuration using [build.toml.example](build.toml.example).
Keep registry auth in its separate runtime file. Set:

```sh
export GRADING_DATABASE_URL_FILE=/run/secrets/operator-url
export GRADING_GITHUB_CONFIG=/run/secrets/github.json
export GRADING_BUILD_CONFIG=/run/secrets/build.toml
```

Import the course first with `gradingctl course apply course.toml`. A course can
now start with no assignments, just schema_version and its `[course]` table.
Keep course configuration committed to its private Git repository as before.

```sh
gradingctl exercise add --course systems-2026 --name echo \
  --template https://github.com/COURSE/echo-template \
  --grader https://github.com/COURSE/echo-grader \
  --template-ref main --grader-ref main \
  --opens-at 2026-10-12T08:00:00+02:00 \
  --deadline 2026-10-26T23:59:00+01:00 \
  --reason 'Initial exercise' --dry-run
```

Remove `--dry-run` to build, push images and publish the definition atomically.
Dry run resolves/fetches sources and validates configuration, but does not test a
build, push images or change the DB. Both public and private repositories use the
configured GitHub App; give it read access to the private grader repository.
Refs may be simple branch names or full lowercase commit SHAs. Omitted refs on
add default to `main`. URLs must be GitHub HTTPS URLs or `owner/repository`.

Registration records exact source commits and registry image digests. Build and
publication failures do not activate a half-built revision. A concurrent update
causes publication to fail rather than overwrite a newer revision; rerun against
the new current revision. Uploaded but unpublished images may remain in the
registry. Build source/logs remain in the private work directory for diagnosis;
apply storage quotas and retention there. Retain image digests used by historical
runs so those runs can be reproduced.

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
student/grader images and grading limits for subsequently queued runs, but keep
original template integrity checks. Previously queued/running jobs keep their
original grading revision. Existing scores are never silently rewritten.

If public tests change, update their public template file, then register that
commit. Existing repositories retain their original protected public files. If a new
grader needs revised test data for those repositories, bundle that data in the
grader image and publish the revised public rubric to students explicitly.
Private checks are not the source of base scoring points. To recompute a prior
submission with the newly selected grader, use the existing audited command:

```sh
gradingctl regrade --submission SUBMISSION_UUID --reason 'Regrade after test correction'
```

This creates another run. Historical run definitions, source SHAs and results remain.

## Script-based grader repository contract

Use [the scripted grader](../examples/scripted-grader/) and
[student template](../examples/scripted-template/) as a starting point. Commit the
grader's `flake.lock`. Its `exercise.toml` contains:

```toml
schema_version = 2
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
public_command = ["/bin/grade-public"]
private_command = ["/bin/grade-private"]
```

`public_tests` names a file or directory in the template; `private_tests` names a
file or directory in the grader repository and is optional. The platform checks
these paths exist, but does not parse their contents. They may contain any testing
framework or data format. `private_command` is optional too. Opening time and
deadline come from registration. All template files outside `editable` are protected
by the platform's automatically generated integrity manifest. The executor checks
this before running scripts. Private grader files never enter student repositories.

The flake supplies `packages.SYSTEM.studentImage` and `packages.SYSTEM.graderImage`
as Nix `dockerTools` image archives. The builder uses locked, pure flake evaluation,
refuses lock-file updates, disables flake-supplied configuration, and requests
sandboxed builds. Enforce sandboxing in the builder's daemon policy. Do not embed
credentials in sources or images. Private sources are fetched before building;
protect stores/caches containing private graders. Build for the worker architecture.

The student image contains `/bin/sh`, `cp`, and the exercise's compiler/runtime/tools.
It must not contain private tests, expected answers, or solutions. The grader image
contains the instructor's scripts and private tests, and `/bin/python3` for the
platform's execution helper. Grading scripts themselves can use any language.
Dependencies are built into the images; grading Pods have no network access.

## Running custom code and returning points

The trusted grading script runs in its own Pod with:

- `/submission`: the immutable student source, read-only.
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

This starts a fresh sandbox using the pinned **student image**, with a writable
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
Reasons are student-visible, so exclude private test data from them.

The public script runs for normal submissions and should implement exactly the
public rubric. The platform does not prescribe a test format or calculate scores
from a fixed list of stdin/stdout cases for schema version 2.

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

Schema version 1 and legacy local profiles remain supported for existing courses.
They use the older fixed stdin/stdout suite and adjustment checker, illustrated in
[the legacy example](../examples/grader/). Their private checks now also require
manual post-deadline scheduling.

## Cluster integration change

Configure the executor once using [registered-executor.toml.example](registered-executor.toml.example).
The `[registry]` section replaces per-exercise allowlists for `registered-v1`.
Register the worker with `--profile registered-v1` (retain additional legacy
profiles if needed). Match builder `registry_prefix` to executor `image_prefix`.
Only trusted instructors/builders may publish into that registry namespace; use
registry permissions and node-side image pull credentials. The prefix policy
is not image signing or a substitute for registry write authorization.

Apply migrations 0002 and 0003 and the updated database grants using the owner role before starting the new binaries. Stop
old web/task/executor processes during this upgrade: old binaries do not populate
the new grading revision column. Existing rows are backfilled to their old revision;
legacy serialized definitions retain their digests. Run history is preserved.

The cluster installs native application binaries from the root flake. Provision
the registration command on the separate trusted builder using that application
package, Nix and Skopeo. Do not add a host Nix socket or cluster administrator
credentials to the public services.

Validate one real build/push/pull, private repository fetch, private checker Pod,
score adjustment/invalidation and update/regrade cycle in the test organization
before using this with students. Unit/integration fixtures do not establish those
external properties.

Scripted workflows run a trusted controller Pod alongside student Jobs. The controller
uses UID/GID 10004 and needs a writable per-lease `control` subpath on the staging
PVC; student Pods remain UID/GID 10003 without that mount. The executor must own
that directory as UID 10004. Both use the restricted runtime and network policy.
Allow additional controller requests (100m CPU, 128 MiB) and limits (1 CPU, 512 MiB
memory, 1 GiB ephemeral storage) in namespace quotas alongside student resources.
Upgrade binaries together before publishing schema version 2 exercises.
