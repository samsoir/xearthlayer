#!/usr/bin/env bash
# Render all four casks and check them the way the tap's Audit workflow does,
# before anything is published: `brew style`, then `brew audit --strict`.
#
# Usage: scripts/homebrew/check-casks.sh [version]
#
#   version   the release being checked, without the leading v (default: the
#             workspace version in Cargo.toml). The stable casks are rendered
#             at its base version, the pre-release casks at the version itself,
#             or at <base>-rc.1 when it has no pre-release suffix.
#
# macOS with Homebrew only. The tap's own audit runs after `publish` has pushed
# the casks, in another repository, so nothing in a release run saw it fail
# (0.5.1-beta.3 and beta.4). The online checks (download, GitHub release) and
# the install smoke test still run only in the tap: they need the release.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
version=${1:-$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -1)}
base=${version%%-*}
case "$version" in
    *-*) pre=$version ;;
    *) pre="$base-rc.1" ;;
esac
# Never a real tarball's hash: no check here downloads anything.
sha256=0000000000000000000000000000000000000000000000000000000000000000

# Homebrew audits only casks inside a tap directory. A tap of its own name,
# so a maintainer's installed xearthlayer/tap is never touched.
work=$(mktemp -d)
tap_link="$(brew --repository)/Library/Taps/xel-check/homebrew-casks"
cleanup() {
    rm -f "$tap_link"
    rmdir "$(dirname "$tap_link")" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT
mkdir -p "$(dirname "$tap_link")"
ln -sfn "$work" "$tap_link"

for product in xearthlayer xearthlayer-publish; do
    "$root/scripts/homebrew/render-cask.sh" "$product" stable "$base" "$sha256" samsoir/xearthlayer "$work/Casks" >/dev/null
    "$root/scripts/homebrew/render-cask.sh" "$product" prerelease "$pre" "$sha256" samsoir/xearthlayer "$work/Casks" >/dev/null
done

brew trust xel-check/casks
brew style "$work"/Casks/*.rb
for cask in "$work"/Casks/*.rb; do
    token=$(basename "$cask" .rb)
    except=()
    case "$token" in *@prerelease) except=(--except github_prerelease_version) ;; esac
    brew audit --cask --strict ${except[@]+"${except[@]}"} "xel-check/casks/$token"
done
echo "Homebrew casks pass brew style and brew audit --strict ($base, $pre)."
