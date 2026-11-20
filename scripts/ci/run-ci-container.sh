#!/usr/bin/env bash
# Run the acceptance tests inside the pinned AlmaLinux 8.8 image. CI uses this
# too, so a local run matches it.
#
# Usage: scripts/ci/run-ci-container.sh [--runtime docker|podman|nerdctl] [--no-build]
#
# Builds the static executables with build-static.sh (unless --no-build), then
# runs run-acceptance.sh in the container:
#   - by manifest digest, so the image cannot change under a tag;
#   - as the calling uid/gid, because root bypasses permission checks and a
#     test expecting EACCES would pass for the wrong reason;
#   - with --network none and a controlled /etc/hosts name, so no test can
#     reach the internet or depend on external DNS;
#   - with a read-only root and the checkout mounted read-only at its own path
#     (test executables embed absolute paths). The scratch directory is the
#     only writable place, so a write anywhere else, /tmp included, fails.
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$here/../.." && pwd)
cd "$repo_root"

runtime=
build=1

while [[ $# -gt 0 ]]; do
  case $1 in
    --runtime) runtime=${2:?--runtime needs a value}; shift 2 ;;
    --no-build) build=0; shift ;;
    -h|--help) sed -n '2,16p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "run-ci-container: unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [[ -z $runtime ]]; then
  for candidate in docker podman nerdctl; do
    if command -v "$candidate" >/dev/null 2>&1; then runtime=$candidate; break; fi
  done
fi
if [[ -z $runtime ]]; then
  echo "run-ci-container: no container runtime found (looked for docker, podman, nerdctl)" >&2
  exit 1
fi

image=$(sed -n 's/^image=//p' "$here/container.lock")
[[ -n $image ]] || { echo "run-ci-container: container.lock has no image" >&2; exit 1; }

if [[ $build -eq 1 ]]; then
  "$here/build-static.sh"
fi

scratch=$(mktemp -d "${TMPDIR:-/tmp}/portable-opentunnel-scratch.XXXXXX")
trap 'rm -rf "$scratch"' EXIT

echo "run-ci-container: $runtime run $image"
"$runtime" run --rm \
  --read-only \
  --network none \
  --user "$(id -u):$(id -g)" \
  --add-host portable-opentunnel-fixture:127.0.0.1 \
  -e PORTABLE_OPENTUNNEL_TEST_HOST=portable-opentunnel-fixture \
  -v "$repo_root:$repo_root:ro" \
  -v "$scratch:/scratch:rw" \
  -w "$repo_root" \
  "$image" \
  bash "$here/run-acceptance.sh" \
    --manifest "$repo_root/.ci-out/manifest.tsv" \
    --binary "$repo_root/target/x86_64-unknown-linux-musl/release/portable-opentunnel" \
    --scratch /scratch
