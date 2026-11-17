#!/bin/sh
# Xcode build phase: compile the Rust bridge library and the mount helper.
#
# The bridge is a static library linked into the app. `quickfs-mount` is a
# separate executable because a FUSE session owns its process (signal-driven
# unmount and a watchdog that exits on a wedged kernel), so the app launches
# one helper process per mounted server.
set -eu

export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
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

# Xcode exports toolchain variables meant for Swift and C targets; keep them
# away from cargo build scripts, which build host tools with their own flags.
unset SDKROOT LIBRARY_PATH CPATH

LIBRARIES=""
HELPERS=""
for ARCH in $ARCHS; do
    TARGET="$(rust_target "$ARCH")"
    # shellcheck disable=SC2086
    cargo build --manifest-path "$WORKSPACE/Cargo.toml" --target "$TARGET" $PROFILE_FLAG \
        -p quickfs-core-bridge
    # shellcheck disable=SC2086
    cargo build --manifest-path "$WORKSPACE/Cargo.toml" --target "$TARGET" $PROFILE_FLAG \
        -p quickfs-filesystem-macfuse --features macfuse --bin quickfs-mount
    LIBRARIES="$LIBRARIES $WORKSPACE/target/$TARGET/$PROFILE_DIR/libquickfs_core_bridge.a"
    HELPERS="$HELPERS $WORKSPACE/target/$TARGET/$PROFILE_DIR/quickfs-mount"
done

# shellcheck disable=SC2086
lipo -create $LIBRARIES -output "$OUTPUT/libquickfs_core_bridge.a"
# shellcheck disable=SC2086
lipo -create $HELPERS -output "$OUTPUT/quickfs-mount"
