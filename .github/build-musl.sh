#!/bin/sh
# Build the Linux release binary: fully static, linked against musl, so it
# runs on any distro regardless of its glibc. Runs inside the official Rust
# Alpine image, whose host target is <arch>-unknown-linux-musl:
#
#   docker run --rm -v "$PWD:/src" -w /src -e IOMARK_RELEASE_TAG \
#     rust:1-alpine .github/build-musl.sh
#
# The binary lands in target/<arch>-unknown-linux-musl/release/iomark. Pass
# --platform linux/amd64 or linux/arm64 to docker for the other architecture.
#
# A glibc build is not an option: whatever glibc the build host has becomes
# the floor. The Ubuntu 24.04 runner stamped GLIBC_2.38 symbol versions into
# fio's objects and linked libaio.so.1t64, so the binary died in the loader on
# every system older than the runner.
set -eu

# libaio-dev and zlib-static carry the archives build.rs links on musl.
apk add --no-cache build-base linux-headers libaio-dev zlib-dev zlib-static git

# The bind-mounted checkout belongs to another uid, and git refuses to read
# it then: build.rs would lose `git describe` and the fio source identity.
git config --global --add safe.directory '*'

# An explicit --target keeps the output out of target/release, which a native
# build on the same checkout owns.
target=$(rustc -vV | sed -n 's/^host: //p')
cargo build --release --target "$target"
