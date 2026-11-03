#!/usr/bin/env bash
# Generate app icons from the SVG source for all platforms.
# Requires: rsvg-convert (from librsvg) and iconutil (macOS built-in).
#
# Install rsvg-convert: brew install librsvg
#
# Usage: ./scripts/generate-icons.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_DIR="$PROJECT_DIR/build"
SVG="$BUILD_DIR/icon.svg"

if ! command -v rsvg-convert &>/dev/null; then
  echo "Error: rsvg-convert not found. Install with: brew install librsvg"
  exit 1
fi

echo "Generating icons from $SVG..."

# --- PNG icons for electron-builder ---
# electron-builder expects icon.png (256x256 minimum) in the build dir.
# It also uses icon.icns (macOS) and icon.ico (Windows).

SIZES=(16 32 64 128 256 512 1024)
for size in "${SIZES[@]}"; do
  rsvg-convert -w "$size" -h "$size" "$SVG" > "$BUILD_DIR/icon-${size}.png"
  echo "  ✓ icon-${size}.png"
done

# Default icon.png at 512x512 for electron-builder
cp "$BUILD_DIR/icon-512.png" "$BUILD_DIR/icon.png"
echo "  ✓ icon.png (512x512)"

# --- macOS .icns ---
ICONSET_DIR="$BUILD_DIR/icon.iconset"
rm -rf "$ICONSET_DIR"
mkdir -p "$ICONSET_DIR"

# macOS iconset requires specific filenames
rsvg-convert -w 16   -h 16   "$SVG" > "$ICONSET_DIR/icon_16x16.png"
rsvg-convert -w 32   -h 32   "$SVG" > "$ICONSET_DIR/icon_16x16@2x.png"
rsvg-convert -w 32   -h 32   "$SVG" > "$ICONSET_DIR/icon_32x32.png"
rsvg-convert -w 64   -h 64   "$SVG" > "$ICONSET_DIR/icon_32x32@2x.png"
rsvg-convert -w 128  -h 128  "$SVG" > "$ICONSET_DIR/icon_128x128.png"
rsvg-convert -w 256  -h 256  "$SVG" > "$ICONSET_DIR/icon_128x128@2x.png"
rsvg-convert -w 256  -h 256  "$SVG" > "$ICONSET_DIR/icon_256x256.png"
rsvg-convert -w 512  -h 512  "$SVG" > "$ICONSET_DIR/icon_256x256@2x.png"
rsvg-convert -w 512  -h 512  "$SVG" > "$ICONSET_DIR/icon_512x512.png"
rsvg-convert -w 1024 -h 1024 "$SVG" > "$ICONSET_DIR/icon_512x512@2x.png"

if command -v iconutil &>/dev/null; then
  iconutil -c icns "$ICONSET_DIR" -o "$BUILD_DIR/icon.icns"
  echo "  ✓ icon.icns"
else
  echo "  ⚠ iconutil not found (macOS only), skipping .icns"
fi

# Clean up iconset directory
rm -rf "$ICONSET_DIR"

# --- Windows .ico ---
# electron-builder can generate .ico from .png, but if we have
# ImageMagick available, we can create a proper multi-resolution .ico
if command -v convert &>/dev/null; then
  convert "$BUILD_DIR/icon-16.png" "$BUILD_DIR/icon-32.png" \
          "$BUILD_DIR/icon-64.png" "$BUILD_DIR/icon-128.png" \
          "$BUILD_DIR/icon-256.png" "$BUILD_DIR/icon.ico"
  echo "  ✓ icon.ico"
else
  echo "  ⚠ ImageMagick not found, skipping .ico (electron-builder will generate from .png)"
fi

# --- Linux ---
# Linux uses icon.png (already generated above)

# --- Tray icon (monochrome, for menu bar) ---
# A simplified white-on-transparent version for the system tray
cat > "$BUILD_DIR/tray-icon.svg" << 'EOF'
<svg xmlns="http://www.w3.org/2000/svg" width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
  <path d="M6 3v12"/>
  <path d="M18 9a3 3 0 1 0 0-6 3 3 0 0 0 0 6z"/>
  <path d="M6 21a3 3 0 1 0 0-6 3 3 0 0 0 0 6z"/>
  <path d="M15 6a9 9 0 0 0-9 9"/>
  <path d="M18 15v6"/>
  <path d="M21 18h-6"/>
</svg>
EOF

# Tray icon PNGs (macOS uses @1x and @2x)
rsvg-convert -w 22 -h 22 "$BUILD_DIR/tray-icon.svg" > "$BUILD_DIR/tray-icon.png"
rsvg-convert -w 44 -h 44 "$BUILD_DIR/tray-icon.svg" > "$BUILD_DIR/tray-icon@2x.png"
echo "  ✓ tray-icon.png + @2x"

echo ""
echo "Done! Icons generated in $BUILD_DIR/"
