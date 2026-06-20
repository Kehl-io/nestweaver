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

# Step 2: Generate icon if needed
ICNS="$SCRIPT_DIR/icon/AppIcon.icns"
if [ ! -f "$ICNS" ]; then
    echo "[2/4] Generating icon..."
    "$SCRIPT_DIR/icon/generate-icns.sh"
else
    echo "[2/4] Icon already exists, skipping"
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

echo -n "APPL????" > "$APP_DIR/Contents/PkgInfo"

echo ""
echo "=== Built: $APP_DIR ==="
echo "Run with: open $APP_DIR"
