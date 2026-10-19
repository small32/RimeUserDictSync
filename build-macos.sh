#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
APP_NAME=RimeUserDictSync
DIST="$ROOT/dist"
ARM_APP="$DIST/arm64/$APP_NAME.app"
X86_APP="$DIST/x86_64/$APP_NAME.app"
UNIVERSAL_APP="$DIST/universal/$APP_NAME.app"
UNIVERSAL_BIN="$ROOT/target/universal/$APP_NAME"
MENUBAR_BIN="$DIST/tools/RimeMenuBar"

make_app() {
    binary=$1
    app=$2
    rm -rf "$app"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cp "$binary" "$app/Contents/MacOS/$APP_NAME"
    cp "$ROOT/macos/Info.plist" "$app/Contents/Info.plist"
    /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$app/Contents/Info.plist"
    /usr/libexec/PlistBuddy -c "Set :CFBundleVersion $VERSION" "$app/Contents/Info.plist"
    cp "$ROOT/macos/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"

    # 菜单栏助手单独装成一个嵌套的 .app，必须有**自己的** bundle id。
    # 早先直接放在 Contents/MacOS/RimeMenuBar：它没有 Info.plist，NSBundle 会
    # 认到外层主程序，LaunchServices 于是把"助手在跑"当成"主程序在跑"，
    # 双击主程序只会激活助手——没有窗口，看起来就是"启动没反应"。
    helper="$app/Contents/Resources/RimeMenuBar.app"
    mkdir -p "$helper/Contents/MacOS" "$helper/Contents/Resources"
    cp "$MENUBAR_BIN" "$helper/Contents/MacOS/RimeMenuBar"
    cp "$ROOT/macos/RimeMenuBar-Info.plist" "$helper/Contents/Info.plist"
    /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$helper/Contents/Info.plist"
    /usr/libexec/PlistBuddy -c "Set :CFBundleVersion $VERSION" "$helper/Contents/Info.plist"
    cp "$ROOT/macos/sync-gear-template.png" "$helper/Contents/Resources/sync-gear-template.png"
    cp "$ROOT/macos/AppIcon.icns" "$helper/Contents/Resources/AppIcon.icns"
    # 嵌套 .app 必须**单独签**：codesign --deep 不会进 Resources/ 里的 bundle，
    # 不签的话 launchd bootstrap 直接报 "Input/output error"。
    codesign --force --deep --sign - \
        --identifier com.small32.rime-user-dict-sync.menubar2 "$helper"

    codesign --force --deep --sign - "$app"
}

cd "$ROOT"
VERSION=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
cargo build --release --locked --target aarch64-apple-darwin
cargo build --release --locked --target x86_64-apple-darwin

# 菜单栏助手：Objective-C + AppKit，编译成 universal 后塞进 .app。
# 用 ObjC 而不是 Swift：只有 Command Line Tools 的机器上，Swift 兼容库没有
# x86_64 切片，交叉链接会失败；clang 直接两个架构都能编。
mkdir -p "$DIST/tools"
clang -O2 -fobjc-arc -target arm64-apple-macos11.0 -framework Cocoa \
    "$ROOT/macos/RimeMenuBar.m" -o "$DIST/tools/RimeMenuBar-arm64"
clang -O2 -fobjc-arc -target x86_64-apple-macos11.0 -framework Cocoa \
    "$ROOT/macos/RimeMenuBar.m" -o "$DIST/tools/RimeMenuBar-x86_64"
lipo -create \
    "$DIST/tools/RimeMenuBar-arm64" \
    "$DIST/tools/RimeMenuBar-x86_64" \
    -output "$MENUBAR_BIN"

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
