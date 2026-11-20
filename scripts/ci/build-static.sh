#!/usr/bin/env bash
# Build the static musl test executables and release binary for acceptance.
#
# Usage: scripts/ci/build-static.sh
#
# Writes .ci-out/manifest.tsv (name <TAB> test executable), builds
# target/x86_64-unknown-linux-musl/release/portable-opentunnel, and fails
# unless every one of them is statically linked. Needs musl-tools and jq.
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$here/../.." && pwd)
cd "$repo_root"

target=x86_64-unknown-linux-musl
out_dir=.ci-out
rm -rf "$out_dir"
mkdir -p "$out_dir"

# shellcheck source=/dev/null
. "$here/musl-env.sh"

echo "build-static: building test executables"
cargo test --locked --no-run --target "$target" --message-format=json \
  > "$out_dir/test-messages.json"
# Only `profile.test` marks a test harness: unit-test harnesses report the
# kind of the target they test (`bin`), so filtering on kind would drop them.
jq -r 'select(.reason == "compiler-artifact" and .profile.test and .executable != null)
       | "\(.target.name)\t\(.executable)"' \
  "$out_dir/test-messages.json" > "$out_dir/manifest.tsv"
if [[ ! -s $out_dir/manifest.tsv ]]; then
  echo "build-static: cargo reported no test executables" >&2
  exit 1
fi
cat "$out_dir/manifest.tsv"

echo "build-static: building the release executable"
cargo build --locked --release --target "$target"

mapfile -t test_bins < <(cut -f2 "$out_dir/manifest.tsv")
"$here/check-static-elf.sh" "target/$target/release/portable-opentunnel" "${test_bins[@]}"
