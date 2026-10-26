#!/usr/bin/env bash
# The local gate, replacing hosted CI (.github/workflows/ci.yml is manual-only
# and Actions is disabled for the repository).
#
#   bash scripts/check.sh
#
# Runs the stages the hosted CI ran, with the same flags:
#   cargo fmt --check
#   cargo clippy --workspace --all-targets -- -D warnings
#   cargo test --workspace
# Unlike CI, every stage runs even when an earlier one fails, so one failure
# never hides another. Each stage's own exit status is printed, and the script
# exits non-zero if any stage failed.
#
# CI ran on Ubuntu and on Windows because breaks that show on one OS only are
# real in this repo ([R:xplat-fixtures]). Run it on both where you can: Git Bash
# on Windows, and a Linux shell (WSL counts). On Linux the keyring crate needs
# libdbus-1-dev and pkg-config.

set -u

cd "$(dirname "$0")/../rust" || exit 2

failed=()

stage() {
    local name=$1
    shift
    echo "== $name: $*"
    "$@"
    local rc=$?
    echo "== $name exit $rc"
    if [ "$rc" -ne 0 ]; then
        failed+=("$name")
    fi
}

stage rustfmt cargo fmt --check
stage clippy cargo clippy --workspace --all-targets -- -D warnings
stage test cargo test --workspace

if [ "${#failed[@]}" -eq 0 ]; then
    echo "PASS: rustfmt, clippy, test"
    exit 0
fi
echo "FAIL: ${failed[*]}"
exit 1
