#!/usr/bin/env bash
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"
cargo fmt --all -- --check
cargo clippy --locked --all-targets -j "${SGPU_BUILD_JOBS:-2}" -- -D warnings
cargo test --locked --all-targets -j "${SGPU_BUILD_JOBS:-2}"
shopt -s nullglob
SCRIPTS=(*.sh scripts/*.sh chkgpu)
for script in "${SCRIPTS[@]}"; do bash -n "$script"; done
command -v shellcheck >/dev/null || { echo "shellcheck is required for the full gate" >&2; exit 1; }
shellcheck --severity=warning "${SCRIPTS[@]}"
cargo run --locked --quiet --bin sgpu -- bench --nodes 16 --repeat 2 >/dev/null
