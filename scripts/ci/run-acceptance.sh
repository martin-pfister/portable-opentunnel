#!/usr/bin/env bash
# Run the acceptance tests inside the container started by run-ci-container.sh.
#
# Usage: run-acceptance.sh --manifest FILE --binary FILE --scratch DIR
#
#   --manifest  TSV from build-static.sh (name <TAB> test executable)
#   --binary    the static release executable
#   --scratch   the only writable directory
#
# Each test executable gets a fresh HOME, TMPDIR and XDG directories under the
# scratch directory. Integration tests launch the release binary rather than a
# debug build, and the run fails unless at least one did. The CLI contract is
# then checked against the same binary.
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

manifest=
binary=
scratch=

while [[ $# -gt 0 ]]; do
  case $1 in
    --manifest) manifest=${2:?--manifest needs a value}; shift 2 ;;
    --binary) binary=${2:?--binary needs a value}; shift 2 ;;
    --scratch) scratch=${2:?--scratch needs a value}; shift 2 ;;
    -h|--help) sed -n '2,13p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "run-acceptance: unknown argument: $1" >&2; exit 2 ;;
  esac
done

for required in manifest binary scratch; do
  [[ -n ${!required} ]] || { echo "run-acceptance: --$required is required" >&2; exit 2; }
done
[[ -s $manifest ]] || { echo "run-acceptance: missing or empty manifest: $manifest" >&2; exit 1; }
[[ -x $binary ]] || { echo "run-acceptance: not executable: $binary" >&2; exit 1; }
# Tests launch the binary from their own working directories.
binary=$(realpath "$binary")

grep -h PRETTY_NAME /etc/os-release || true

used="$scratch/release-binary-used.log"
: > "$used"
failures=0

while IFS=$'\t' read -r name path; do
  home="$scratch/home/$name"
  rm -rf "$home"
  mkdir -p "$home/tmp" "$home/xdg-cache" "$home/xdg-data"
  echo "run-acceptance: running $name"
  if ! env -i \
      PATH=/usr/local/bin:/usr/bin:/bin \
      HOME="$home" \
      TMPDIR="$home/tmp" \
      XDG_CACHE_HOME="$home/xdg-cache" \
      XDG_DATA_HOME="$home/xdg-data" \
      RUST_BACKTRACE=1 \
      PORTABLE_OPENTUNNEL_TEST_BINARY="$binary" \
      PORTABLE_OPENTUNNEL_TEST_BINARY_USED="$used" \
      PORTABLE_OPENTUNNEL_TEST_HOST="${PORTABLE_OPENTUNNEL_TEST_HOST:-localhost}" \
      "$path" --test-threads="${RUST_TEST_THREADS:-4}"; then
    echo "run-acceptance: FAILED: $name" >&2
    failures=$((failures + 1))
  fi
done < "$manifest"

if [[ -s $used ]]; then
  echo "run-acceptance: integration tests launched the release binary $(wc -l < "$used") times"
else
  echo "run-acceptance: FAILED: no integration test launched the release binary" >&2
  failures=$((failures + 1))
fi

if ! "$here/check-cli-contract.sh" "$binary" "$scratch"; then
  failures=$((failures + 1))
fi

if [[ $failures -ne 0 ]]; then
  echo "run-acceptance: $failures failure(s)" >&2
  exit 1
fi
echo "run-acceptance: all checks passed"
