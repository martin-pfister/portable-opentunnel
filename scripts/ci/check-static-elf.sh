#!/usr/bin/env bash
# Verify an ELF is fully static: no interpreter (no dynamic loader) and no
# DT_NEEDED entries. The musl target name alone does not prove this, so the
# check reads the actual files.
#
# Usage: check-static-elf.sh <path-to-elf> [<more paths>...]
#
# Fails when any file is:
#   - missing or not an ELF executable/shared object
#   - dynamically linked (has PT_INTERP)
#   - linked against shared libraries (has DT_NEEDED)
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: check-static-elf.sh <path-to-elf> [<path>...]" >&2
  exit 2
fi

for tool in readelf file stat; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "check-static-elf: $tool (binutils/coreutils) is required" >&2
    exit 1
  fi
done

failed=()

for binary in "$@"; do
  if [[ ! -f $binary ]]; then
    echo "check-static-elf: missing file: $binary" >&2
    failed+=("$binary")
    continue
  fi

  description=$(file -b "$binary")
  case $description in
    ELF*executable* | ELF*shared\ object*) ;;
    *)
      echo "check-static-elf: $binary is not an ELF executable: $description" >&2
      failed+=("$binary")
      continue
      ;;
  esac

  if readelf -lW "$binary" | grep -q 'INTERP'; then
    echo "check-static-elf: $binary has a dynamic loader (PT_INTERP)" >&2
    readelf -lW "$binary" | grep 'INTERP' | sed 's/^/    /' >&2
    failed+=("$binary")
    continue
  fi

  if readelf -dW "$binary" 2>/dev/null | grep -q '(NEEDED)'; then
    echo "check-static-elf: $binary requires shared libraries (DT_NEEDED)" >&2
    readelf -dW "$binary" | grep '(NEEDED)' | sed 's/^/    /' >&2
    failed+=("$binary")
    continue
  fi

  echo "check-static-elf: static: $binary ($(stat -c '%s' "$binary") bytes)"
done

if [[ ${#failed[@]} -gt 0 ]]; then
  printf 'check-static-elf: %d file(s) not statically linked\n' "${#failed[@]}" >&2
  exit 1
fi
