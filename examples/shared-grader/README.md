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
