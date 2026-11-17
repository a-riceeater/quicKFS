#!/bin/sh
# Xcode build phase: place the mount helper in Contents/Helpers and sign it
# before Xcode seals the app bundle.
set -eu

HELPERS="$TARGET_BUILD_DIR/$CONTENTS_FOLDER_PATH/Helpers"
mkdir -p "$HELPERS"
cp -f "$BUILT_PRODUCTS_DIR/quickfs-core/quickfs-mount" "$HELPERS/quickfs-mount"

IDENTITY="${EXPANDED_CODE_SIGN_IDENTITY:--}"
if [ -z "$IDENTITY" ]; then
    IDENTITY="-"
fi
if [ "${ENABLE_HARDENED_RUNTIME:-NO}" = "YES" ]; then
    # macFUSE's libfuse is signed by its own team, so the hardened helper must
    # opt out of library validation to load it.
    codesign --force --sign "$IDENTITY" --options runtime --timestamp=none \
        --entitlements "$PROJECT_DIR/quicKFS/Resources/Helper.entitlements" \
        "$HELPERS/quickfs-mount"
else
    codesign --force --sign "$IDENTITY" --timestamp=none "$HELPERS/quickfs-mount"
fi
