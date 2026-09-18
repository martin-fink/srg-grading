# Current examples

Use these together for the shared runner workflow:

- `course.toml`: local course metadata.
- `exercises.toml`: local exercise catalog, **schema version 1**.
- `shared-grader/`: copy its contents to the private grader repository. Its
  `exercise.toml` uses **schema version 3**, with commands pointing at `/grader`
  and a tiny automatically prepared cache. See its README for the cluster check.
- `scripted-template/`: copy its contents to the student template repository.

The CLI reads the grader repository at the commit selected by `grader_ref` in the
local catalog. Commit and push grader changes, then use `main` or the new full SHA.
Local edits to a grader checkout do not change a previously pinned remote commit.

For a local cache smoke test without a database or cluster, run
`python3 examples/check-caching.py` from the platform checkout.
