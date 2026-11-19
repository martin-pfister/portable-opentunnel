#!/usr/bin/env bash
# Select a musl-targeting toolchain for x86_64-unknown-linux-musl builds.
#
# Source this file; it exports variables for the current shell:
#
#   . scripts/ci/musl-env.sh
#
# Why this exists:
#   rustc uses `cc` as the linker driver for the musl target. On a glibc host,
#   plain `cc` is the GNU toolchain, and the `cc` crate would also pick it for
#   ring's C and assembly sources. Installing `musl-tools` provides `musl-gcc`,
#   which is the driver that matches the target's libc. Setting it explicitly
#   removes the guesswork and makes local runs match CI.
#
# Nothing here is required if your environment already works: the variables are
# only set when the tool is present, and are otherwise left alone with a warning
# so the failure is visible rather than silent.

MUSL_TARGET=${MUSL_TARGET:-x86_64-unknown-linux-musl}

# Persist a variable for later CI steps when running under GitHub Actions, where
# each step is a separate shell. Locally this is a no-op.
persist() {
  if [[ -n ${GITHUB_ENV:-} ]]; then
    printf '%s=%s\n' "$1" "$2" >> "$GITHUB_ENV"
  fi
}

if command -v musl-gcc >/dev/null 2>&1; then
  MUSL_CC=$(command -v musl-gcc)
  MUSL_TARGET_ENV=$(printf '%s' "$MUSL_TARGET" | tr 'a-z-' 'A-Z_')
  linker_var="CARGO_TARGET_${MUSL_TARGET_ENV}_LINKER"
  cc_var="CC_${MUSL_TARGET_ENV}"
  export "$linker_var=$MUSL_CC"
  # The `cc` crate reads CC_<target> with the same upper-cased spelling.
  export "$cc_var=$MUSL_CC"
  persist "$linker_var" "$MUSL_CC"
  persist "$cc_var" "$MUSL_CC"
  echo "musl-env: using $MUSL_CC for $MUSL_TARGET"
else
  echo "musl-env: warning: musl-gcc not found; relying on the default cc driver." >&2
  echo "musl-env: install musl-tools (Debian/Ubuntu) or the equivalent for a" >&2
  echo "musl-env: reproducible static build with ring's C and assembly sources." >&2
fi
