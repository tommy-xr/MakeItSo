#!/bin/sh
# Pack mis, mis-worker and mis-orchestrator into <out dir>/mis-<target>.tar.gz.
#   release/package.sh <bin dir> <target> <out dir> [exe suffix, e.g. .exe]
set -eu
bin=$1 target=$2 out=$3 ext=${4:-}
mkdir -p "$out"
tar -C "$bin" -czf "$out/mis-$target.tar.gz" "mis$ext" "mis-worker$ext" "mis-orchestrator$ext"
