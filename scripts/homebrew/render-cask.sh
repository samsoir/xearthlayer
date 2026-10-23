#!/usr/bin/env bash
# Render a Homebrew cask for one release from the templates in pkg/homebrew/.
#
# Usage: scripts/homebrew/render-cask.sh <stable|prerelease> <version> <sha256> <owner/repo> <out-dir>
#
#   version   without the leading v, e.g. 0.5.0 or 0.5.0-alpha.2
#   sha256    of xearthlayer-v<version>-arm64-macos.tar.gz
#   owner/repo  the GitHub repository the tarball is downloaded from
#   out-dir   the tap's Casks/ directory; the file is named after the cask token
#
# Prints the path written. Used by release.yml's "Update Homebrew tap" step
# and by hand when a bump must be redone; see docs/dev/cicd.md.
set -euo pipefail

if [ "$#" -ne 5 ]; then
    sed -n '2,12p' "$0" >&2
    exit 2
fi
channel=$1 version=$2 sha256=$3 repo=$4 out_dir=$5

case "$channel" in
    stable) token="xearthlayer" ;;
    prerelease) token="xearthlayer@prerelease" ;;
    *) echo "error: channel must be stable or prerelease, not '$channel'" >&2; exit 2 ;;
esac
if ! printf '%s' "$version" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+(-(dev|alpha|beta|rc)\.[0-9]+)?$'; then
    echo "error: '$version' is not a release version (no leading v, e.g. 0.5.0 or 0.5.0-alpha.2)" >&2
    exit 2
fi
case "$channel:$version" in
    stable:*-*) echo "error: a pre-release version cannot go to the stable cask" >&2; exit 2 ;;
    prerelease:*-*) ;;
    prerelease:*) echo "error: a stable version cannot go to the pre-release cask" >&2; exit 2 ;;
esac
if ! printf '%s' "$sha256" | grep -qE '^[0-9a-f]{64}$'; then
    echo "error: '$sha256' is not a lowercase hex SHA-256" >&2
    exit 2
fi

template="$(dirname "$0")/../../pkg/homebrew/${token}.rb.tmpl"
mkdir -p "$out_dir"
out="$out_dir/${token}.rb"
sed -e "s|@VERSION@|$version|g" -e "s|@SHA256@|$sha256|g" -e "s|@REPO@|$repo|g" "$template" > "$out"
if grep -q '@[A-Z0-9]*@' "$out"; then
    echo "error: unreplaced placeholder in $out" >&2
    exit 1
fi
echo "$out"
