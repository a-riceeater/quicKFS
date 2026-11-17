#!/bin/sh
# Build a signed, update-ready quicKFS.app and its Sparkle appcast entry.
#
#   SIGN_IDENTITY="Developer ID Application: …"  (default: ad-hoc "-")
#   TEAM_ID=ABCDE12345                            (needed with a Developer ID)
#   SPARKLE_PUBLIC_KEY=…                          (from Sparkle's generate_keys)
#   NOTARY_PROFILE=quickfs                        (optional notarytool profile)
#   VERSION=0.2.0 BUILD=2                         (default: project.yml values)
#
# Output lands in clients/macos/app/build/release: the zipped app and an
# appcast.xml signed with the EdDSA key held in your login Keychain. Upload
# both to the release that SPARKLE_FEED_URL points at.
set -eu

APP_DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$APP_DIR/build/release"
DERIVED="$APP_DIR/build/DerivedData"
mkdir -p "$OUT"

cd "$APP_DIR"
xcodegen generate --quiet

set -- -project quicKFS.xcodeproj -scheme quicKFS -configuration Release \
    -derivedDataPath "$DERIVED" \
    CODE_SIGN_IDENTITY="${SIGN_IDENTITY:--}" \
    DEVELOPMENT_TEAM="${TEAM_ID:-}" \
    SPARKLE_PUBLIC_KEY="${SPARKLE_PUBLIC_KEY:-}"
[ -n "${VERSION:-}" ] && set -- "$@" MARKETING_VERSION="$VERSION"
[ -n "${BUILD:-}" ] && set -- "$@" CURRENT_PROJECT_VERSION="$BUILD"
if [ "${SIGN_IDENTITY:--}" != "-" ]; then
    set -- "$@" OTHER_CODE_SIGN_FLAGS="--timestamp"
else
    # Library validation needs a shared Team ID, which ad-hoc signatures lack,
    # so a hardened ad-hoc app cannot load its embedded Sparkle framework.
    # Notarized Developer ID builds keep the hardened runtime.
    set -- "$@" ENABLE_HARDENED_RUNTIME=NO
fi
xcodebuild "$@" build

APP="$DERIVED/Build/Products/Release/quicKFS.app"
codesign --verify --deep --strict "$APP"
SHORT="$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$APP/Contents/Info.plist")"
ZIP="$OUT/quicKFS-$SHORT.zip"
rm -f "$ZIP"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"

if [ -n "${NOTARY_PROFILE:-}" ]; then
    xcrun notarytool submit "$ZIP" --keychain-profile "$NOTARY_PROFILE" --wait
    xcrun stapler staple "$APP"
    rm -f "$ZIP"
    ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"
fi

GENERATE="$DERIVED/SourcePackages/artifacts/sparkle/Sparkle/bin/generate_appcast"
if [ -n "${SPARKLE_PUBLIC_KEY:-}" ] && [ -x "$GENERATE" ]; then
    "$GENERATE" "$OUT"
    echo "Appcast: $OUT/appcast.xml"
else
    echo "Skipping appcast: set SPARKLE_PUBLIC_KEY to publish updates."
fi
echo "App archive: $ZIP"
