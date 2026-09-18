# Shared-runner exercise

Use this directory as the private grader repository, together with
`examples/scripted-template` as a separate student template repository.
No exercise image build or flake is required. The prebuilt `runner-image` from the
platform contains Python and GCC for this example. Register with schema 3 and a
`--runner-image REGISTRY/runner@sha256:DIGEST` approved by the executor.

The controller runs Python from `/grader` and requests isolated compilation and
execution via `/platform/grading-run`. Only the controller mounts this repository.
Private grading is manually scheduled after the effective deadline and halves the
original public score if extra inputs fail. Student private reports contain only
score/status; detailed reasons are instructor-only.

## Quick cache check

This example enables optional `[caching]` in schema 3. The recipe compiles
`cache/launcher.c` once and exports `launcher.o`. Every public/private test compiles
the current student source with its `main` renamed to `student_main`, then links
that object with the cached launcher. The student template needs no changes.
The cache is tiny; preparation uses one CPU, 1 GiB memory/storage and a 60-second
limit. `max_size_gib = 1` is an upper bound, not an allocation.

From the platform checkout, the fastest check requires only Python, Bash and `cc`:

```sh
python3 examples/check-caching.py
```

It prepares once, runs the example's public/private cases, touches the student
source, changes its contents, and checks that the same seed is reused with the
correct new output. It also checks that compilation fails without the seed.
This is a local recipe/compilation smoke test; it does not exercise Kubernetes,
registration, or sandbox isolation.

For a real preparation/reuse check:

1. Copy this directory's contents into your private grader repository, then commit
   and push. In your existing exercise catalog, set `grader_ref` to that new commit
   (or `main`). Set `existing = true` if testing an already-created student repository.
   Use your actual repository names and approved runner digest; the top-level example
   catalog contains placeholders.
2. On the cache-enabled operator environment, run the same apply twice:

   ```sh
   export GRADING_CACHE_CONFIG=/etc/grading/executor.toml
   gradingctl exercise apply exercises.toml --reason 'Test example cache'
   gradingctl exercise apply exercises.toml --reason 'Verify example cache reuse'
   ```

   The first invocation should print `Preparing cache` and then `Cache ready`.
   The second should print `Cache ready (reused)` with the same artifact digest,
   and should create no new preparation Job. If the seed already exists, both
   invocations reuse it; increment `caching.version`, commit and push to force a build.
3. Submit the unchanged echo starter within the exercise's open window. Public
   grading should return 20/20. Change its output and submit again: the score should
   change even though the launcher cache is reused. Private grading remains manual
   and available after the effective deadline.

The operator must run as UID 10004 with the shared executor staging PVC and
Kubernetes access, as described in the platform's `docs/exercises.md` cache section.
The existing runner already contains the tools this example needs; no image changes
are needed if that runner is already approved. Cache artifacts are mounted read-only
at `/cache/launcher` in student execution Pods. The trusted controller gets the grader
repository; the preparation Pod gets only its `cache/` subtree and the pinned template.
