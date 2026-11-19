#!/usr/bin/env bash
# Run acceptance tests INSIDE the pinned AlmaLinux 8.8 container.
#
# Inputs (required):
#   --manifest FILE   TSV from scripts/ci/test-manifest.py (name <TAB> path)
#   --binary FILE     release executable built for x86_64-unknown-linux-musl
#   --scratch DIR     writable directory inside the container
#
# Options:
#   --require-release-exercised
#       Fail unless an integration test actually launched the release binary.
#       Tests signal this by appending to $PORTABLE_OPENTUNNEL_TEST_BINARY_USED.
#   --skip-binary-smoke
#       Skip the direct CLI checks. Only for debugging a broken binary.
#
# Order: OS gate, loopback preflight, filesystem snapshot, tests, CLI checks,
# filesystem diff. A wrong image or an unusable loopback stack fails before any
# test output can be mistaken for acceptance evidence.
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

manifest=
binary=
scratch=
require_exercised=0
run_smoke=1

while [[ $# -gt 0 ]]; do
  case $1 in
    --manifest) manifest=${2:?--manifest needs a value}; shift 2 ;;
    --binary) binary=${2:?--binary needs a value}; shift 2 ;;
    --scratch) scratch=${2:?--scratch needs a value}; shift 2 ;;
    --require-release-exercised) require_exercised=1; shift ;;
    --skip-binary-smoke) run_smoke=0; shift ;;
    -h|--help) sed -n '2,20p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "run-acceptance: unknown argument: $1" >&2; exit 2 ;;
  esac
done

for required in manifest binary scratch; do
  [[ -n ${!required} ]] || { echo "run-acceptance: --$required is required" >&2; exit 2; }
done

"$here/verify-os-release.sh"

[[ -f $manifest ]] || { echo "run-acceptance: missing manifest: $manifest" >&2; exit 1; }
[[ -x $binary ]] || { echo "run-acceptance: not executable: $binary" >&2; exit 1; }
mkdir -p "$scratch"
# `find` emits absolute paths, so the prune list must be absolute too; a
# relative scratch path would silently fail to match and get flagged as a leak.
scratch=$(cd -- "$scratch" && pwd)

# ---------------------------------------------------------------- loopback
# The run normally uses `--network none`. Tests bind loopback fixtures, so a
# dead `lo` would turn every test into a confusing bind error. A closed port
# must report "refused" (stack present) rather than "unreachable".
loopback_state() {
  local message
  if message=$(bash -c 'exec 3<>/dev/tcp/127.0.0.1/9' 2>&1); then
    echo open
  elif [[ $message == *refused* ]]; then
    echo ok
  else
    echo "broken:$message"
  fi
}

state=$(loopback_state)
if [[ $state != ok && $state != open ]]; then
  for tool in ip /sbin/ip ifconfig /sbin/ifconfig; do
    command -v "$tool" >/dev/null 2>&1 || continue
    case $tool in
      *ifconfig) "$tool" lo up 2>/dev/null || true ;;
      *) "$tool" link set lo up 2>/dev/null || true ;;
    esac
  done
  state=$(loopback_state)
  if [[ $state != ok && $state != open ]]; then
    echo "run-acceptance: 127.0.0.1 is unusable in this container ($state)" >&2
    echo "run-acceptance: loopback-binding tests cannot run; refusing to report a" >&2
    echo "run-acceptance: misleading pass or a confusing bind failure" >&2
    exit 1
  fi
  echo "run-acceptance: brought lo up"
else
  echo "run-acceptance: loopback stack is usable"
fi

