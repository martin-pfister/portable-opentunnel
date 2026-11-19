#!/usr/bin/env bash
# Print the container image reference pinned in container.lock.
#
# Usage: container-ref.sh [--list]
#   (default) print the x86-64 platform manifest reference used by CI
#   --list    print the multi-arch list reference (provenance only)
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
lock="$here/container.lock"

[[ -f $lock ]] || { echo "missing $lock" >&2; exit 1; }

value() {
  # Read a flat `key=value` line without sourcing the file.
  sed -n "s/^$1=//p" "$lock" | head -n 1
}

registry=$(value registry)
repository=$(value repository)
amd64=$(value amd64_digest)
list=$(value list_digest)

[[ -n $registry && -n $repository ]] || { echo "container.lock is incomplete" >&2; exit 1; }

case "${1:-}" in
  "") [[ -n $amd64 ]] || { echo "container.lock has no amd64_digest" >&2; exit 1; }
      printf '%s/%s@%s\n' "$registry" "$repository" "$amd64" ;;
  --list) [[ -n $list ]] || { echo "container.lock has no list_digest" >&2; exit 1; }
      printf '%s/%s@%s\n' "$registry" "$repository" "$list" ;;
  *) echo "usage: container-ref.sh [--list]" >&2; exit 2 ;;
esac
