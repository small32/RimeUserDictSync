#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
APP_NAME=RimeUserDictSync
DIST="$ROOT/dist"
ARM_APP="$DIST/arm64/$APP_NAME.app"
X86_APP="$DIST/x86_64/$APP_NAME.app"
UNIVERSAL_APP="$DIST/universal/$APP_NAME.app"
UNIVERSAL_BIN="$ROOT/target/universal/$APP_NAME"

make_app() {
    binary=$1
    app=$2
    rm -rf "$app"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cp "$binary" "$app/Contents/MacOS/$APP_NAME"
    cp "$ROOT/macos/Info.plist" "$app/Contents/Info.plist"
    sips -s format icns "$ROOT/weasel.ico" \
        --out "$app/Contents/Resources/AppIcon.icns" >/dev/null
    codesign --force --deep --sign - "$app"
}

cd "$ROOT"
cargo build --release --locked --target aarch64-apple-darwin
cargo build --release --locked --target x86_64-apple-darwin

mkdir -p "$DIST/arm64" "$DIST/x86_64" "$DIST/universal" "$(dirname "$UNIVERSAL_BIN")"
lipo -create \
    "$ROOT/target/aarch64-apple-darwin/release/$APP_NAME" \
    "$ROOT/target/x86_64-apple-darwin/release/$APP_NAME" \
    -output "$UNIVERSAL_BIN"

make_app "$ROOT/target/aarch64-apple-darwin/release/$APP_NAME" "$ARM_APP"
make_app "$ROOT/target/x86_64-apple-darwin/release/$APP_NAME" "$X86_APP"
make_app "$UNIVERSAL_BIN" "$UNIVERSAL_APP"

rm -f \
    "$DIST/$APP_NAME-macOS-arm64.zip" \
    "$DIST/$APP_NAME-macOS-x86_64.zip" \
    "$DIST/$APP_NAME-macOS-universal.zip"
ditto -c -k --sequesterRsrc --keepParent "$ARM_APP" "$DIST/$APP_NAME-macOS-arm64.zip"
ditto -c -k --sequesterRsrc --keepParent "$X86_APP" "$DIST/$APP_NAME-macOS-x86_64.zip"
ditto -c -k --sequesterRsrc --keepParent "$UNIVERSAL_APP" "$DIST/$APP_NAME-macOS-universal.zip"

echo "Created $DIST/$APP_NAME-macOS-arm64.zip"
echo "Created $DIST/$APP_NAME-macOS-x86_64.zip"
echo "Created $DIST/$APP_NAME-macOS-universal.zip"
