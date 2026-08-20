#!/usr/bin/env bash
# Build inm in release mode and assemble a macOS .app bundle so Spotlight/
# Launchpad can find it — modeled on ../canopy/scripts/bundle-mac.sh, minus
# the sibling-binary bit (inm is a single executable).
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

APP_NAME="inm"
BUNDLE_ID="dev.loyalpartner.inm"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
INSTALL_DIR="${INM_APP_DEST:-$HOME/Applications}"
APP_DIR="$INSTALL_DIR/$APP_NAME.app"

echo "==> Building release binary"
PKG_CONFIG_PATH="${PKG_CONFIG_PATH:-/opt/homebrew/opt/spice-gtk/lib/pkgconfig}" \
    cargo build --release

echo "==> Assembling $APP_DIR"
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"

cp target/release/inm "$APP_DIR/Contents/MacOS/inm"
cp assets/mac/AppIcon.icns "$APP_DIR/Contents/Resources/AppIcon.icns"

cat > "$APP_DIR/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleExecutable</key>
    <string>inm</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.utilities</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSHumanReadableCopyright</key>
    <string>inm</string>
</dict>
</plist>
PLIST

echo "==> Registering with Launch Services / Spotlight"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$APP_DIR"
mdimport "$APP_DIR" >/dev/null 2>&1 || true

echo "==> Done: $APP_DIR"
echo "    Spotlight 里搜 \"$APP_NAME\" 应该就能找到了（如果没马上出现，等几秒让 mds 建完索引）。"
