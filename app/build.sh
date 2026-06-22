#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
APP_DIR="$REPO_ROOT/target/release/NestWeaver.app"

echo "=== Building NestWeaver.app ==="

# Step 1: Build Rust binary
echo "[1/4] Building Rust binary..."
cd "$REPO_ROOT"
cargo build --release --features embed,metal

# Step 2: Generate icons if needed
ICNS="$SCRIPT_DIR/icon/AppIcon.icns"
MENU_ICON="$SCRIPT_DIR/icon/MenuIcon.png"
if [ ! -f "$ICNS" ]; then
    echo "[2/5] Generating app icon..."
    "$SCRIPT_DIR/icon/generate-icns.sh"
else
    echo "[2/5] App icon already exists, skipping"
fi

# Generate menubar template icon (18x18, transparent bg)
if [ ! -f "$MENU_ICON" ] && command -v rsvg-convert &>/dev/null; then
    echo "[2b/5] Generating menubar icon..."
    rsvg-convert -w 36 -h 36 "$REPO_ROOT/assets/logo-icon-dark.svg" -o "$MENU_ICON"
elif [ ! -f "$MENU_ICON" ] && command -v sips &>/dev/null; then
    echo "[2b/5] Generating menubar icon (sips)..."
    # Convert SVG to PNG via temporary — sips can't read SVG directly
    # Fall back: the Swift launcher will use AppIcon.icns with isTemplate=true
    echo "[2b/5] Skipping menubar icon (no rsvg-convert), will use app icon as template"
else
    echo "[2b/5] Menubar icon already exists"
fi

# Step 3: Compile Swift launcher
echo "[3/4] Compiling Swift launcher..."
swiftc "$SCRIPT_DIR/Sources/main.swift" \
    -o "$REPO_ROOT/target/release/NestWeaverLauncher" \
    -framework AppKit \
    -O

# Step 4: Assemble .app bundle
echo "[4/4] Assembling app bundle..."
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS"
mkdir -p "$APP_DIR/Contents/Resources"

cp "$SCRIPT_DIR/Info.plist" "$APP_DIR/Contents/"
cp "$REPO_ROOT/target/release/NestWeaverLauncher" "$APP_DIR/Contents/MacOS/NestWeaver"
cp "$REPO_ROOT/target/release/nestweaver" "$APP_DIR/Contents/MacOS/nestweaver-cli"
cp "$ICNS" "$APP_DIR/Contents/Resources/AppIcon.icns"
if [ -f "$MENU_ICON" ]; then
    cp "$MENU_ICON" "$APP_DIR/Contents/Resources/MenuIcon.png"
fi

echo -n "APPL????" > "$APP_DIR/Contents/PkgInfo"

echo ""
echo "=== Built: $APP_DIR ==="
echo "Run with: open $APP_DIR"
