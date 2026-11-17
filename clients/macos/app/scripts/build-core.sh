#!/bin/sh
# Xcode build phase: compile the Rust bridge library and the mount helper.
#
# The bridge is a static library linked into the app. `quickfs-mount` is a
# separate executable because a FUSE session owns its process (signal-driven
# unmount and a watchdog that exits on a wedged kernel), so the app launches
# one helper process per mounted server.
set -eu

# A terminal-like PATH: Xcode prepends its raw toolchain, whose clang has no
# default SDK, ahead of the /usr/bin shims cargo build scripts expect.
CARGO_PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
MACOS_SDK="$(/usr/bin/xcrun --sdk macosx --show-sdk-path)"
WORKSPACE="$(cd "$PROJECT_DIR/../../.." && pwd)"
OUTPUT="$BUILT_PRODUCTS_DIR/quickfs-core"
mkdir -p "$OUTPUT"

if [ "${CONFIGURATION:-Debug}" = "Release" ]; then
    PROFILE_FLAG="--release"
    PROFILE_DIR="release"
else
    PROFILE_FLAG=""
    PROFILE_DIR="debug"
fi

rust_target() {
    case "$1" in
        arm64) echo "aarch64-apple-darwin" ;;
        x86_64) echo "x86_64-apple-darwin" ;;
        *) echo "unsupported architecture $1" >&2; exit 1 ;;
    esac
}

# Xcode exports dozens of toolchain variables meant for its own targets.
# Run cargo in a clean environment so C build scripts (ring, aws-lc, zstd)
# see the same toolchain as a terminal build. The deployment target matches the
# app so C objects (ring, aws-lc, zstd) link without version warnings; a later
# terminal build without it recompiles those crates once.
cargo_clean() {
    env -i HOME="$HOME" USER="${USER:-}" TMPDIR="${TMPDIR:-/tmp}" PATH="$CARGO_PATH" \
        SDKROOT="$MACOS_SDK" MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-26.0}" \
        ${CARGO_HOME:+CARGO_HOME="$CARGO_HOME"} ${RUSTUP_HOME:+RUSTUP_HOME="$RUSTUP_HOME"} \
        cargo "$@"
}

HOST_ARCH="$(uname -m)"
LIBRARIES=""
HELPERS=""
for ARCH in $ARCHS; do
    # A native build shares target/<profile> with ordinary terminal builds
    # instead of compiling the whole workspace a second time.
    if [ "$ARCH" = "$HOST_ARCH" ]; then
        TARGET_FLAG=""
        OUT="$WORKSPACE/target/$PROFILE_DIR"
    else
        TARGET="$(rust_target "$ARCH")"
        TARGET_FLAG="--target $TARGET"
        OUT="$WORKSPACE/target/$TARGET/$PROFILE_DIR"
    fi
    # shellcheck disable=SC2086
    cargo_clean build --manifest-path "$WORKSPACE/Cargo.toml" $TARGET_FLAG $PROFILE_FLAG \
        -p quickfs-core-bridge
    # shellcheck disable=SC2086
    cargo_clean build --manifest-path "$WORKSPACE/Cargo.toml" $TARGET_FLAG $PROFILE_FLAG \
        -p quickfs-filesystem-macfuse --features macfuse --bin quickfs-mount
    LIBRARIES="$LIBRARIES $OUT/libquickfs_core_bridge.a"
    HELPERS="$HELPERS $OUT/quickfs-mount"
done

# shellcheck disable=SC2086
lipo -create $LIBRARIES -output "$OUTPUT/libquickfs_core_bridge.a"
# shellcheck disable=SC2086
lipo -create $HELPERS -output "$OUTPUT/quickfs-mount"
