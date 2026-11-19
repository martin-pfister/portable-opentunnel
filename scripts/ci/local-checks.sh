#!/usr/bin/env bash
# Local pre-push mirror of CI. Runs the same gates that pull requests run, so a
# failure is found before pushing rather than in Actions.
#
# Usage: scripts/ci/local-checks.sh [--no-musl]
#
# What it runs:
#   1. cargo fmt --check
#   2. cargo clippy --all-targets --locked -- -D warnings
#   3. cargo test --locked (host target; fast feedback)
#   4. cargo deny check (dependency licenses, advisories, sources, bans)
#   5. with musl tooling present: build the static release binary and verify
#      that it has no loader and no DT_NEEDED entries
#
# It does not claim the AlmaLinux container acceptance tests ran. Those need the
# pinned container and are executed by CI (or run-ci-container.sh locally).
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$here/../.." && pwd)
cd "$repo_root"

run_musl=1
for arg in "$@"; do
  case $arg in
    --no-musl) run_musl=0 ;;
    -h|--help) sed -n '2,16p' "${BASH_SOURCE[0]}"; exit 0 ;;
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

step "host tests"
cargo test --locked

step "dependency licenses, advisories, bans, sources"
if cargo deny --version >/dev/null 2>&1; then
  cargo deny --all-features check
else
  cat >&2 <<'MSG'
local-checks: cargo-deny is not installed, so the dependency license/security
gate did not run. Install it with:
    cargo install cargo-deny --locked --version 0.21.0
For manual review without cargo-deny, see README.md "Dependency license and
security checks".
MSG
  exit 1
fi

if [[ $run_musl -eq 1 ]]; then
  step "static release build (x86_64-unknown-linux-musl)"
  if ! rustup target list --installed | grep -qx x86_64-unknown-linux-musl; then
    echo "local-checks: musl target not installed; run:" >&2
    echo "    rustup target add x86_64-unknown-linux-musl" >&2
    echo "and install a musl C compiler (musl-tools) for ring's C/asm build." >&2
    exit 1
  fi
  # shellcheck source=/dev/null
  . "$here/musl-env.sh"
  cargo build --release --locked --target x86_64-unknown-linux-musl
  "$here/check-static-elf.sh" "target/x86_64-unknown-linux-musl/release/portable-opentunnel"else
  step "static release build skipped (--no-musl)"
fi

printf '\nlocal-checks: passed. Container acceptance tests are not covered here.\n'
