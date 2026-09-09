#!/usr/bin/env bash
# Runs the same checks as CI. Pass --database to include the PostgreSQL
# integration tests, which need TEST_DATABASE_URL (see .env.example).
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked

# The contract is TypeScript, and no Rust check can see a type error in it.
# CI always runs this; locally it is skipped with a notice rather than made a
# hard dependency of the backend's own test loop.
if command -v tsc >/dev/null 2>&1; then
    tsc -p contract
elif command -v npx >/dev/null 2>&1; then
    npx -y -p typescript@7.0.2 tsc -p contract
else
    echo "note: no TypeScript available; skipping the contract typecheck that CI runs" >&2
fi

if [ "${1:-}" = "--database" ]; then
    cargo test --locked --test backend -- --ignored
fi
