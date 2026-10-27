# shellcheck shell=bash
SCRIPT="$SCRIPTS_DIR/build-pkg.sh"

ids() { printf '%s\n%s' "$APP_ID" "$INSTALLER_ID"; }

test_refuses_non_darwin() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    local rc=0
    FAKE_UNAME=Linux "$SCRIPT" xearthlayer "$FAKE_DIR/bin/xearthlayer" 0.5.0 "$FAKE_DIR/out/x.pkg" >/dev/null 2>&1 || rc=$?
    assert_eq "$rc" 1
}

test_usage_errors_exit_2() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    local rc=0
    "$SCRIPT" xearthlayer >/dev/null 2>&1 || rc=$?
    assert_eq "$rc" 2
    rc=0
    "$SCRIPT" nope "$FAKE_DIR/bin/xearthlayer" 0.5.0 "$FAKE_DIR/out/x.pkg" >/dev/null 2>&1 || rc=$?
    assert_eq "$rc" 2
    rc=0
    "$SCRIPT" xearthlayer "$FAKE_DIR/bin/missing" 0.5.0 "$FAKE_DIR/out/x.pkg" >/dev/null 2>&1 || rc=$?
    assert_eq "$rc" 2
    rc=0
    "$SCRIPT" xearthlayer "$FAKE_DIR/bin/xearthlayer" 0.5.0 "$FAKE_DIR/out/x.tar.gz" >/dev/null 2>&1 || rc=$?
    assert_eq "$rc" 2
}

test_builds_signs_notarizes_and_staples_the_runtime() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    local out="$FAKE_DIR/out/xearthlayer-v0.5.0-rc.1-arm64-macos.pkg" log
    FAKE_IDENTITIES="$(ids)" FAKE_SPCTL_STATUS="assessments enabled" MACOS_NOTARY_PROFILE=p \
        "$SCRIPT" xearthlayer "$FAKE_DIR/bin/xearthlayer" 0.5.0-rc.1 "$out" >/dev/null 2>&1 || fail "build failed"
    [ -f "$out" ] || fail "no package written"
    log=$(cat "$FAKE_LOG")
    assert_contains "$log" "--identifier net.defreyssi.xearthlayer --version 0.5.0-rc.1 --install-location /"
    assert_contains "$log" "staged: xearthlayer"
    assert_contains "$log" "--sign Developer ID Installer: Sam de Freyssinet (Y7DVMR5MEX) $out"
    assert_contains "$log" "xcrun notarytool submit $out --keychain-profile p"
    assert_contains "$log" "xcrun stapler staple $out"
    assert_contains "$log" "xcrun stapler validate $out"
    assert_contains "$log" "spctl --assess --type install -vv $out"
    assert_contains "$(cat "$FAKE_DIR/distribution.xml")" '<welcome file="welcome.html"'
    assert_contains "$(cat "$FAKE_DIR/resources.txt")" "welcome.html"
}

test_publisher_package_has_no_welcome_page() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer-publish"
    FAKE_IDENTITIES="$(ids)" MACOS_SKIP_NOTARIZE=1 \
        "$SCRIPT" xearthlayer-publish "$FAKE_DIR/bin/xearthlayer-publish" 0.5.0 "$FAKE_DIR/out/p.pkg" >/dev/null 2>&1 \
        || fail "build failed"
    assert_contains "$(cat "$FAKE_LOG")" "staged: xearthlayer-publish"
    assert_not_contains "$(cat "$FAKE_DIR/distribution.xml")" "<welcome"
    assert_eq "$(cat "$FAKE_DIR/resources.txt")" ""
}

test_explicit_identity_skips_lookup() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    MACOS_INSTALLER_IDENTITY="Developer ID Installer: X (Y)" MACOS_SKIP_NOTARIZE=1 \
        "$SCRIPT" xearthlayer "$FAKE_DIR/bin/xearthlayer" 0.5.0 "$FAKE_DIR/out/x.pkg" >/dev/null 2>&1 || fail "build failed"
    assert_not_contains "$(cat "$FAKE_LOG")" "security find-identity"
    assert_contains "$(cat "$FAKE_LOG")" "--sign Developer ID Installer: X (Y)"
}

test_refuses_two_installer_identities() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    local err rc=0
    err=$(FAKE_IDENTITIES="$(printf '%s\n%s' "$INSTALLER_ID" "$OTHER_INSTALLER_ID")" MACOS_SKIP_NOTARIZE=1 \
        "$SCRIPT" xearthlayer "$FAKE_DIR/bin/xearthlayer" 0.5.0 "$FAKE_DIR/out/x.pkg" 2>&1) || rc=$?
    assert_eq "$rc" 1
    assert_contains "$err" "set MACOS_INSTALLER_IDENTITY"
    assert_not_contains "$(cat "$FAKE_LOG")" "productbuild"
}

