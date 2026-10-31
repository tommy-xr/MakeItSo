#!/bin/sh
# Print the version a release tarball's mis-orchestrator reports, e.g. 0.1.0+1a2b3c4.
# Release names and signatures use it, so they always match what the binaries say.
#   release/version.sh <tarball with a binary runnable here>
set -eu
die() { echo "::error::version: $*" >&2; exit 1; }
x=$(mktemp -d)
trap 'rm -rf "$x"' EXIT
tar -xzf "$1" -C "$x" mis-orchestrator
v=$("$x/mis-orchestrator" --version | sed -n 's/^mis-orchestrator //p')
# A dirty or sha-less build can't be fetched by version, so it is never published.
echo "$v" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+\+[0-9a-f]{7}$' || die "unexpected version '$v'"
echo "$v"
