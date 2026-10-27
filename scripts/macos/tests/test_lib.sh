# shellcheck shell=bash
# shellcheck source=/dev/null
. "$SCRIPTS_DIR/lib.sh"

test_valid_product_accepts_both_products() {
    valid_product xearthlayer || fail "xearthlayer rejected"
    valid_product xearthlayer-publish || fail "xearthlayer-publish rejected"
}

test_valid_product_rejects_other_names() {
    local rc=0
    valid_product xearthlayer-cli 2>/dev/null || rc=$?
    assert_eq "$rc" 2
}

test_select_identity_picks_the_one_application_identity() {
    local got
    got=$(printf '%s\n%s\n' "$APP_ID" "$INSTALLER_ID" | select_identity "Developer ID Application" MACOS_SIGN_IDENTITY)
    assert_eq "$got" "$(printf '1111111111111111111111111111111111111111\tDeveloper ID Application: Sam de Freyssinet (Y7DVMR5MEX)')"
}

test_select_identity_picks_the_one_installer_identity() {
    local got
    got=$(printf '%s\n%s\n' "$APP_ID" "$INSTALLER_ID" | select_identity "Developer ID Installer" MACOS_INSTALLER_IDENTITY)
    assert_eq "$(printf '%s' "$got" | cut -f2)" "Developer ID Installer: Sam de Freyssinet (Y7DVMR5MEX)"
}

test_select_identity_refuses_none() {
    local err rc=0
    err=$(printf '%s\n' "$APP_ID" | select_identity "Developer ID Installer" MACOS_INSTALLER_IDENTITY 2>&1 >/dev/null) || rc=$?
    assert_eq "$rc" 1
    assert_contains "$err" "no valid 'Developer ID Installer' identity"
}

test_select_identity_refuses_several() {
    local err rc=0
    err=$(printf '%s\n%s\n' "$INSTALLER_ID" "$OTHER_INSTALLER_ID" | select_identity "Developer ID Installer" MACOS_INSTALLER_IDENTITY 2>&1 >/dev/null) || rc=$?
    assert_eq "$rc" 1
    assert_contains "$err" "2 'Developer ID Installer' identities found; set MACOS_INSTALLER_IDENTITY"
}

test_notary_args_from_profile() {
    MACOS_NOTARY_PROFILE=xearthlayer set_notary_args
    assert_eq "${NOTARY_ARGS[*]}" "--keychain-profile xearthlayer"
}

test_notary_args_from_api_key() {
    unset MACOS_NOTARY_PROFILE
    MACOS_NOTARY_KEY=/k.p8 MACOS_NOTARY_KEY_ID=KID MACOS_NOTARY_ISSUER_ID=ISS set_notary_args
    assert_eq "${NOTARY_ARGS[*]}" "--key /k.p8 --key-id KID --issuer ISS"
}

test_notary_args_refuse_key_without_ids() {
    local rc=0
    unset MACOS_NOTARY_PROFILE MACOS_NOTARY_KEY_ID MACOS_NOTARY_ISSUER_ID
    MACOS_NOTARY_KEY=/k.p8 set_notary_args 2>/dev/null || rc=$?
    assert_eq "$rc" 1
}

test_notary_args_refuse_no_credentials() {
    local err rc=0
    unset MACOS_NOTARY_PROFILE MACOS_NOTARY_KEY
    err=$(set_notary_args 2>&1) || rc=$?
    assert_eq "$rc" 1
    assert_contains "$err" "no notary credentials"
}

test_notarize_accepted() {
    setup_fakes
    NOTARY_ARGS=(--keychain-profile p)
    local work
    work=$(mktemp -d)
    : >"$work/x.pkg"
    notarize "$work/x.pkg" "$work" >/dev/null || fail "accepted submission reported as failure"
    assert_contains "$(cat "$FAKE_LOG")" "xcrun notarytool submit $work/x.pkg --keychain-profile p --wait --timeout 30m"
}

test_notarize_rejected_prints_log() {
    setup_fakes
    NOTARY_ARGS=(--keychain-profile p)
    local work rc=0
    work=$(mktemp -d)
    : >"$work/x.pkg"
    FAKE_NOTARY_STATUS=Invalid notarize "$work/x.pkg" "$work" >/dev/null 2>&1 || rc=$?
    assert_eq "$rc" 1
    assert_contains "$(cat "$FAKE_LOG")" "xcrun notarytool log 4f1e-test"
}

test_distribution_for_runtime() {
    local xml
    xml=$(render_distribution xearthlayer 0.5.0-rc.1)
    assert_contains "$xml" '<title>XEarthLayer</title>'
    assert_contains "$xml" '<welcome file="welcome.html" mime-type="text/html"/>'
    assert_contains "$xml" 'hostArchitectures="arm64"'
    assert_contains "$xml" '<os-version min="12.0"/>'
    assert_contains "$xml" '<pkg-ref id="net.defreyssi.xearthlayer" version="0.5.0-rc.1" onConclusion="none">xearthlayer.pkg</pkg-ref>'
    if command -v xmllint >/dev/null 2>&1; then
        printf '%s' "$xml" | xmllint --noout - || fail "distribution.xml is not well formed"
    fi
}

test_distribution_for_publisher_has_no_welcome() {
    local xml
    xml=$(render_distribution xearthlayer-publish 0.5.0)
    assert_contains "$xml" '<title>XEarthLayer Publishing Tools</title>'
    assert_contains "$xml" 'net.defreyssi.xearthlayer-publish'
    assert_not_contains "$xml" '<welcome'
}

test_distribution_rejects_unknown_product() {
    local rc=0
    render_distribution nope 1.0 >/dev/null 2>&1 || rc=$?
    assert_eq "$rc" 2
}
