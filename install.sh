#!/bin/sh
# Install mis, mis-worker and mis-orchestrator from a GitHub release.
#   curl -fsSL https://raw.githubusercontent.com/tommy-xr/MakeItSo/main/install.sh | sh
# Env: MIS_VERSION (tag, or `edge` for the newest build of main; default latest release),
#      MIS_INSTALL_DIR (default ~/.local/bin),
#      MIS_TARBALL (local .tar.gz; skips download).
set -eu

repo="tommy-xr/MakeItSo"
dir="${MIS_INSTALL_DIR:-$HOME/.local/bin}"

die() { echo "mis install: $*" >&2; exit 1; }

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-gnu ;;
  *) die "unsupported OS: $(uname -s) (Windows: use install.ps1)" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac
target="$arch-$os"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

if [ -n "${MIS_TARBALL:-}" ]; then
  tarball="$MIS_TARBALL"
  [ -f "$tarball" ] || die "MIS_TARBALL not found: $tarball"
else
  command -v curl >/dev/null || die "curl is required"
  tag="${MIS_VERSION:-}"
  if [ -z "$tag" ]; then
    json="$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest")" ||
      die "could not find latest release of $repo"
    tag="$(printf '%s\n' "$json" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
    [ -n "$tag" ] || die "could not parse latest release tag"
  fi
  asset="mis-$target.tar.gz"
  # Edge assets are named by commit, e.g. mis-aarch64-apple-darwin-1a2b3c4.tar.gz.
  if [ "$tag" = edge ]; then
    json="$(curl -fsSL "https://github.com/$repo/releases/download/edge/edge.json")" ||
      die "could not fetch the edge build's edge.json"
    sha="$(printf '%s\n' "$json" | sed -n 's/.*"sha": *"\([0-9a-f]\{7\}\)[0-9a-f]*".*/\1/p' | head -n 1)"
    [ -n "$sha" ] || die "could not parse edge.json"
    asset="mis-$target-$sha.tar.gz"
  fi
  url="https://github.com/$repo/releases/download/$tag/$asset"
  echo "Downloading $url"
  tarball="$tmp/mis.tar.gz"
  curl -fsSL -o "$tarball" "$url" ||
    die "download failed: release $tag or asset $asset not found"
fi

mkdir "$tmp/x"
tar -xzf "$tarball" -C "$tmp/x" || die "could not extract $tarball"
mkdir -p "$dir"
for bin in mis mis-worker mis-orchestrator; do
  [ -f "$tmp/x/$bin" ] || die "$bin missing from archive"
  # rm first so a running binary is replaced, not overwritten in place.
  rm -f "$dir/$bin"
  cp "$tmp/x/$bin" "$dir/$bin"
  chmod +x "$dir/$bin"
done

echo "Installed mis, mis-worker, mis-orchestrator to $dir"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "Note: $dir is not on PATH. Add: export PATH=\"$dir:\$PATH\"" ;;
esac
