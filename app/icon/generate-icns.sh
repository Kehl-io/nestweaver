#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SVG="$REPO_ROOT/assets/logo-icon-light.svg"
ICONSET_DIR="$SCRIPT_DIR/NestWeaver.iconset"
ICNS_OUT="$SCRIPT_DIR/AppIcon.icns"

if ! command -v rsvg-convert &>/dev/null; then
    echo "rsvg-convert not found. Install with: brew install librsvg"
    exit 1
fi

rm -rf "$ICONSET_DIR"
mkdir -p "$ICONSET_DIR"

for size in 16 32 128 256 512; do
    rsvg-convert -w $size -h $size "$SVG" -o "$ICONSET_DIR/icon_${size}x${size}.png"
    double=$((size * 2))
    rsvg-convert -w $double -h $double "$SVG" -o "$ICONSET_DIR/icon_${size}x${size}@2x.png"
done

iconutil -c icns "$ICONSET_DIR" -o "$ICNS_OUT"
rm -rf "$ICONSET_DIR"

echo "Generated $ICNS_OUT"
