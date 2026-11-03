#!/usr/bin/env bash
# Regenerate the desktop icons from packages/web/crab.optimized.svg.
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BUILD_DIR="$PROJECT_DIR/build"
SOURCE_SVG="$PROJECT_DIR/../web/crab.optimized.svg"
APP_SVG="$BUILD_DIR/icon.svg"

if ! command -v rsvg-convert >/dev/null; then
  echo "error: rsvg-convert is required to regenerate desktop icons" >&2
  exit 1
fi

# The white outline keeps the black tile visible against dark desktop backgrounds.
cat > "$APP_SVG" <<'EOF'
<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">
  <rect x="14" y="14" width="996" height="996" rx="216" fill="black" stroke="white" stroke-width="20"/>
  <g transform="translate(-88 -109) scale(3)" fill="white">
EOF
sed '1d;$d' "$SOURCE_SVG" >> "$APP_SVG"
cat >> "$APP_SVG" <<'EOF'
  </g>
</svg>
EOF

cp "$APP_SVG" "$PROJECT_DIR/public/icon.svg"
cp "$SOURCE_SVG" "$PROJECT_DIR/src/assets/crab.svg"
cp "$SOURCE_SVG" "$BUILD_DIR/tray-icon.svg"
for size in 16 32 64 128 256 512 1024; do
  rsvg-convert -w "$size" -h "$size" -o "$BUILD_DIR/icon-$size.png" "$APP_SVG"
done
cp "$BUILD_DIR/icon-512.png" "$BUILD_DIR/icon.png"
rsvg-convert -w 22 -h 22 -o "$BUILD_DIR/tray-icon.png" "$SOURCE_SVG"
rsvg-convert -w 44 -h 44 -o "$BUILD_DIR/tray-icon@2x.png" "$SOURCE_SVG"

if command -v iconutil >/dev/null; then
  temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/crab-icons.XXXXXX")"
  trap 'rm -r "$temp_dir"' EXIT
  iconset="$temp_dir/icon.iconset"
  mkdir "$iconset"
  for size in 16 32 128 256 512; do
    cp "$BUILD_DIR/icon-$size.png" "$iconset/icon_${size}x${size}.png"
    cp "$BUILD_DIR/icon-$((size * 2)).png" "$iconset/icon_${size}x${size}@2x.png"
  done
  iconutil -c icns "$iconset" -o "$BUILD_DIR/icon.icns"
else
  echo "macOS icon not regenerated: iconutil is available on macOS only"
fi
echo "Desktop icons regenerated from $SOURCE_SVG"
