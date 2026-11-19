#!/usr/bin/env bash
# Package a release executable with the metadata required by the release
# contract: versioned archive, SHA-256 checksum, commit identifier, target
# triple, and the actual byte size. It also re-checks static linkage so an
# artifact is never published with a hidden dynamic loader.
#
# Usage:
#   package-artifact.sh --binary PATH --target TRIPLE --outdir DIR [--version VERSION]
#
# --version defaults to `cargo metadata`'s package version when Cargo metadata
# is available, otherwise to "0.0.0-unknown". Commit and build metadata come from
# the environment when set: GITHUB_SHA (fallback: git rev-parse HEAD) and
# GITHUB_RUN_ID / GITHUB_RUN_ATTEMPT (empty outside CI).
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$here/../.." && pwd)

binary=
target=
outdir=
version=

while [[ $# -gt 0 ]]; do
  case $1 in
    --binary) binary=${2:?--binary needs a value}; shift 2 ;;
    --target) target=${2:?--target needs a value}; shift 2 ;;
    --outdir) outdir=${2:?--outdir needs a value}; shift 2 ;;
    --version) version=${2:?--version needs a value}; shift 2 ;;
    -h|--help) sed -n '2,16p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "package-artifact: unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ -n $binary ]] || { echo "package-artifact: --binary is required" >&2; exit 2; }
[[ -n $target ]] || { echo "package-artifact: --target is required" >&2; exit 2; }
[[ -n $outdir ]] || { echo "package-artifact: --outdir is required" >&2; exit 2; }
[[ -f $binary ]] || { echo "package-artifact: missing binary: $binary" >&2; exit 1; }

if [[ -z $version ]]; then
  version=$(cd "$repo_root" && cargo metadata --locked --no-deps --format-version 1 2>/dev/null \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])' 2>/dev/null || true)
  [[ -n $version ]] || version=0.0.0-unknown
fi

commit=${GITHUB_SHA:-}
if [[ -z $commit ]] && command -v git >/dev/null 2>&1 && git -C "$repo_root" rev-parse HEAD >/dev/null 2>&1; then
  commit=$(git -C "$repo_root" rev-parse HEAD)
fi
[[ -n $commit ]] || commit=unknown

name=portable-opentunnel
archive="$name-$version-$target"
size=$(stat -c '%s' "$binary")

mkdir -p "$outdir"
work=$(mktemp -d "${TMPDIR:-/tmp}/package-artifact.XXXXXX")
trap 'rm -rf "$work"' EXIT

cp "$binary" "$work/$name"
chmod 0755 "$work/$name"

# The published artifact must be statically linked; verify the copy that ships.
"$here/check-static-elf.sh" "$work/$name"

tar --sort=name --owner=0 --group=0 --numeric-owner --mtime='@0' -czf \
  "$outdir/$archive.tar.gz" -C "$work" "$name"

(
  cd "$outdir"
  sha256sum "$archive.tar.gz" > "$archive.tar.gz.sha256"
)

cat > "$outdir/$archive.metadata.json" <<EOF
{
  "name": "$name",
  "version": "$version",
  "target": "$target",
  "commit": "$commit",
  "run_id": "${GITHUB_RUN_ID:-}",
  "run_attempt": "${GITHUB_RUN_ATTEMPT:-}",
  "binary_bytes": $size,
  "archive": "$archive.tar.gz",
  "archive_sha256": "$(cut -d' ' -f1 "$outdir/$archive.tar.gz.sha256")",
  "linked": "static",
  "generated_by": "scripts/ci/package-artifact.sh"
}
EOF

echo "package-artifact: $outdir/$archive.tar.gz"
echo "package-artifact: $outdir/$archive.tar.gz.sha256"
echo "package-artifact: $outdir/$archive.metadata.json"
echo "package-artifact: target=$target commit=$commit binary_bytes=$size"
