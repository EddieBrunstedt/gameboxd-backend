#!/usr/bin/env bash
# Runs the same checks as CI. Pass --database to include the PostgreSQL
# integration tests, which need TEST_DATABASE_URL (see .env.example).
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked

if [ "${1:-}" = "--database" ]; then
    cargo test --locked --test backend -- --ignored
fi
