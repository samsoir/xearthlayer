#!/usr/bin/env bash
# Builds the Linux release binaries the way release.yml's build-binary job
# does, in an ubuntu:22.04 container, and applies the same glibc floor check.
#
# release.yml runs only on a tag push, so no PR check builds in that
# container: run this before tagging. PR CI builds on a newer glibc, where a
# new libm or libc call can raise the floor without any visible change
# (0.5.1-beta.1 was stopped by hypot@GLIBC_2.35).
#
# Usage: scripts/linux/check-glibc-floor.sh   (needs podman or docker)
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
# Keep in step with GLIBC_FLOOR and the container image in release.yml.
GLIBC_FLOOR=${GLIBC_FLOOR:-2.34}
IMAGE=ubuntu:22.04

engine=$(command -v podman || command -v docker || true)
if [ -z "$engine" ]; then
    echo "error: podman or docker is required" >&2
    exit 1
fi

# The source goes in as an archive of the files git knows about (tracked, and
# untracked but not ignored), listed on the host: inside the container a git
# worktree's .git points at a host path that does not exist there, and the
# host's target/ (built against the host's glibc) must never be used. A named
# volume keeps cargo's registry and the container's target/ between runs.
src=$(mktemp -d "${TMPDIR:-/tmp}/xel-glibc-floor.XXXXXX")
trap 'rm -rf "$src"' EXIT
git -C "$repo_root" ls-files -z --cached --others --exclude-standard \
    | (cd "$repo_root" && tar --null -T - -cf "$src/source.tar")

"$engine" run --rm \
    -v "$src:/src:ro,z" \
    -v xel-glibc-floor-cache:/cache \
    -e GLIBC_FLOOR="$GLIBC_FLOOR" \
    -e CARGO_HOME=/cache/cargo -e RUSTUP_HOME=/cache/rustup \
    -e CARGO_TARGET_DIR=/cache/target \
    "$IMAGE" bash -euo pipefail -c '
        export DEBIAN_FRONTEND=noninteractive PATH=/cache/cargo/bin:$PATH
        apt-get update -qq
        apt-get install -y -qq --no-install-recommends \
            build-essential pkg-config libfuse3-dev curl ca-certificates git binutils >/dev/null
        command -v rustup >/dev/null || \
            curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none >/dev/null
        mkdir -p /build && tar -xf /src/source.tar -C /build
        cd /build && cargo build --release --quiet
        status=0
        for bin in xearthlayer xearthlayer-publish; do
            MAX=$(objdump -T "$CARGO_TARGET_DIR/release/$bin" \
                | grep -oE "GLIBC_[0-9]+\.[0-9]+(\.[0-9]+)?" \
                | sed "s/GLIBC_//" | sort -V -u | tail -1)
            echo "$bin: highest required glibc symbol: $MAX (floor: $GLIBC_FLOOR)"
            if [ "$(printf "%s\n%s\n" "$MAX" "$GLIBC_FLOOR" | sort -V | tail -1)" != "$GLIBC_FLOOR" ]; then
                echo "error: $bin requires glibc $MAX, above the floor $GLIBC_FLOOR:" >&2
                objdump -T "$CARGO_TARGET_DIR/release/$bin" | grep -F "GLIBC_$MAX" >&2
                status=1
            fi
        done
        exit $status
    '
