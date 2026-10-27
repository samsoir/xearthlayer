#!/usr/bin/env bash
# Build, sign, notarize and staple the macOS installer package for one
# XEarthLayer product.
#
# Usage: scripts/macos/build-pkg.sh <product> <binary> <version> <out.pkg>
#
#   product  xearthlayer (the runtime) or xearthlayer-publish (the publisher)
#   binary   the executable, already signed and notarized by sign-and-notarize.sh
#   version  without the leading v, e.g. 0.5.0 or 0.5.0-rc.1
#   out.pkg  where to write the package
#
# The package installs the binary as /usr/local/bin/<product> under the
# receipt net.defreyssi.<product>, with no install scripts. Unlike a bare
# executable it can carry its notarization ticket, so it is stapled and
# installs with no network; and the installer does not quarantine the files
# it writes, so the installed binary never meets a first-run assessment.
#
# The same script runs in the release workflow and under `make pkg-macos`.
# See docs/dev/macos-signing-runbook.md.
#
# Environment:
#   MACOS_INSTALLER_IDENTITY  "Developer ID Installer: ..." identity to sign with.
#                             Default: the one such identity in the keychain
#                             search list. Zero or several is an error.
#   MACOS_NOTARY_*            Notary credentials, as for sign-and-notarize.sh.
#   MACOS_SKIP_NOTARIZE       Set to 1 to sign only (local smoke tests).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=scripts/macos/lib.sh
. "$here/lib.sh"
repo_root=$(cd "$here/../.." && pwd)

if [ "$(uname)" != "Darwin" ]; then
    echo "error: packaging runs on macOS only" >&2
    exit 1
fi
if [ "$#" -ne 4 ]; then
    sed -n '2,10p' "$0" >&2
    exit 2
fi
product=$1 binary=$2 version=$3 out=$4
valid_product "$product" || exit 2
if [ ! -f "$binary" ] || [ ! -x "$binary" ]; then
    echo "error: $binary is not an executable file" >&2
    exit 2
fi
case "$out" in
    *.pkg) ;;
    *) echo "error: $out must end in .pkg" >&2; exit 2 ;;
esac

# Credentials are checked before anything is built, so a missing secret
# fails in seconds rather than after the package exists.
if [ "${MACOS_SKIP_NOTARIZE:-0}" != "1" ]; then
    set_notary_args
fi

# --- identity ---------------------------------------------------------------

identity="${MACOS_INSTALLER_IDENTITY:-}"
if [ -z "$identity" ]; then
    # `-p basic`, not `-p codesigning`: an Installer certificate has no code
    # signing purpose, so the codesigning policy never lists it.
    selected=$(security find-identity -v -p basic | select_identity "Developer ID Installer" MACOS_INSTALLER_IDENTITY)
    # productbuild takes the identity's name; it does not accept a SHA-1.
    identity=$(printf '%s' "$selected" | cut -f2)
fi
echo "Installer identity: $identity"

# --- build ------------------------------------------------------------------

work=$(mktemp -d "${TMPDIR:-/tmp}/xel-pkg.XXXXXX")
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/root/usr/local/bin" "$work/resources" "$work/components"
cp "$binary" "$work/root/usr/local/bin/$product"
chmod 755 "$work/root/usr/local/bin/$product"
if [ "$product" = "xearthlayer" ]; then
    cp "$repo_root/pkg/macos/welcome.html" "$work/resources/"
fi
render_distribution "$product" "$version" >"$work/distribution.xml"

echo "==> pkgbuild $product $version"
pkgbuild --root "$work/root" --identifier "net.defreyssi.$product" --version "$version" \
    --install-location / "$work/components/$product.pkg"

echo "==> productbuild $(basename "$out")"
mkdir -p "$(dirname "$out")"
productbuild --distribution "$work/distribution.xml" --resources "$work/resources" \
    --package-path "$work/components" --sign "$identity" "$out"
pkgutil --check-signature "$out"

if [ "${MACOS_SKIP_NOTARIZE:-0}" = "1" ]; then
    echo "MACOS_SKIP_NOTARIZE=1: signed, not notarized: $out"
    exit 0
fi

# --- notarize, staple, assess -----------------------------------------------

notarize "$out" "$work"
xcrun stapler staple "$out"
xcrun stapler validate "$out"

# spctl understands installer packages (unlike bare executables), so this is
# the same assessment the Installer makes when a user opens the download.
assessment=$(spctl --assess --type install -vv "$out" 2>&1) || {
    printf '%s\n' "$assessment" >&2
    echo "error: Gatekeeper rejected $out" >&2
    exit 1
}
printf '%s\n' "$assessment"
case "$assessment" in
    *"source=Notarized Developer ID"*) ;;
    *)
        echo "error: Gatekeeper does not report $out as notarized" >&2
        exit 1
        ;;
esac

echo "Built, notarized and stapled: $out"
