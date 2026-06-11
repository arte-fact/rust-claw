# `just check` is the definition-of-done gate for every PLAN.md subtask.

check: fmt-check clippy test

fmt-check:
    cargo fmt --check

fmt:
    cargo fmt

clippy:
    cargo clippy --all-targets -- -D warnings

test:
    cargo test

build:
    cargo build
