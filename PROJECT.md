# Verification

`bash scripts/check.sh` is the required gate on Linux with Rust 1.93:
rustfmt, clippy, all Rust tests, shell syntax/ShellCheck and a small render benchmark.
Root checks run five isolated installer lifecycle tests (including legacy-daemon
mode preservation and candidate preflight) and one filesystem
identity test (thread isolation, restoration, ownership and denied replacement).
They require root or passwordless sudo; `SGPU_REQUIRE_ROOT_TESTS=1` makes missing
privileges an error. Warm local runs normally take under a minute.

CI uses one job on pull requests or manual dispatch. No GPU or NFS service is
required in CI; actual NFS publication is checked on the affected deployment.