# ------------------------------------------------------- filesystem boundary
# Snapshot the real filesystem, then diff it after the tests.
#
# The purpose is narrow: notice *unexpected persistent changes* to paths this
# program should never touch. It is not a syscall-level proof of a write
# boundary, and it does not catch everything. Specifically:
#   - a file created and deleted during the run leaves no trace;
#   - only the paths below are excluded, and the scratch tree is the only place
#     writes are expected and permitted;
#   - directories the snapshot could not read are skipped, so anything inside
#     them is invisible. The count is reported at the end;
#   - directory mtimes are not compared, because creating a file legitimately
#     bumps its parent's mtime and that would flag every run.
#
# /tmp and /var/tmp are *not* excluded, only the scratch tree inside them is.
# They are exactly where a tool that ignores TMPDIR would leak, so pruning them
# wholesale would hide the bug this check exists to find.
excluded_paths=(
  /proc      # pseudo-filesystem: kernel state, not persistent storage
  /sys       # pseudo-filesystem: kernel state
  /dev       # device nodes, churned by the container runtime
  /run       # runtime state owned by container init, volatile by definition
  /var/log   # container and harness logging churn
  /var/cache # package-manager cache churn
)

find_error_log="$scratch/.fs-find-errors"
: > "$find_error_log"

snapshot() {
  local root=${FS_SNAPSHOT_ROOT:-/}
  local prune=()
  local path
  for path in "${excluded_paths[@]}" "$scratch"; do
    prune+=(-path "$path" -prune -o)
  done
  # Regular files: mode, size, mtime, path. Directories: path only.
  # `find` exits non-zero on directories it cannot read, which is expected when
  # walking a whole root; the guard keeps `pipefail` from aborting the run, and
  # the unreadable paths are counted afterwards instead of being forgotten.
  {
    find "$root" "${prune[@]}" -type f -printf 'f %m %s %T@ %p\n' 2>>"$find_error_log" || true
    find "$root" "${prune[@]}" -type d -printf 'd %p\n' 2>>"$find_error_log" || true
  } | LC_ALL=C sort -u
}

# A snapshot that silently traversed almost nothing would produce a vacuous
# diff and a false pass, so require several known-populated paths and a sane
# entry count before trusting the comparison.
snapshot_is_plausible() {
  local file=$1 root=${FS_SNAPSHOT_ROOT:-/}
  local expected entries
  for expected in /etc/passwd /etc /usr/bin /usr/lib; do
    if ! grep -qF " ${root%/}$expected" "$file"; then
      echo "run-acceptance: snapshot is missing ${root%/}$expected; it did not traverse the filesystem" >&2
      return 1
    fi
  done
  entries=$(wc -l < "$file")
  if [[ $entries -lt 20 ]]; then
    echo "run-acceptance: snapshot has only $entries entries; refusing to trust it" >&2
    return 1
  fi
  return 0
}

echo "run-acceptance: snapshotting the filesystem"
before="$scratch/.fs-before"
after="$scratch/.fs-after"
snapshot > "$before"
if ! snapshot_is_plausible "$before"; then
  # Refusing is better than "fixing" it into an empty comparison, which would
  # turn the boundary check into a no-op that always passes.
  echo "run-acceptance: refusing to run with an unusable filesystem snapshot" >&2
  exit 1
fi
echo "run-acceptance: snapshot covers $(wc -l < "$before") entries"

failures=0
run=0

# Exporting the release binary lets integration tests exercise the artifact that
# ships instead of a separate dev build. Tests that launch it must append one
# line per launch to the *_USED file; --require-release-exercised enforces it.
export PORTABLE_OPENTUNNEL_TEST_BINARY="$binary"
export PORTABLE_OPENTUNNEL_TEST_BINARY_USED="$scratch/release-binary-used.log"
: > "$PORTABLE_OPENTUNNEL_TEST_BINARY_USED"
# The controlled /etc/hosts name for the host-target test. A container run sets
# this to an alias added with `--add-host`; a plain local run falls back to
# localhost, which resolves without any external DNS either way. It is forwarded
# explicitly because the test environment below is built with `env -i`.
test_host=${PORTABLE_OPENTUNNEL_TEST_HOST:-localhost}
# These must not leak from the host into the test environment.
unset CARGO_BIN_EXE_portable-opentunnel || true

