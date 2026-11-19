#!/usr/bin/env python3
"""Collect Cargo test executables from `cargo test --no-run` JSON messages.

Cargo decides which targets are tests; this script never names them. It reads
the machine-readable messages Cargo already emits and writes a manifest that a
shell loop can execute inside the acceptance-test container.

Input:  a file containing the JSON stream (one message per line), or stdin.
Output: a TSV manifest on stdout: name <TAB> absolute path

A message qualifies when it is a `compiler-artifact` with a non-null
`executable` **and** `profile.test` is true.

Filtering on `target.kind` would be wrong: Cargo reports a library's unit-test
harness with `kind: ["lib"]` and a binary's unit-test harness with
`kind: ["bin"]`, so a kind-based filter silently skips every unit test. Only
`profile.test` distinguishes "compiled as a test harness" from "compiled as the
real artifact".

This still excludes the plain release/debug binary, which `cargo test --no-run`
also builds (to prove it compiles) with `profile.test: false`. Running that
binary would start forwarding a tunnel instead of running assertions; the
release binary is exercised separately by the CLI smoke checks in
scripts/ci/run-acceptance.sh.

Usage:
    cargo test --no-run --locked --target x86_64-unknown-linux-musl \\
        --message-format=json-render-diagnostics > artifacts.json
    scripts/ci/test-manifest.py artifacts.json > manifest.tsv

The script fails when no test executable is found so an empty run cannot pass
silently. `--require-target` fails when an executable was built for a different
target, which catches the "only host GNU test binaries ran" mistake.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def read_messages(stream) -> list[dict]:
    messages = []
    for line_number, line in enumerate(stream, start=1):
        line = line.strip()
        if not line:
            continue
        # Cargo interleaves rendered diagnostics on stdout ahead of the JSON
        # payload, so a message line may not start with '{'. Only attempt to
        # decode when the line contains a JSON object; anything else is a
        # diagnostic and is ignored.
        start = line.find("{")
        if start < 0:
            continue
        try:
            messages.append(json.loads(line[start:]))
        except json.JSONDecodeError:
            # A rendered diagnostic may legitimately contain a brace (for
            # example a code snippet). Treat an undecodable line as a diagnostic
            # unless it looks like it started as a Cargo message.
            if line.startswith('{"reason"'):
                raise SystemExit(
                    "test-manifest: "
                    f"cannot parse Cargo message on line {line_number}: {line!r}"
                )
    return messages


def collect(messages: list[dict], require_target: str | None) -> list[tuple[str, str]]:
    # Keyed by executable path: a name alone is not unique (a bin target and an
    # integration test may share it).
    found: dict[str, str] = {}
    for message in messages:
        if message.get("reason") != "compiler-artifact":
            continue
        executable = message.get("executable")
        if not executable:
            continue
        # The only reliable "this is a test harness" signal. See the module
        # docstring for why target.kind is not used.
        if not (message.get("profile") or {}).get("test"):
            continue
        target = message.get("target") or {}
        name = target.get("name") or Path(executable).stem
        if require_target and f"/{require_target}/" not in executable:
            raise SystemExit(
                f"test-manifest: {name} was built for the wrong target: {executable}"
            )
        found[executable] = name

    # Disambiguate repeated names so log output identifies each harness.
    seen: dict[str, int] = {}
    entries: list[tuple[str, str]] = []
    for path, name in sorted(found.items()):
        seen[name] = seen.get(name, 0) + 1
        label = name if seen[name] == 1 else f"{name}-{seen[name]}"
        entries.append((label, path))
    return sorted(entries)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "messages",
        nargs="?",
        default="-",
        help="Cargo JSON message file, or '-' for stdin",
    )
    parser.add_argument(
        "--require-target",
        metavar="TRIPLE",
        help="fail unless each path was built under this target directory name",
    )
    args = parser.parse_args()

    if args.messages == "-":
        entries = collect(read_messages(sys.stdin), args.require_target)
    else:
        with open(args.messages, encoding="utf-8") as stream:
            entries = collect(read_messages(stream), args.require_target)

    if not entries:
        print(
            "test-manifest: no test executables in the Cargo output; "
            "refusing to report an empty test run",
            file=sys.stderr,
        )
        return 1

    for name, path in entries:
        print(f"{name}\t{path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
