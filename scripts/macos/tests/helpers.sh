# Assertions and fakes for the release script tests. Sourced by run.sh.
# shellcheck shell=bash

SCRIPTS_DIR=$(cd "$TESTS_DIR/.." && pwd)
# shellcheck disable=SC2034
REPO_ROOT=$(cd "$SCRIPTS_DIR/../.." && pwd)

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

assert_eq() {
    [ "$1" = "$2" ] || fail "expected '$2', got '$1'"
}

assert_contains() {
    case "$1" in
        *"$2"*) ;;
        *) fail "expected to find '$2' in: $1" ;;
    esac
}

assert_not_contains() {
    case "$1" in
        *"$2"*) fail "did not expect '$2' in: $1" ;;
        *) ;;
    esac
}

# A temp directory of fake macOS tools, first on PATH. Every fake is a
# symlink to fakes/fake-tool, which appends "<tool> <args>" to $FAKE_LOG.
setup_fakes() {
    FAKE_DIR=$(mktemp -d)
    FAKE_LOG="$FAKE_DIR/calls.log"
    : >"$FAKE_LOG"
    local tool
    for tool in uname security codesign xcrun ditto xattr uuidgen spctl pkgbuild productbuild pkgutil; do
        ln -s "$TESTS_DIR/fakes/fake-tool" "$FAKE_DIR/$tool"
    done
    PATH="$FAKE_DIR:$PATH"
    export FAKE_DIR FAKE_LOG PATH
}

make_exe() {
    mkdir -p "$(dirname "$1")"
    printf '#!/bin/sh\necho "%s 0.0.0"\n' "$(basename "$1")" >"$1"
    chmod +x "$1"
}

# Fixtures used by the test files that source this one.
# shellcheck disable=SC2034
APP_ID='  1) 1111111111111111111111111111111111111111 "Developer ID Application: Sam de Freyssinet (Y7DVMR5MEX)"'
# shellcheck disable=SC2034
INSTALLER_ID='  2) 2222222222222222222222222222222222222222 "Developer ID Installer: Sam de Freyssinet (Y7DVMR5MEX)"'
# shellcheck disable=SC2034
OTHER_INSTALLER_ID='  3) 3333333333333333333333333333333333333333 "Developer ID Installer: Someone Else (ABCDE12345)"'
