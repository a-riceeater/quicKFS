#!/bin/sh
# Build a signed, update-ready quicKFS.app and its Sparkle appcast entry.
#
#   SIGN_IDENTITY="Developer ID Application: …"  (default: Signing.xcconfig)
#   TEAM_ID=ABCDE12345                            (default: Signing.xcconfig)
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
    SPARKLE_PUBLIC_KEY="${SPARKLE_PUBLIC_KEY:-}"
# Without SIGN_IDENTITY, signing comes from Signing.xcconfig / Local.xcconfig.
[ -n "${SIGN_IDENTITY:-}" ] && set -- "$@" CODE_SIGN_IDENTITY="$SIGN_IDENTITY"
[ -n "${TEAM_ID:-}" ] && set -- "$@" DEVELOPMENT_TEAM="$TEAM_ID"
[ -n "${VERSION:-}" ] && set -- "$@" MARKETING_VERSION="$VERSION"
[ -n "${BUILD:-}" ] && set -- "$@" CURRENT_PROJECT_VERSION="$BUILD"
# Library validation needs a shared Team ID, which ad-hoc signatures lack, so
# only a real identity gets the hardened runtime that notarization requires.
if [ -n "${SIGN_IDENTITY:-}" ] && [ "$SIGN_IDENTITY" != "-" ]; then
    set -- "$@" ENABLE_HARDENED_RUNTIME=YES OTHER_CODE_SIGN_FLAGS="--timestamp"
fi
LOG="$OUT/xcodebuild.log"
if ! xcodebuild "$@" build > "$LOG" 2>&1; then
    grep -E "error" "$LOG" | tail -20
    echo "Build failed; full log: $LOG" >&2
    exit 1
fi

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
