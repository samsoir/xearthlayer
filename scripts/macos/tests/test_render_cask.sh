# shellcheck shell=bash
# scripts/homebrew/render-cask.sh, the casks release.yml pushes to the tap.

RENDER="$REPO_ROOT/scripts/homebrew/render-cask.sh"
SHA=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef

# Homebrew's audit runs RuboCop's Layout/LineLength on every cask, comments
# included, and fails the tap's CI past 118 characters. Like RuboCop
# (AllowURI), a line long only because a URI runs to its end is allowed.
assert_lines_fit() {
    local file=$1 long
    long=$(awk 'length($0) > 118 && $0 !~ /https?:\/\/[^ ]*"?$/ { printf "%d: %d chars\n", NR, length($0) }' "$file")
    [ -z "$long" ] || fail "$(basename "$file") has lines over 118 characters: $long"
}

render() {
    "$RENDER" "$1" "$2" "$3" "$SHA" samsoir/xearthlayer "$OUT"
}

test_stable_casks_fit_homebrews_line_length() {
    OUT=$(mktemp -d)
    trap 'rm -rf "$OUT"' EXIT
    local product
    for product in xearthlayer xearthlayer-publish; do
        assert_lines_fit "$(render "$product" stable 10.10.10)"
    done
}

test_prerelease_casks_fit_homebrews_line_length() {
    OUT=$(mktemp -d)
    trap 'rm -rf "$OUT"' EXIT
    local product
    for product in xearthlayer xearthlayer-publish; do
        assert_lines_fit "$(render "$product" prerelease 10.10.10-alpha.10)"
    done
}

test_rendered_casks_carry_the_release() {
    OUT=$(mktemp -d)
    trap 'rm -rf "$OUT"' EXIT
    local cask
    cask=$(cat "$(render xearthlayer stable 0.5.1)")
    assert_contains "$cask" 'version "0.5.1"'
    assert_contains "$cask" "sha256 \"$SHA\""
    assert_contains "$cask" "github.com/samsoir/xearthlayer/releases/download/v#{version}"
}
