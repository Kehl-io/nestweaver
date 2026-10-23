#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
APP_DIR="$REPO_ROOT/target/release/NestWeaver.app"
VALIDATION_ARTIFACTS=""
if [ "$#" -gt 0 ]; then
    if [ "$#" -ne 2 ] || [ "$1" != "--validation-artifacts" ]; then
        echo "Usage: $0 [--validation-artifacts DIRECTORY]" >&2
        exit 64
    fi
    VALIDATION_ARTIFACTS="$2"
fi
if [ "$(uname -s)" != "Darwin" ]; then
    echo "NestWeaver.app must be built and validated on macOS" >&2
    exit 1
fi
# LadybugDB uses floating-point std::format, available since macOS 13.3.
export MACOSX_DEPLOYMENT_TARGET=13.3
APP_ARCH="$(uname -m)"
case "$APP_ARCH" in
    arm64|x86_64) SWIFT_TARGET="$APP_ARCH-apple-macosx$MACOSX_DEPLOYMENT_TARGET" ;;
    *)
        echo "Unsupported macOS architecture: $APP_ARCH" >&2
        exit 1
        ;;
esac

echo "=== Building NestWeaver.app ==="

# Step 1: Build frontend
echo "[1/6] Building frontend..."
cd "$REPO_ROOT/crates/nestweaver-web/frontend"
npm ci
npm run build
cd "$REPO_ROOT"

# Step 2: Build Rust binary
echo "[2/6] Building Rust binary..."
if [ -n "$VALIDATION_ARTIFACTS" ]; then
    # The canonical app path retains release test artifacts in the same Cargo
    # invocation, then packages the exact standard CLI produced by that build.
    python3 scripts/build-validation-artifacts.py --release --output "$VALIDATION_ARTIFACTS"
else
    cargo build --release --features embed,metal
fi

# Step 3: Generate icons if needed
ICNS="$SCRIPT_DIR/icon/AppIcon.icns"
MENU_ICON="$SCRIPT_DIR/icon/MenuIcon.png"
if [ ! -f "$ICNS" ]; then
    echo "[3/6] Generating app icon..."
    "$SCRIPT_DIR/icon/generate-icns.sh"
else
    echo "[3/6] App icon already exists, skipping"
fi

# Generate menubar template icon (18x18, transparent bg)
if [ ! -f "$MENU_ICON" ] && command -v rsvg-convert &>/dev/null; then
    echo "[3b/6] Generating menubar icon..."
    rsvg-convert -w 36 -h 36 "$REPO_ROOT/assets/logo-icon-light.svg" -o "$MENU_ICON"
elif [ ! -f "$MENU_ICON" ] && command -v sips &>/dev/null; then
    echo "[3b/6] Generating menubar icon (sips)..."
    # Convert SVG to PNG via temporary — sips can't read SVG directly
    # Fall back: the Swift launcher will use AppIcon.icns with isTemplate=true
    echo "[3b/6] Skipping menubar icon (no rsvg-convert), will use app icon as template"
else
    echo "[3b/6] Menubar icon already exists"
fi

# Step 4: Compile Swift launcher
echo "[4/6] Compiling Swift launcher..."
swiftc "$SCRIPT_DIR/Sources/main.swift" "$SCRIPT_DIR/Sources/DatabaseSelection.swift" "$SCRIPT_DIR/Sources/UIChildReadiness.swift" \
    -o "$REPO_ROOT/target/release/NestWeaverLauncher" \
    -target "$SWIFT_TARGET" \
    -framework AppKit \
    -O

# Step 5: Assemble .app bundle
echo "[5/6] Assembling app bundle..."
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS"
mkdir -p "$APP_DIR/Contents/Resources"

cp "$SCRIPT_DIR/Info.plist" "$APP_DIR/Contents/"
# Stamp the bundle version from version.txt (release-please keeps it current)
# so Finder/About match the nestweaver binary the bundle wraps.
VERSION="$(tr -d '[:space:]' < "$REPO_ROOT/version.txt")"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$APP_DIR/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $VERSION" "$APP_DIR/Contents/Info.plist"
echo "      Stamped app bundle version: $VERSION"
cp "$REPO_ROOT/target/release/NestWeaverLauncher" "$APP_DIR/Contents/MacOS/NestWeaver"
cp "$REPO_ROOT/target/release/nestweaver" "$APP_DIR/Contents/MacOS/nestweaver-cli"
if [ -n "$VALIDATION_ARTIFACTS" ]; then
    python3 - "$VALIDATION_ARTIFACTS/artifacts.json" "$APP_DIR/Contents/MacOS/nestweaver-cli" <<'PYTHON'
import hashlib, json, pathlib, sys
manifest = json.loads(pathlib.Path(sys.argv[1]).read_text())
cli = next(item for item in manifest['executables'] if item['path'] == manifest['cli'])
assert hashlib.sha256(pathlib.Path(sys.argv[2]).read_bytes()).hexdigest() == cli['sha256'], 'packaged CLI differs from retained build'
PYTHON
fi
cp "$ICNS" "$APP_DIR/Contents/Resources/AppIcon.icns"
if [ -f "$MENU_ICON" ]; then
    cp "$MENU_ICON" "$APP_DIR/Contents/Resources/MenuIcon.png"
fi

echo -n "APPL????" > "$APP_DIR/Contents/PkgInfo"

echo ""
echo "=== Built: $APP_DIR ==="
echo "Run with: open $APP_DIR"
