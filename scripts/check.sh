#!/usr/bin/env bash
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"
cargo fmt --all -- --check
cargo clippy --locked --all-targets -j "${SGPU_BUILD_JOBS:-2}" -- -D warnings
cargo test --locked --all-targets -j "${SGPU_BUILD_JOBS:-2}"
shopt -s nullglob
INSTALL_TEST=""
for candidate in "${CARGO_TARGET_DIR:-$REPO/target}"/debug/deps/install_lifecycle-*; do
    if [ -f "$candidate" ] && [ -x "$candidate" ] && { [ -z "$INSTALL_TEST" ] || [ "$candidate" -nt "$INSTALL_TEST" ]; }; then INSTALL_TEST="$candidate"; fi
done
[ -n "$INSTALL_TEST" ] || { echo "installer integration test binary missing" >&2; exit 1; }
if [ "$(id -u)" = 0 ]; then
    "$INSTALL_TEST" --ignored
elif command -v sudo >/dev/null && sudo -n true 2>/dev/null; then
    sudo -n -- "$INSTALL_TEST" --ignored
elif [ "${SGPU_REQUIRE_ROOT_TESTS:-0}" = 1 ]; then
    echo "root installer checks require passwordless sudo" >&2; exit 1
else
    echo "SKIPPED: three root-only installer checks (passwordless sudo unavailable)"
fi
SCRIPTS=(bootstrap.sh scripts/*.sh)
for script in "${SCRIPTS[@]}"; do bash -n "$script"; done
command -v shellcheck >/dev/null || { echo "shellcheck is required for the full gate" >&2; exit 1; }
shellcheck --severity=warning "${SCRIPTS[@]}"
cargo run --locked --quiet --bin sgpu -- bench --nodes 16 --repeat 2 >/dev/null
