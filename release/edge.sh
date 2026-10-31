#!/bin/sh
# Local steps of the rolling `edge` prerelease; the workflow makes the GitHub calls.
#   release/edge.sh stage <dir> <version> <commit>
#       Rename <dir>/mis-<target>.tar.gz to mis-<target>-<sha7>.tar.gz, sign them (sign.sh, sums
#       file SHA256SUMS-<sha7>) and write <dir>/edge.json:
#       {"sha": <full commit>, "version": "0.1.0+<sha7>", "assets": [<file names>]}
#   release/edge.sh prune <keep> <sha7 to keep> < <release JSON from the API>
#       Print the ids of assets outside the newest <keep> builds; <sha7 to keep> always stays.
set -eu
die() { echo "::error::edge: $*" >&2; exit 1; }
here=$(cd "$(dirname "$0")" && pwd)
case $1 in
stage)
  dir=$2 version=$3 commit=$4 sha=${3##*+}
  case $commit in "$sha"*) ;; *) die "version $version is not commit $commit" ;; esac
  for f in "$dir"/mis-*.tar.gz; do mv "$f" "${f%.tar.gz}-$sha.tar.gz"; done
  "$here/sign.sh" "$dir" "$version" "SHA256SUMS-$sha"
  (cd "$dir" && ls -- *"-$sha"*) | jq -R . | jq -s --arg sha "$commit" --arg v "$version" \
    '{sha: $sha, version: $v, assets: .}' >"$dir/edge.json"
  ;;
prune)
  jq -r --argjson keep "$2" --arg cur "$3" '
    [.assets[] | {id, at: .created_at,
      sha: (.name | capture("-(?<s>[0-9a-f]{7})(\\.tar\\.gz)?(\\.minisig)?$").s)}]
    | (group_by(.sha) | map({sha: .[0].sha, at: (map(.at) | max)})
       | sort_by(.at) | reverse | map(.sha)) as $newest
    | (([$cur] + ($newest - [$cur]))[:$keep]) as $kept
    | .[] | select(.sha as $s | $kept | any(. == $s) | not) | .id'
  ;;
*) die "usage: edge.sh stage|prune ..." ;;
esac
