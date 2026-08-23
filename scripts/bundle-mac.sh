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

# Substituted from a template rather than written by a heredoc. A heredoc
# is streamed through a pipe, and where the kernel hands out an undersized
# pipe buffer — which happens on a long-lived macOS box whose pipe submap
# has fragmented — bash blocks writing it and the script hangs after
# "Assembling" with an empty Info.plist and no further output. `sed` writes
# straight to the file. (Same fix, same reason, as canopy's bundler.)
#
# The template also earns its keep on its own: it can be linted, diffed,
# and opened by plist tooling, which a 30-line string in a shell script
# cannot.
sed -e "s|@APP_NAME@|$APP_NAME|g" \
    -e "s|@BUNDLE_ID@|$BUNDLE_ID|g" \
    -e "s|@VERSION@|$VERSION|g" \
    assets/mac/Info.plist.in > "$APP_DIR/Contents/Info.plist"

# A malformed plist gives a bundle that silently refuses to launch, so it
# fails here instead.
plutil -lint "$APP_DIR/Contents/Info.plist" >/dev/null

# Prove what was produced can actually run, rather than reporting success
# because no step errored. inm has no CLI flag to invoke (main() just opens
# a window), so check the two things that make a bundle assemble perfectly
# and then refuse to open: the wrong architecture, and a dylib that is not
# where the binary expects it. inm links Homebrew's spice-gtk by absolute
# path, so an .app built here stops working the moment that keg moves.
echo "==> Verifying"
BIN="$APP_DIR/Contents/MacOS/inm"
test -x "$BIN"
if ! lipo -archs "$BIN" | tr ' ' '\n' | grep -qx "$(uname -m)"; then
    echo "    !! $BIN 不含本机架构 $(uname -m)：$(lipo -archs "$BIN")" >&2
    exit 1
fi
missing=0
while read -r dylib; do
    case "$dylib" in
        /usr/lib/*|/System/*|@*) continue ;;
    esac
    if [ ! -e "$dylib" ]; then
        echo "    !! 缺少依赖库：$dylib" >&2
        missing=1
    fi
done < <(otool -L "$BIN" | tail -n +2 | awk '{print $1}')
[ "$missing" -eq 0 ] || exit 1

echo "==> Registering with Launch Services / Spotlight"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$APP_DIR"
mdimport "$APP_DIR" >/dev/null 2>&1 || true

echo "==> Done: $APP_DIR"
echo "    Spotlight 里搜 \"$APP_NAME\" 应该就能找到了（如果没马上出现，等几秒让 mds 建完索引）。"