while IFS=$'\t' read -r name path; do
  [[ -n ${path:-} ]] || continue
  if [[ ! -x $path ]]; then
    echo "run-acceptance: manifest entry is not executable: $path" >&2
    failures=$((failures + 1))
    continue
  fi
  # A fresh HOME/TMPDIR/XDG per target: one harness cannot inherit state from
  # another, and any write outside those is caught by the snapshot diff.
  home="$scratch/home/$name"
  rm -rf "$home"
  mkdir -p "$home/tmp" "$home/xdg-cache" "$home/xdg-data"
  echo "run-acceptance: running test target: $name"
  run=$((run + 1))
  if ! env -i \
      PATH=/usr/local/bin:/usr/bin:/bin \
      HOME="$home" \
      TMPDIR="$home/tmp" \
      XDG_CACHE_HOME="$home/xdg-cache" \
      XDG_DATA_HOME="$home/xdg-data" \
      RUST_BACKTRACE=1 \
      PORTABLE_OPENTUNNEL_TEST_BINARY="$PORTABLE_OPENTUNNEL_TEST_BINARY" \
      PORTABLE_OPENTUNNEL_TEST_BINARY_USED="$PORTABLE_OPENTUNNEL_TEST_BINARY_USED" \
      PORTABLE_OPENTUNNEL_TEST_HOST="$test_host" \
      "$path" --test-threads="${RUST_TEST_THREADS:-4}"; then
    echo "run-acceptance: FAILED: $name"
    failures=$((failures + 1))
  fi
done < "$manifest"

if [[ $run -eq 0 ]]; then
  echo "run-acceptance: manifest had no runnable entries; refusing to pass" >&2
  exit 1
fi

if [[ $require_exercised -eq 1 ]]; then
  if [[ -s $PORTABLE_OPENTUNNEL_TEST_BINARY_USED ]]; then
    echo "run-acceptance: the release binary was exercised by integration tests"
    sed 's/^/run-acceptance:   launched /' "$PORTABLE_OPENTUNNEL_TEST_BINARY_USED"
  else
    echo "run-acceptance: FAILED: no integration test launched the release binary" >&2
    echo "run-acceptance: a test that does must append to \$PORTABLE_OPENTUNNEL_TEST_BINARY_USED" >&2
    failures=$((failures + 1))
  fi
fi

# ------------------------------------------------------------- CLI checks
# Delegated to its own script so that the runner stays focused on ordering and
# the filesystem boundary. Lifecycle coverage comes from the integration tests;
# this only pins the documented exit-status and stream contract.
if [[ $run_smoke -eq 1 ]]; then
  if ! "$here/check-cli-contract.sh" "$binary" "$scratch"; then
    failures=$((failures + 1))
  fi
fi

# ------------------------------------------------------------- fs diff
echo "run-acceptance: comparing the filesystem snapshot"
snapshot > "$after"
unexpected="$scratch/.fs-diff"
if LC_ALL=C diff -u "$before" "$after" > "$unexpected"; then
  echo "run-acceptance: no persistent changes found in the monitored paths"
else
  echo "run-acceptance: FAILED: persistent changes in monitored paths:" >&2
  grep -E '^[+-][^+-]' "$unexpected" | sed 's/^/run-acceptance:   /' >&2 || true
  failures=$((failures + 1))
fi

# State the limits of this check plainly, every run, so a pass is not read as
# more than it is.
unreadable=$(grep -c 'No such file\|Permission denied' "$find_error_log" 2>/dev/null || true)
unreadable=${unreadable:-0}
echo "run-acceptance: filesystem check scope: all paths except ${excluded_paths[*]}"
echo "run-acceptance:                          and the scratch tree $scratch"
echo "run-acceptance: unreadable directories skipped: $unreadable (contents not checked)"
echo "run-acceptance: limits: persistent changes only; a file created and removed"
echo "run-acceptance:         during the run leaves no trace, and this is not a"
echo "run-acceptance:         syscall-level proof of the application write boundary"

if [[ $failures -ne 0 ]]; then
  echo "run-acceptance: $failures failure(s)" >&2
  exit 1
fi

echo "run-acceptance: all checks passed in the pinned container"
