#!/usr/bin/env bash
# Sign and notarize XEarthLayer's macOS release binaries.
#
# Usage: scripts/macos/sign-and-notarize.sh <binary>...
#
# Every binary is signed with a Developer ID Application identity, hardened
# runtime on and a secure timestamp attached, then all of them go to Apple's
# notary service in one submission. A bare executable cannot carry a stapled
# ticket (stapling needs a bundle, disk image or installer package), so
# Gatekeeper fetches the ticket online the first time a quarantined copy runs.
# The final step reproduces exactly that: a quarantined copy of each binary is
# run with --version, and must survive. (spctl --assess and syspolicy_check
# only understand bundles, images and packages; both reject a notarized bare
# executable.)
#
# The same script runs in the release workflow and under `make sign-macos`.
# Identity selection and notarization live in lib.sh, shared with build-pkg.sh.
# See docs/dev/macos-signing-runbook.md.
#
# Environment:
#   MACOS_SIGN_IDENTITY     Identity to sign with: a "Developer ID Application: ..."
#                           name or its SHA-1 hash. Default: the one Developer ID
#                           Application identity in the keychain search list. Zero
#                           or several is an error, never a guess.
#   MACOS_NOTARY_KEY        Path to an App Store Connect API key (.p8), with
#   MACOS_NOTARY_KEY_ID     its key ID and
#   MACOS_NOTARY_ISSUER_ID  its issuer ID.
#   MACOS_NOTARY_PROFILE    Alternative to the three above: a profile saved with
#                           `xcrun notarytool store-credentials`.
#   MACOS_SKIP_NOTARIZE     Set to 1 to sign only (local smoke tests).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=scripts/macos/lib.sh
. "$here/lib.sh"

if [ "$(uname)" != "Darwin" ]; then
    echo "error: signing runs on macOS only" >&2
    exit 1
fi
if [ "$#" -eq 0 ]; then
    echo "usage: $0 <binary>..." >&2
    exit 2
fi
for bin in "$@"; do
    if [ ! -f "$bin" ] || [ ! -x "$bin" ]; then
        echo "error: $bin is not an executable file" >&2
        exit 2
    fi
done

# --- identity ---------------------------------------------------------------

identity="${MACOS_SIGN_IDENTITY:-}"
if [ -z "$identity" ]; then
    # `find-identity -v` lists only identities whose certificate is valid now
    # and whose private key is present.
    selected=$(security find-identity -v -p codesigning | select_identity "Developer ID Application" MACOS_SIGN_IDENTITY)
    identity=$(printf '%s' "$selected" | cut -f1)
    echo "Signing identity: $(printf '%s' "$selected" | cut -f2)"
else
    echo "Signing identity: $identity"
fi

# --- sign -------------------------------------------------------------------

for bin in "$@"; do
    echo "==> codesign $bin"
    # --force replaces the ad-hoc, linker-generated signature every Rust build
    # carries on Apple Silicon. --options runtime (hardened runtime) and
    # --timestamp are both notarization requirements. No entitlements: the
    # binaries link only Apple frameworks and reach macFUSE by launching its
    # mount helper as a child process, never by loading a library.
    codesign --force --timestamp --options runtime --sign "$identity" "$bin"
    codesign --verify --strict --verbose=2 "$bin"
    codesign --display --verbose=2 "$bin" 2>&1 | grep -E '^(Authority|TeamIdentifier|Timestamp|CodeDirectory)' || true
done

if [ "${MACOS_SKIP_NOTARIZE:-0}" = "1" ]; then
    echo "MACOS_SKIP_NOTARIZE=1: signed, not notarized."
    exit 0
fi

# --- notarize ---------------------------------------------------------------

set_notary_args

work=$(mktemp -d "${TMPDIR:-/tmp}/xel-notarize.XXXXXX")
trap 'rm -rf "$work"' EXIT
mkdir "$work/payload"
for bin in "$@"; do
    cp "$bin" "$work/payload/"
done
# ditto is the archiver Apple documents for notarization: it keeps the
# executable bits and the signatures intact, where a plain zip may not.
ditto -c -k --keepParent "$work/payload" "$work/payload.zip"

notarize "$work/payload.zip" "$work"

# --- confirm Gatekeeper sees the ticket ------------------------------------
#
# A browser download carries com.apple.quarantine, and the first exec of a
# quarantined binary is a Gatekeeper assessment: with a notarization ticket
# it runs, without one it is killed (SIGKILL, exit 137). Running a quarantined
# copy is therefore the real test, and the only one that works on a bare
# executable. It is meaningful only where assessments are on, so a host with
# them disabled (a hardened CI image, a developer who turned them off) gets a
# note instead of a vacuous pass.

if spctl --status 2>&1 | grep -q 'assessments enabled'; then
    for bin in "$@"; do
        probe="$work/probe-$(basename "$bin")"
        cp "$bin" "$probe"
        xattr -w com.apple.quarantine "0083;$(printf '%x' "$(date +%s)");Safari;$(uuidgen)" "$probe"
        echo "==> quarantined run: $(basename "$bin") --version"
        passed=0
        for attempt in 1 2 3; do
            set +e
            output=$("$probe" --version 2>&1)
            rc=$?
            set -e
            if [ "$rc" -eq 0 ] && [ -n "$output" ]; then
                passed=1
                break
            fi
            echo "attempt $attempt: exit $rc ${output:+($output)}"
            sleep 10
        done
        if [ "$passed" -ne 1 ]; then
            echo "error: Gatekeeper refused a quarantined copy of $bin; the notarization ticket is not being served" >&2
            exit 1
        fi
        echo "$output"
    done
else
    echo "note: Gatekeeper assessments are disabled on this host; skipping the quarantined-run check"
fi

echo "Signed and notarized: $*"
