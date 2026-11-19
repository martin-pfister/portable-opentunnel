#!/usr/bin/env bash
# Check the documented CLI contract by running the release executable.
#
# Usage: check-cli-contract.sh <binary> <scratch-dir>
#
# This covers exit statuses and stdout/stderr separation only. It does not open
# a tunnel; lifecycle coverage comes from the integration tests, which launch
# the same binary (see PORTABLE_OPENTUNNEL_TEST_BINARY).
#
# Every run gets a throwaway HOME and TMPDIR inside <scratch-dir>.
set -euo pipefail

binary=${1:?usage: check-cli-contract.sh <binary> <scratch-dir>}
scratch=${2:?usage: check-cli-contract.sh <binary> <scratch-dir>}

[[ -x $binary ]] || { echo "check-cli-contract: not executable: $binary" >&2; exit 1; }

home="$scratch/cli-contract"
rm -rf "$home"
mkdir -p "$home/tmp"

failures=0

run() {
  env -i PATH=/usr/local/bin:/usr/bin:/bin HOME="$home" TMPDIR="$home/tmp" "$@"
}

fail() {
  echo "check-cli-contract: FAILED: $1" >&2
  failures=$((failures + 1))
}

# Informational flags must succeed and print something.
for flag in --help --version; do
  if out=$(run "$binary" "$flag" 2>&1) && [[ -n $out ]]; then
    echo "check-cli-contract: ok: $flag"
  else
    fail "$flag did not succeed with output"
  fi
done

# Invalid arguments must exit exactly 2. "Non-zero" would also accept a crash or
# an operational failure as a correct argument error.
expect_exit() {
  local expected=$1 label=$2
  shift 2
  local actual
  set +e
  run "$@" >/dev/null 2>&1
  actual=$?
  set -e
  if [[ $actual -eq $expected ]]; then
    echo "check-cli-contract: ok: $label exits $expected"
  else
    fail "$label exited $actual, expected $expected"
  fi
}

expect_exit 2 "invalid target" "$binary" not-a-port
expect_exit 2 "port 0" "$binary" 0
expect_exit 2 "init with --ephemeral" "$binary" init --ephemeral
expect_exit 2 "url with a target" "$binary" url 3000
for action in 3000 init url delete; do
  expect_exit 2 "$action without --directory" "$binary" "$action" \
    --api http://127.0.0.1:1 --startup-timeout 1 --shutdown-timeout 1
done

# `url` on absent state: exit 3, empty stdout, a diagnostic on stderr, and the
# directory must not be created (the command is read-only).
url_dir="$home/absent-state"
set +e
stdout=$(run "$binary" url --directory "$url_dir" 2>"$home/url.err")
code=$?
set -e
if [[ $code -ne 3 ]]; then
  fail "url on missing state exited $code, expected 3"
elif [[ -n $stdout ]]; then
  fail "url on missing state wrote to stdout"
elif [[ ! -s $home/url.err ]]; then
  fail "url on missing state printed no diagnostic"
elif [[ -e $url_dir ]]; then
  fail "url created $url_dir"
else
  echo "check-cli-contract: ok: url on missing state exits 3 and creates nothing"
fi

if [[ $failures -ne 0 ]]; then
  echo "check-cli-contract: $failures failure(s)" >&2
  exit 1
fi

echo "check-cli-contract: the CLI contract holds"
