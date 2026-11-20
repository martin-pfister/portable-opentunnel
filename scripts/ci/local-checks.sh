#!/usr/bin/env bash
# Local pre-push mirror of CI. Runs the same gates that pull requests run, so a
# failure is found before pushing rather than in Actions.
#
# Usage: scripts/ci/local-checks.sh [--no-musl]
#
# What it runs:
#   1. cargo fmt --check
#   2. cargo clippy --all-targets --locked -- -D warnings
#   3. cargo deny check (dependency licenses, advisories, sources, bans)
#   4. scripts/ci/test-static.sh: the static release build, the test suite
#      against it, and the CLI contract, exactly as CI runs them.
#      With --no-musl, plain `cargo test` on the host target instead.
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$here/../.." && pwd)
cd "$repo_root"

run_musl=1
for arg in "$@"; do
  case $arg in
    --no-musl) run_musl=0 ;;
    -h|--help) sed -n '2,13p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "local-checks: unknown argument: $arg" >&2; exit 2 ;;
  esac
done

step() { printf '\n=== %s ===\n' "$1"; }

step "rustc and cargo versions"
rustc --version
cargo --version

step "format"
cargo fmt --all --check

step "clippy (warnings denied)"
cargo clippy --all-targets --locked --all-features -- -D warnings

step "dependency licenses, advisories, bans, sources"
if cargo deny --version >/dev/null 2>&1; then
  cargo deny --all-features --locked check
else
  cat >&2 <<'MSG'
local-checks: cargo-deny is not installed, so the dependency license/security
gate did not run. Install it with:
    cargo install cargo-deny --locked --version 0.21.0
For manual review without cargo-deny, see docs/ci.md#dependency-policy.
MSG
  exit 1
fi

if [[ $run_musl -eq 1 ]]; then
  step "tests against the static release build"
  "$here/test-static.sh"
else
  step "host tests (--no-musl)"
  cargo test --locked
fi

printf '\nlocal-checks: passed.\n'
