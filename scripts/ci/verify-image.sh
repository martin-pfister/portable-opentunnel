#!/usr/bin/env bash
# Verify that an OCI image reference resolves to the pinned amd64 manifest
# digest in container.lock.
#
# Usage: verify-image.sh [--runtime docker|podman] [image-ref]
#
# With no image-ref, the pinned reference from container.lock is used, which is
# the interesting case: it proves the lock file, the registry, and what the local
# runtime actually pulled all agree.
#
# Docker and Podman normalize references differently (`almalinux@sha256:...` vs
# `docker.io/library/almalinux@sha256:...`), so this compares the digest itself,
# not the display string.
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

runtime=
ref=

while [[ $# -gt 0 ]]; do
  case $1 in
    --runtime) runtime=${2:?--runtime needs a value}; shift 2 ;;
    -h|--help) sed -n '2,14p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) ref=$1; shift ;;
  esac
done

if [[ -z $runtime ]]; then
  for candidate in docker podman; do
    if command -v "$candidate" >/dev/null 2>&1; then runtime=$candidate; break; fi
  done
fi
[[ -n $runtime ]] || { echo "verify-image: no docker or podman on PATH" >&2; exit 1; }

if [[ -z $ref ]]; then
  ref=$("$here/container-ref.sh")
fi

expected=$(sed -n 's/^amd64_digest=//p' "$here/container.lock" | head -n 1)
[[ -n $expected ]] || { echo "verify-image: container.lock has no amd64_digest" >&2; exit 1; }
expected=${expected#sha256:}

echo "verify-image: runtime=$runtime image=$ref"
echo "verify-image: expected amd64 digest sha256:$expected"

# The local image's digests, one per line, as bare hex.
actual=$("$runtime" image inspect \
  --format '{{range .RepoDigests}}{{println .}}{{end}}' "$ref" 2>/dev/null \
  | sed 's/.*@sha256://' | sed '/^$/d' || true)

if [[ -z $actual ]]; then
  echo "verify-image: no RepoDigests for $ref; pull it by digest first" >&2
  exit 1
fi

if ! printf '%s\n' "$actual" | grep -Fxq "$expected"; then
  echo "verify-image: digest mismatch" >&2
  printf '  actual: %s\n' $actual >&2
  echo "  The image no longer matches scripts/ci/container.lock." >&2
  echo "  Do not edit the lock file to match a moving tag without reviewing the change." >&2
  exit 1
fi

platform=$("$runtime" image inspect --format '{{.Os}}/{{.Architecture}}' "$ref")
if [[ $platform != "linux/amd64" ]]; then
  echo "verify-image: expected linux/amd64, got $platform" >&2
  exit 1
fi

echo "verify-image: digest and platform match container.lock ($platform)"
