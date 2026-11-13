#!/usr/bin/env bash
# Builds the app with SwiftPM (Command Line Tools are enough; no Xcode
# license needed) and wraps the binary in build/Toolbox.app.
#
#   CONFIG=debug|release   (default debug)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CONFIG="${CONFIG:-debug}"
APP="$ROOT/build/Toolbox.app"
unset DEVELOPER_DIR

# The Command Line Tools lack SwiftUI's macro plugin; borrow Xcode's copy.
SWIFT_FLAGS=()
XCODE_PLUGINS="/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/usr/lib/swift/host/plugins"
if [[ -d "$XCODE_PLUGINS" ]]; then
  SWIFT_FLAGS+=(-Xswiftc -plugin-path -Xswiftc "$XCODE_PLUGINS")
fi

cd "$ROOT/App"
swift build -c "$CONFIG" --product Toolbox "${SWIFT_FLAGS[@]}"
BIN_DIR="$(swift build -c "$CONFIG" --show-bin-path)"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$APP/Contents/Frameworks"
cp "$BIN_DIR/Toolbox" "$APP/Contents/MacOS/Toolbox"
# SwiftPM resource bundles (e.g. SwiftTerm's) live next to the binary.
for bundle in "$BIN_DIR"/*.bundle; do
  [[ -e "$bundle" ]] && cp -R "$bundle" "$APP/Contents/Resources/"
done
# Package products built as dynamic libraries (RoyalVNCKit) are loaded via
# @rpath; ship them in Frameworks and point the executable there.
for dylib in "$BIN_DIR"/*.dylib; do
  [[ -e "$dylib" ]] || continue
  cp "$dylib" "$APP/Contents/Frameworks/"
  codesign --force --sign - "$APP/Contents/Frameworks/$(basename "$dylib")" >/dev/null
done
install_name_tool -add_rpath "@executable_path/../Frameworks" "$APP/Contents/MacOS/Toolbox" 2>/dev/null || true
cp "$ROOT/App/Resources/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleExecutable</key><string>Toolbox</string>
  <key>CFBundleIdentifier</key><string>com.dobbs.Toolbox</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>Toolbox</string>
  <key>CFBundleDisplayName</key><string>Toolbox</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
  <key>NSSupportsAutomaticTermination</key><false/>
  <key>NSSupportsSuddenTermination</key><false/>
</dict>
</plist>
PLIST

codesign --force --sign - "$APP" >/dev/null
echo "ok: $APP"
