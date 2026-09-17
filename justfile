set shell := ["bash", "-euo", "pipefail", "-c"]

check:
    cargo fmt --all -- --check
    cargo clippy --locked --offline --workspace --all-targets -- -D warnings

test:
    bash tests/database.sh

sqlx-prepare:
    bash tests/database.sh prepare

preview:
    cargo run --locked --offline -p grading-web -- --preview

build:
    cargo build --locked --offline --workspace
