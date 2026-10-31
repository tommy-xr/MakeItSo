#!/bin/sh
# Sign every <dir>/*.tar.gz with minisign and write a signed checksum list.
#   release/sign.sh <dir> <version> <sums file name>
# Each <file>.minisig has the trusted comment "mis <version> <file>". Updaters require it, so a
# signed asset can't be passed off as another version or target.
# Env: MINISIGN_SECRET_KEY  unencrypted secret key file contents (`minisign -G -W`); required.
#      MINISIGN_PUBLIC_KEY  public key file each signature must verify with (release/minisign.pub).
set -eu
die() { echo "::error::sign: $*" >&2; exit 1; }
dir=$1 version=$2 sums=$3
[ -n "${MINISIGN_SECRET_KEY:-}" ] || die "MINISIGN_SECRET_KEY is not set; refusing to publish unsigned assets"
pub=${MINISIGN_PUBLIC_KEY:-$(dirname "$0")/minisign.pub}
pub=$(cd "$(dirname "$pub")" && pwd)/$(basename "$pub")
key=$(mktemp)
trap 'rm -f "$key"' EXIT
chmod 600 "$key"
printf '%s\n' "$MINISIGN_SECRET_KEY" >"$key"
cd "$dir"
set -- *.tar.gz
[ -f "$1" ] || die "no .tar.gz in $dir"
shasum -a 256 "$@" >"$sums"
for f in "$@" "$sums"; do
  minisign -S -s "$key" -m "$f" -x "$f.minisig" -t "mis $version $f" </dev/null
  # Catches a secret that doesn't match the committed public key before anything is published.
  minisign -V -q -p "$pub" -m "$f" -x "$f.minisig" || die "$f does not verify with $pub"
done
