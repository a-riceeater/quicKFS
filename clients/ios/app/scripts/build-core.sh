#!/bin/sh
# Xcode build phase: compile the Rust bridge as a static library for the
# platform being built (an iPhone or iPad, or the simulator on Apple silicon).
# iOS has no mount helper; the app talks to the server only through the bridge.
set -eu

# A terminal-like PATH: Xcode prepends its raw toolchain, whose clang has no
# default SDK, ahead of the /usr/bin shims cargo build scripts expect.
CARGO_PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
PLATFORM="${PLATFORM_NAME:-iphonesimulator}"
SDK="$(/usr/bin/xcrun --sdk "$PLATFORM" --show-sdk-path)"
WORKSPACE="$(cd "$PROJECT_DIR/../../.." && pwd)"
OUTPUT="$BUILT_PRODUCTS_DIR/quickfs-core"
mkdir -p "$OUTPUT"

case "$PLATFORM" in
    iphoneos) TARGET="aarch64-apple-ios" ;;
    iphonesimulator) TARGET="aarch64-apple-ios-sim" ;;
    *) echo "unsupported platform $PLATFORM" >&2; exit 1 ;;
esac

if [ "${CONFIGURATION:-Debug}" = "Release" ]; then
    PROFILE_FLAG="--release"
    PROFILE_DIR="release"
else
    PROFILE_FLAG=""
    PROFILE_DIR="debug"
fi

# Run cargo in a clean environment so C build scripts (ring, aws-lc, zstd)
# see the iOS SDK rather than the dozens of toolchain variables Xcode exports
# for its own targets. The deployment target matches the app's so C objects
# link without version warnings.
# shellcheck disable=SC2086
env -i HOME="$HOME" USER="${USER:-}" TMPDIR="${TMPDIR:-/tmp}" PATH="$CARGO_PATH" \
    SDKROOT="$SDK" IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-18.0}" \
    ${CARGO_HOME:+CARGO_HOME="$CARGO_HOME"} ${RUSTUP_HOME:+RUSTUP_HOME="$RUSTUP_HOME"} \
    cargo build --manifest-path "$WORKSPACE/Cargo.toml" --target "$TARGET" $PROFILE_FLAG \
    -p quickfs-core-bridge

cp "$WORKSPACE/target/$TARGET/$PROFILE_DIR/libquickfs_core_bridge.a" "$OUTPUT/libquickfs_core_bridge.a"
