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
# See docs/dev/macos-signing.md.
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
    # and whose private key is present, one per line:
    #   1) <sha1> "Developer ID Application: Name (TEAMID)"
    matches=$(security find-identity -v -p codesigning | grep 'Developer ID Application' || true)
    count=$(printf '%s' "$matches" | grep -c . || true)
    if [ "$count" -eq 0 ]; then
        echo "error: no valid 'Developer ID Application' identity in the keychain search list" >&2
        echo "       (security find-identity -v -p codesigning). See docs/dev/macos-signing.md." >&2
        exit 1
    fi
    if [ "$count" -gt 1 ]; then
        echo "error: $count 'Developer ID Application' identities found; set MACOS_SIGN_IDENTITY:" >&2
        printf '%s\n' "$matches" >&2
        exit 1
    fi
    identity=$(printf '%s' "$matches" | awk '{print $2}')
    echo "Signing identity: $(printf '%s' "$matches" | sed -E 's/^ *[0-9]+\) //')"
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

notary_args=()
if [ -n "${MACOS_NOTARY_PROFILE:-}" ]; then
    notary_args=(--keychain-profile "$MACOS_NOTARY_PROFILE")
elif [ -n "${MACOS_NOTARY_KEY:-}" ]; then
    : "${MACOS_NOTARY_KEY_ID:?set with MACOS_NOTARY_KEY}"
    : "${MACOS_NOTARY_ISSUER_ID:?set with MACOS_NOTARY_KEY}"
    notary_args=(--key "$MACOS_NOTARY_KEY" --key-id "$MACOS_NOTARY_KEY_ID" --issuer "$MACOS_NOTARY_ISSUER_ID")
else
    echo "error: no notary credentials: set MACOS_NOTARY_PROFILE, or MACOS_NOTARY_KEY/_KEY_ID/_ISSUER_ID," >&2
    echo "       or MACOS_SKIP_NOTARIZE=1 to sign only" >&2
    exit 1
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/xel-notarize.XXXXXX")
trap 'rm -rf "$work"' EXIT
mkdir "$work/payload"
for bin in "$@"; do
    cp "$bin" "$work/payload/"
done
# ditto is the archiver Apple documents for notarization: it keeps the
# executable bits and the signatures intact, where a plain zip may not.
ditto -c -k --keepParent "$work/payload" "$work/payload.zip"

echo "==> notarytool submit ($# binaries)"
submit_log="$work/submit.log"
set +e
xcrun notarytool submit "$work/payload.zip" "${notary_args[@]}" --wait --timeout 30m 2>&1 | tee "$submit_log"
submit_status=${PIPESTATUS[0]}
set -e
submission_id=$(grep -m1 -E '^ *id: ' "$submit_log" | awk '{print $2}')
if [ "$submit_status" -ne 0 ] || ! grep -qE '^ *status: Accepted' "$submit_log"; then
    echo "error: notarization did not succeed (exit $submit_status)" >&2
    if [ -n "$submission_id" ]; then
        echo "==> notarytool log $submission_id" >&2
        xcrun notarytool log "$submission_id" "${notary_args[@]}" >&2 || true
    fi
    exit 1
fi
echo "Notarized: submission $submission_id accepted"

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
