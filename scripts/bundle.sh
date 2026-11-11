#!/bin/sh
# Build a release "Claude Deck.app" in ./dist.
#   --install   also copy it to /Applications and put a shortcut on the Desktop
set -e
cd "$(dirname "$0")/.."
cargo build --release
APP="dist/Claude Deck.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/claude-deck "$APP/Contents/MacOS/claude-deck"

# App icon: assets/icon-1024.png (regenerate with scripts/make_icon.py) -> .icns
[ -f assets/icon-1024.png ] || python3 scripts/make_icon.py assets/icon-1024.png
ICONSET="$(mktemp -d)/AppIcon.iconset"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z $size $size assets/icon-1024.png --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  sips -z $((size * 2)) $((size * 2)) assets/icon-1024.png --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Claude Deck</string>
  <key>CFBundleDisplayName</key><string>Claude Deck</string>
  <key>CFBundleIdentifier</key><string>local.claude-deck</string>
  <key>CFBundleExecutable</key><string>claude-deck</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
echo "Built $APP"

if [ "$1" = "--install" ]; then
  DEST=/Applications
  [ -w "$DEST" ] || DEST="$HOME/Applications"
  mkdir -p "$DEST"
  rm -rf "$DEST/Claude Deck.app"
  cp -R "$APP" "$DEST/"
  touch "$DEST/Claude Deck.app"   # make Finder pick up the new icon
  ln -sfn "$DEST/Claude Deck.app" "$HOME/Desktop/Claude Deck"
  echo "Installed to $DEST/Claude Deck.app with a Desktop shortcut"
fi
