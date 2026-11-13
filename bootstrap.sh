#!/usr/bin/env bash
# Fetch main and build/install natively on this host.
set -euo pipefail
SOURCE="${SGPU_SOURCE_DIR:-$HOME/.sgpu/source}"
REPOSITORY="https://github.com/eightmm/slurm-gpu-tui.git"
REQUIRED_RUST=1.93.0
BUILD=1
for arg in "$@"; do
    case "$arg" in
        --no-build) BUILD=0 ;;
        -h|--help)
            echo "usage: curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh | bash -s -- [install options]"
            echo "Options: --role master|node --mode gpu|cpu --prefix PATH --no-service --no-build"
            echo "Requires Linux, git and a C compiler. Missing Rust is installed with rustup (SGPU_INSTALL_RUST=0 disables this)."
            exit 0 ;;
    esac
done
case "$SOURCE" in
    /|*/../*|*/..|../*|..|[!/]*) echo "SGPU_SOURCE_DIR must be an absolute dedicated directory" >&2; exit 1 ;;
esac
[ "$(uname -s)" = Linux ] || { echo "sgpu requires Linux" >&2; exit 1; }
command -v git >/dev/null || { echo "git is required; install it with your system package manager" >&2; exit 1; }

rust_ready() {
    command -v cargo >/dev/null && command -v rustc >/dev/null || return 1
    local version major minor
    version="$(rustc --version 2>/dev/null)" || return 1
    [[ "$version" =~ ^rustc\ ([0-9]+)\.([0-9]+)\. ]] || return 1
    major="${BASH_REMATCH[1]}"
    minor="${BASH_REMATCH[2]}"
    ((major > 1 || major == 1 && minor >= 93))
}
if [ "$BUILD" = 1 ]; then
    if [ -z "${CC:-}" ] && ! command -v cc >/dev/null && ! command -v gcc >/dev/null && ! command -v clang >/dev/null; then
        echo "A C compiler/linker is required (e.g. build-essential on Debian/Ubuntu, gcc on RHEL)" >&2
        exit 1
    fi
    if ! rust_ready; then
        export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
        if ! rust_ready; then
            [ "${SGPU_INSTALL_RUST:-1}" != 0 ] || { echo "Rust $REQUIRED_RUST+ is required; install it or use --no-build with SGPU_BIN_DIR" >&2; exit 1; }
            if ! command -v rustup >/dev/null; then
                command -v curl >/dev/null || { echo "curl is required to install rustup" >&2; exit 1; }
                RUSTUP_SCRIPT="$(mktemp "${TMPDIR:-/tmp}/sgpu-rustup.XXXXXXXX")"
                trap 'rm -f -- "$RUSTUP_SCRIPT"' EXIT
                echo "Installing the minimal Rust toolchain for this build..."
                curl --proto '=https' --tlsv1.2 --connect-timeout 15 --max-time 120 -fsSL https://sh.rustup.rs -o "$RUSTUP_SCRIPT"
                sh "$RUSTUP_SCRIPT" -y --no-modify-path --default-toolchain none </dev/null
                rm -f -- "$RUSTUP_SCRIPT"
                trap - EXIT
            fi
            rustup toolchain install "$REQUIRED_RUST" --profile minimal </dev/null
            export RUSTUP_TOOLCHAIN="$REQUIRED_RUST"
            rust_ready || { echo "Rust toolchain setup failed" >&2; exit 1; }
        fi
    fi
fi
if [ -d "$SOURCE/.git" ]; then
    if [ -n "$(git -C "$SOURCE" status --porcelain)" ]; then
        if [ "${SGPU_FORCE_UPDATE:-0}" != 1 ]; then
            echo "source has local changes; preserve them before upgrading (SGPU_FORCE_UPDATE=1 explicitly discards tracked changes)" >&2
            exit 1
        fi
        git -C "$SOURCE" reset --hard HEAD
    fi
    git -C "$SOURCE" fetch --quiet origin main </dev/null
    git -C "$SOURCE" merge --ff-only origin/main </dev/null
else
    git clone --quiet --depth 1 --branch main --single-branch "$REPOSITORY" "$SOURCE" </dev/null
fi
exec bash "$SOURCE/install.sh" "$@"
