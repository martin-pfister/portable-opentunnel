#!/usr/bin/env bash
# Confirm the running container is the pinned AlmaLinux 8.8 x86-64 image before
# any acceptance test trusts its userland.
#
# This deliberately reads /etc/os-release instead of trusting the image tag or
# an environment variable: the point of the gate is to notice when CI is not
# actually running inside the intended OS.
#
# Usage: verify-os-release.sh [--expect-id ID] [--expect-version VERSION]
set -euo pipefail

expect_id=almalinux
expect_version=8.8
os_release=${OS_RELEASE_FILE:-/etc/os-release}

while [[ $# -gt 0 ]]; do
  case $1 in
    --expect-id) expect_id=${2:?--expect-id needs a value}; shift 2 ;;
    --expect-version) expect_version=${2:?--expect-version needs a value}; shift 2 ;;
    *) echo "usage: verify-os-release.sh [--expect-id ID] [--expect-version VERSION]" >&2; exit 2 ;;
  esac
done

if [[ ! -r $os_release ]]; then
  echo "verify-os-release: cannot read $os_release; this is not the expected container" >&2
  exit 1
fi

# Parse without sourcing: os-release is data, and a hostile file must not run.
get() {
  sed -n "s/^$1=//p" "$os_release" | head -n 1 | tr -d '"'
}

id=$(get ID)
version_id=$(get VERSION_ID)
pretty=$(get PRETTY_NAME)
arch=$(uname -m)

printf 'os-release: ID=%s VERSION_ID=%s ARCH=%s (%s)\n' \
  "${id:-<unset>}" "${version_id:-<unset>}" "$arch" "${pretty:-<unset>}"

status=0

if [[ $id != "$expect_id" ]]; then
  echo "verify-os-release: expected ID '$expect_id', found '${id:-<unset>}'" >&2
  status=1
fi

if [[ $version_id != "$expect_version" ]]; then
  echo "verify-os-release: expected VERSION_ID '$expect_version', found '${version_id:-<unset>}'" >&2
  status=1
fi

if [[ $arch != "x86_64" ]]; then
  echo "verify-os-release: expected x86_64, found '$arch'" >&2
  status=1
fi

if [[ $status -ne 0 ]]; then
  echo "verify-os-release: this is not the pinned AlmaLinux 8.8 x86-64 test environment" >&2
  exit 1
fi

echo "verify-os-release: pinned AlmaLinux 8.8 x86-64 confirmed"
