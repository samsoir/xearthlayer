#!/usr/bin/env bash
# Runs the release script tests: every test_* function in every test_*.sh
# beside this file, each in its own subshell so no state leaks between
# tests. Plain bash on purpose (no framework to install on either runner).
# A test fails when its subshell exits non-zero; the assertions in
# helpers.sh exit rather than return, because `set -e` is ignored inside an
# `if` and so cannot be relied on here.
set -uo pipefail

TESTS_DIR=$(cd "$(dirname "$0")" && pwd)
export TESTS_DIR
# A maintainer's shell may hold real signing settings (a notary profile, an
# identity); the tests must never see them.
unset MACOS_SIGN_IDENTITY MACOS_INSTALLER_IDENTITY MACOS_NOTARY_PROFILE \
    MACOS_NOTARY_KEY MACOS_NOTARY_KEY_ID MACOS_NOTARY_ISSUER_ID MACOS_SKIP_NOTARIZE
passed=0
failed=0
out=$(mktemp)
trap 'rm -f "$out"' EXIT

for file in "$TESTS_DIR"/test_*.sh; do
    # shellcheck source=/dev/null
    names=$( (. "$TESTS_DIR/helpers.sh"; . "$file"; declare -F | awk '{print $3}' | grep '^test_') )
    for name in $names; do
        # shellcheck source=/dev/null
        if ( . "$TESTS_DIR/helpers.sh"; . "$file"; "$name" ) >"$out" 2>&1; then
            passed=$((passed + 1))
        else
            failed=$((failed + 1))
            echo "FAILED $(basename "$file") $name"
            sed 's/^/    /' "$out"
        fi
    done
done

echo "release script tests: $passed passed, $failed failed"
[ "$failed" -eq 0 ]