test_skip_notarize_signs_only() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    FAKE_IDENTITIES="$(ids)" MACOS_SKIP_NOTARIZE=1 \
        "$SCRIPT" xearthlayer "$FAKE_DIR/bin/xearthlayer" 0.5.0 "$FAKE_DIR/out/x.pkg" >/dev/null 2>&1 || fail "build failed"
    assert_contains "$(cat "$FAKE_LOG")" "productbuild"
    assert_not_contains "$(cat "$FAKE_LOG")" "notarytool"
    assert_not_contains "$(cat "$FAKE_LOG")" "stapler"
}

test_fails_when_notarization_is_rejected() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    local rc=0
    FAKE_IDENTITIES="$(ids)" FAKE_NOTARY_STATUS=Invalid MACOS_NOTARY_PROFILE=p \
        "$SCRIPT" xearthlayer "$FAKE_DIR/bin/xearthlayer" 0.5.0 "$FAKE_DIR/out/x.pkg" >/dev/null 2>&1 || rc=$?
    assert_eq "$rc" 1
    assert_not_contains "$(cat "$FAKE_LOG")" "stapler staple"
}

test_fails_when_gatekeeper_does_not_see_notarization() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    local err rc=0
    err=$(FAKE_IDENTITIES="$(ids)" FAKE_SPCTL_STATUS="assessments enabled" FAKE_SPCTL_SOURCE="Unnotarized Developer ID" MACOS_NOTARY_PROFILE=p \
        "$SCRIPT" xearthlayer "$FAKE_DIR/bin/xearthlayer" 0.5.0 "$FAKE_DIR/out/x.pkg" 2>&1) || rc=$?
    assert_eq "$rc" 1
    assert_contains "$err" "Gatekeeper does not report"
}

test_refuses_without_notary_credentials_before_building() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    local rc=0
    unset MACOS_NOTARY_PROFILE MACOS_NOTARY_KEY
    FAKE_IDENTITIES="$(ids)" "$SCRIPT" xearthlayer "$FAKE_DIR/bin/xearthlayer" 0.5.0 "$FAKE_DIR/out/x.pkg" >/dev/null 2>&1 || rc=$?
    assert_eq "$rc" 1
    assert_not_contains "$(cat "$FAKE_LOG")" "pkgbuild"
}

test_skips_assessment_when_gatekeeper_assessments_are_disabled() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    local out rc=0
    out=$(FAKE_IDENTITIES="$(ids)" MACOS_NOTARY_PROFILE=p \
        "$SCRIPT" xearthlayer "$FAKE_DIR/bin/xearthlayer" 0.5.0 "$FAKE_DIR/out/x.pkg" 2>&1) || rc=$?
    assert_eq "$rc" 0
    assert_contains "$out" "Gatekeeper assessments are disabled on this host"
    assert_contains "$(cat "$FAKE_LOG")" "stapler validate"
    assert_not_contains "$(cat "$FAKE_LOG")" "spctl --assess"
}

test_fails_when_gatekeeper_rejects_the_package() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    local err rc=0
    err=$(FAKE_IDENTITIES="$(ids)" FAKE_SPCTL_STATUS="assessments enabled" FAKE_SPCTL_REJECT=1 MACOS_NOTARY_PROFILE=p \
        "$SCRIPT" xearthlayer "$FAKE_DIR/bin/xearthlayer" 0.5.0 "$FAKE_DIR/out/x.pkg" 2>&1) || rc=$?
    assert_eq "$rc" 1
    assert_contains "$err" "Gatekeeper rejected"
}

test_retries_stapling_after_a_transient_failure() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    local rc=0 n
    FAKE_IDENTITIES="$(ids)" FAKE_STAPLE_FAILURES=1 MACOS_STAPLE_RETRY_DELAY=0 MACOS_NOTARY_PROFILE=p \
        "$SCRIPT" xearthlayer "$FAKE_DIR/bin/xearthlayer" 0.5.0 "$FAKE_DIR/out/x.pkg" >/dev/null 2>&1 || rc=$?
    assert_eq "$rc" 0
    n=$(grep -c "xcrun stapler staple" "$FAKE_LOG")
    assert_eq "$n" 2
}

test_fails_when_stapling_never_succeeds() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    local err rc=0 n
    err=$(FAKE_IDENTITIES="$(ids)" FAKE_STAPLE_FAILURES=3 MACOS_STAPLE_RETRY_DELAY=0 MACOS_NOTARY_PROFILE=p \
        "$SCRIPT" xearthlayer "$FAKE_DIR/bin/xearthlayer" 0.5.0 "$FAKE_DIR/out/x.pkg" 2>&1) || rc=$?
    assert_eq "$rc" 1
    assert_contains "$err" "attempt 1: stapler staple failed"
    n=$(grep -c "xcrun stapler staple" "$FAKE_LOG")
    assert_eq "$n" 3
}
