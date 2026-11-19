#!/usr/bin/env bash
# Run the AlmaLinux 8.8 acceptance tests locally using any OCI-compatible
# runtime on PATH (docker, podman, or nerdctl). CI is the authoritative run;
# this exists so the same gates can be reproduced before pushing.
#
# Usage: scripts/ci/run-ci-container.sh [--runtime docker|podman|nerdctl] [--keep]
#
# Preconditions:
#   - a runtime is installed and the daemon is reachable
#   - the network can pull docker.io/library/almalinux by manifest digest
#   - the static musl test binaries and release binary are already built
#
# The container runs as the calling user (--user uid:gid), not root, so
# permission tests that expect EACCES are meaningful; and with a controlled
# /etc/hosts alias for the host-target test. Both match the GitHub workflow.
#
# Steps:
#   1. build static test executables and a manifest (host GNU toolchain is only
#      used to orchestrate; the binaries themselves are musl)
#   2. build the static release binary and strip it
#   3. start the pinned image with the repo bind-mounted read-only and a
#      writable scratch directory, then run scripts/ci/run-acceptance.sh inside
#
# Nothing here runs on GitHub-hosted infrastructure automatically, and nothing
# here contacts the hosted OpenTunnel service.
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$here/../.." && pwd)
cd "$repo_root"

runtime=
keep=0

while [[ $# -gt 0 ]]; do
  case $1 in
    --runtime) runtime=${2:?--runtime needs a value}; shift 2 ;;
    --keep) keep=1; shift ;;
    -h|--help) sed -n '2,25p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "run-ci-container: unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [[ -z $runtime ]]; then
  for candidate in docker podman nerdctl; do
    if command -v "$candidate" >/dev/null 2>&1; then runtime=$candidate; break; fi
  done
fi

if [[ -z $runtime ]]; then
  cat >&2 <<'MSG'
run-ci-container: no container runtime found (looked for docker, podman, nerdctl).
The AlmaLinux 8.8 acceptance tests therefore did not run. Either install a
runtime, or rely on the GitHub Actions container job, which does run them.
MSG
  exit 1
fi

image=$("$here/container-ref.sh")
target=x86_64-unknown-linux-musl

echo "run-ci-container: runtime=$runtime"
echo "run-ci-container: image=$image"

# shellcheck source=/dev/null
. "$here/musl-env.sh"

out_dir="$repo_root/.ci-out"
rm -rf "$out_dir"
mkdir -p "$out_dir"

echo "run-ci-container: building static test executables"
cargo test --no-run --locked --target "$target" --message-format=json \
  > "$out_dir/test-messages.json"
python3 "$here/test-manifest.py" --require-target "$target" \
  "$out_dir/test-messages.json" > "$out_dir/manifest.tsv"

echo "run-ci-container: building static release executable"
cargo build --release --locked --target "$target"
strip "$repo_root/target/$target/release/portable-opentunnel"
"$here/check-static-elf.sh" "$repo_root/target/$target/release/portable-opentunnel"

# Test executables and their manifest carry absolute host paths. Mount the
# repository at the *same* absolute path inside the container, otherwise those
# paths do not resolve there. The repository is read-only; only a separate
# scratch directory is writable, so a stray write is contained.
container_name=portable-opentunnel-acceptance-$$
scratch=$(mktemp -d "${TMPDIR:-/tmp}/portable-opentunnel-scratch.XXXXXX")
cleanup() {
  if [[ $keep -eq 0 ]]; then
    "$runtime" rm -f "$container_name" >/dev/null 2>&1 || true
  fi
  rm -rf "$scratch"
}
trap cleanup EXIT

# Runs unprivileged, matching the GitHub workflow: docker's default root
# bypasses filesystem permission checks, so a test expecting `EACCES` would
# spuriously pass. The host uid/gid owns the mounted scratch tree.
args=(
  run --rm --name "$container_name"
  --network none
  --user "$(id -u):$(id -g)"
  # Controlled /etc/hosts name for the host-target test; no external DNS.
  --add-host portable-opentunnel-fixture:127.0.0.1
  -e PORTABLE_OPENTUNNEL_TEST_HOST=portable-opentunnel-fixture
  -v "$repo_root:$repo_root:ro"
  -v "$scratch:/scratch:rw"
  -w "$repo_root"
  "$image"
  bash "$repo_root/scripts/ci/run-acceptance.sh"
  --manifest "$out_dir/manifest.tsv"
  --binary "$repo_root/target/$target/release/portable-opentunnel"
  --scratch /scratch
  --require-release-exercised
)

echo "run-ci-container: starting $image"
set +e
"$runtime" "${args[@]}"
status=$?
set -e

if [[ $keep -ne 0 ]]; then
  echo "run-ci-container: kept container $container_name"
fi

if [[ $status -ne 0 ]]; then
  echo "run-ci-container: acceptance run failed (exit $status)" >&2
  exit "$status"
fi

echo "run-ci-container: acceptance run passed inside the pinned image"
