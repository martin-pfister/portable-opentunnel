#!/usr/bin/env bash
# Build the static release binary, then run the whole test suite, compiled for
# musl, against it. CI runs exactly this. Needs musl-tools and binutils.
#
# Usage: scripts/ci/test-static.sh
#
# Integration tests launch the release binary instead of a debug build. Set
# PORTABLE_OPENTUNNEL_TEST_HOST to a name mapped to 127.0.0.1 in /etc/hosts to
# also test a target given by a hostname other than localhost.
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$here/../.." && pwd)
cd "$repo_root"

target=x86_64-unknown-linux-musl
binary="$repo_root/target/$target/release/portable-opentunnel"

# shellcheck source=/dev/null
. "$here/musl-env.sh"

cargo build --locked --release --target "$target"
"$here/check-static-elf.sh" "$binary"

PORTABLE_OPENTUNNEL_TEST_BINARY="$binary" cargo test --locked --target "$target"

scratch=$(mktemp -d "${TMPDIR:-/tmp}/portable-opentunnel-cli.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
"$here/check-cli-contract.sh" "$binary" "$scratch"
