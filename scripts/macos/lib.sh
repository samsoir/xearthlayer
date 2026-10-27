# Helpers shared by the macOS release scripts (sign-and-notarize.sh and
# build-pkg.sh). Sourced, never run. Nothing here checks for Darwin, so the
# tests in scripts/macos/tests run it on Linux against fake tools.
# shellcheck shell=bash

valid_product() {
    case "$1" in
        xearthlayer | xearthlayer-publish) return 0 ;;
        *)
            echo "error: product must be xearthlayer or xearthlayer-publish, not '$1'" >&2
            return 2
            ;;
    esac
}

# Reads `security find-identity -v` output on stdin and prints
# "<sha1><TAB><name>" for the one valid identity of the given kind
# ("Developer ID Application" or "Developer ID Installer"). Zero or several
# is an error, never a guess; the message names the variable that overrides.
select_identity() {
    local kind=$1 override=$2 matches count
    matches=$(grep -F "\"$kind: " || true)
    count=$(printf '%s' "$matches" | grep -c . || true)
    if [ "$count" -eq 0 ]; then
        echo "error: no valid '$kind' identity in the keychain search list" >&2
        echo "       (security find-identity -v). See docs/dev/macos-signing-runbook.md." >&2
        return 1
    fi
    if [ "$count" -gt 1 ]; then
        echo "error: $count '$kind' identities found; set $override:" >&2
        printf '%s\n' "$matches" >&2
        return 1
    fi
    printf '%s\t%s\n' \
        "$(printf '%s' "$matches" | awk '{print $2}')" \
        "$(printf '%s' "$matches" | sed -E 's/^[^"]*"(.*)"$/\1/')"
}

# Sets NOTARY_ARGS for notarytool: a saved keychain profile, or an App Store
# Connect API key with its key ID and issuer ID.
set_notary_args() {
    NOTARY_ARGS=()
    if [ -n "${MACOS_NOTARY_PROFILE:-}" ]; then
        NOTARY_ARGS=(--keychain-profile "$MACOS_NOTARY_PROFILE")
    elif [ -n "${MACOS_NOTARY_KEY:-}" ]; then
        if [ -z "${MACOS_NOTARY_KEY_ID:-}" ] || [ -z "${MACOS_NOTARY_ISSUER_ID:-}" ]; then
            echo "error: MACOS_NOTARY_KEY needs MACOS_NOTARY_KEY_ID and MACOS_NOTARY_ISSUER_ID" >&2
            return 1
        fi
        NOTARY_ARGS=(--key "$MACOS_NOTARY_KEY" --key-id "$MACOS_NOTARY_KEY_ID" --issuer "$MACOS_NOTARY_ISSUER_ID")
    else
        echo "error: no notary credentials: set MACOS_NOTARY_PROFILE, or MACOS_NOTARY_KEY/_KEY_ID/_ISSUER_ID," >&2
        echo "       or MACOS_SKIP_NOTARIZE=1 to sign only" >&2
        return 1
    fi
}

# Submits one file (a zip of binaries, or a package) and waits for Apple's
# verdict. On anything but Accepted, prints the submission's log and fails.
notarize() {
    local file=$1 work=$2 log status id
    log="$work/submit-$(basename "$file").log"
    echo "==> notarytool submit $(basename "$file")"
    set +e
    xcrun notarytool submit "$file" "${NOTARY_ARGS[@]}" --wait --timeout 30m 2>&1 | tee "$log"
    status=${PIPESTATUS[0]}
    set -e
    id=$(grep -m1 -E '^ *id: ' "$log" | awk '{print $2}' || true)
    if [ "$status" -ne 0 ] || ! grep -qE '^ *status: Accepted' "$log"; then
        echo "error: notarization of $(basename "$file") did not succeed (exit $status)" >&2
        if [ -n "$id" ]; then
            echo "==> notarytool log $id" >&2
            xcrun notarytool log "$id" "${NOTARY_ARGS[@]}" >&2 || true
        fi
        return 1
    fi
    echo "Notarized: submission $id accepted"
}

# Prints the productbuild distribution for one product's package: arm64
# only, macOS 12 or later (the casks' `depends_on macos: :monterey`), one
# hidden choice, no customisation, no scripts. The runtime gets a welcome
# page (pkg/macos/welcome.html) about macFUSE.
render_distribution() {
    local product=$1 version=$2 title welcome="" id="net.defreyssi.$1"
    case "$product" in
        xearthlayer)
            title="XEarthLayer"
            welcome='    <welcome file="welcome.html" mime-type="text/html"/>
'
            ;;
        xearthlayer-publish) title="XEarthLayer Publishing Tools" ;;
        *)
            valid_product "$product"
            return 2
            ;;
    esac
    cat <<EOT
<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
    <title>$title</title>
${welcome}    <options customize="never" require-scripts="false" hostArchitectures="arm64"/>
    <domains enable_localSystem="true" enable_anywhere="false" enable_currentUserHome="false"/>
    <volume-check>
        <allowed-os-versions>
            <os-version min="12.0"/>
        </allowed-os-versions>
    </volume-check>
    <choices-outline>
        <line choice="$id"/>
    </choices-outline>
    <choice id="$id" visible="false" title="$title">
        <pkg-ref id="$id"/>
    </choice>
    <pkg-ref id="$id" version="$version" onConclusion="none">$product.pkg</pkg-ref>
</installer-gui-script>
EOT
}
