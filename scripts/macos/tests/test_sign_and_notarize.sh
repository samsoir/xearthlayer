# shellcheck shell=bash
SCRIPT="$SCRIPTS_DIR/sign-and-notarize.sh"

test_refuses_non_darwin() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    local rc=0
    FAKE_UNAME=Linux "$SCRIPT" "$FAKE_DIR/bin/xearthlayer" >/dev/null 2>&1 || rc=$?
    assert_eq "$rc" 1
}

test_signs_with_the_one_application_identity() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    FAKE_IDENTITIES="$APP_ID" MACOS_SKIP_NOTARIZE=1 "$SCRIPT" "$FAKE_DIR/bin/xearthlayer" >/dev/null 2>&1 \
        || fail "signing failed"
    assert_contains "$(cat "$FAKE_LOG")" "codesign --force --timestamp --options runtime --sign 1111111111111111111111111111111111111111 $FAKE_DIR/bin/xearthlayer"
}

test_refuses_two_application_identities() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    local err rc=0
    err=$(FAKE_IDENTITIES="$(printf '%s\n%s' "$APP_ID" "${APP_ID/1111111111111111111111111111111111111111/4444444444444444444444444444444444444444}")" \
        MACOS_SKIP_NOTARIZE=1 "$SCRIPT" "$FAKE_DIR/bin/xearthlayer" 2>&1) || rc=$?
    assert_eq "$rc" 1
    assert_contains "$err" "set MACOS_SIGN_IDENTITY"
}

test_notarizes_all_binaries_in_one_submission() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    make_exe "$FAKE_DIR/bin/xearthlayer-publish"
    FAKE_IDENTITIES="$APP_ID" MACOS_NOTARY_PROFILE=p "$SCRIPT" "$FAKE_DIR/bin/xearthlayer" "$FAKE_DIR/bin/xearthlayer-publish" >/dev/null 2>&1 \
        || fail "sign and notarize failed"
    assert_eq "$(grep -c 'notarytool submit' "$FAKE_LOG")" 1
    assert_contains "$(cat "$FAKE_LOG")" "payload.zip --keychain-profile p --wait --timeout 30m"
}

test_fails_when_notarization_is_rejected() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    local rc=0
    FAKE_IDENTITIES="$APP_ID" FAKE_NOTARY_STATUS=Invalid MACOS_NOTARY_PROFILE=p \
        "$SCRIPT" "$FAKE_DIR/bin/xearthlayer" >/dev/null 2>&1 || rc=$?
    assert_eq "$rc" 1
    assert_contains "$(cat "$FAKE_LOG")" "notarytool log 4f1e-test"
}

test_refuses_without_notary_credentials() {
    setup_fakes
    make_exe "$FAKE_DIR/bin/xearthlayer"
    local rc=0
    FAKE_IDENTITIES="$APP_ID" "$SCRIPT" "$FAKE_DIR/bin/xearthlayer" >/dev/null 2>&1 || rc=$?
    assert_eq "$rc" 1
}
