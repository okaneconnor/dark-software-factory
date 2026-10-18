#!/usr/bin/env bash
# dsf's definition of done for its own changes. Exit 0 = pass.
#
# CI runs exactly this script, and so will the factory once it builds itself:
# one gate, not a local check that drifts from CI. Keep it deterministic and
# offline-safe after the first dependency fetch (--locked keeps Cargo.lock
# authoritative).
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
