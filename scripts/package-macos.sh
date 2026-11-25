#!/bin/sh
set -eu

if [ "$(uname -s)" != Darwin ]; then
    printf '%s\n' 'Run this script on macOS.' >&2
    exit 1
fi

cd "$(dirname "$0")/.."
target_dir="${CARGO_TARGET_DIR:-target}"
minimum_macos=13.0
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    QUICK_BRIDGE_BUNDLE_FONTS=1 MACOSX_DEPLOYMENT_TARGET="$minimum_macos" \
        cargo build --locked --release \
        --bin aws-quick-bridge-gui --target "$target" --target-dir "$target_dir"
done

version="$(awk -F '"' '/^version = "/ { print $2; exit }' Cargo.toml)"
bundle="$target_dir/release/Quick Bridge.app"
mkdir -p "$target_dir/release"
staging_dir="$(mktemp -d "$target_dir/release/.quick-bridge-package.XXXXXX")"
staged_bundle="$staging_dir/Quick Bridge.app"
previous_bundle="$staging_dir/previous.app"

cleanup() {
    if [ -e "$previous_bundle" ] && [ ! -e "$bundle" ]; then
        mv "$previous_bundle" "$bundle" || return
    fi
    rm -rf "$staging_dir"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

mkdir -p "$staged_bundle/Contents/MacOS" "$staged_bundle/Contents/Resources/fonts"
executable="$staged_bundle/Contents/MacOS/QuickBridge"
xcrun lipo -create -output "$executable" \
    "$target_dir/aarch64-apple-darwin/release/aws-quick-bridge-gui" \
    "$target_dir/x86_64-apple-darwin/release/aws-quick-bridge-gui"
chmod 755 "$executable"
for arch in arm64 x86_64; do
    xcrun lipo "$executable" -verify_arch "$arch"
    minos="$(xcrun vtool -arch "$arch" -show-build "$executable" | awk '$1 == "minos" { print $2 }')"
    if [ "$minos" != "$minimum_macos" ]; then
        printf 'Expected macOS %s for %s, found %s\n' "$minimum_macos" "$arch" "$minos" >&2
        exit 1
    fi
done

install -m 644 assets/app-icon.icns "$staged_bundle/Contents/Resources/AppIcon.icns"
for font in \
    assets/fonts/MiSans-Regular.ttf \
    assets/fonts/MiSans-Medium.ttf
do
    gzip -n -9 -c "$font" > "$staged_bundle/Contents/Resources/fonts/${font##*/}.gz"
done

cat > "$staged_bundle/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Quick Bridge</string>
    <key>CFBundleDisplayName</key>
    <string>Quick Bridge</string>
    <key>CFBundleIdentifier</key>
    <string>org.quickbridge.desktop</string>
    <key>CFBundleExecutable</key>
    <string>QuickBridge</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon.icns</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleVersion</key>
    <string>$version</string>
    <key>LSMinimumSystemVersion</key>
    <string>$minimum_macos</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

codesign --force --sign - "$staged_bundle"
codesign --verify --strict "$staged_bundle"
if [ -e "$bundle" ]; then
    mv "$bundle" "$previous_bundle"
fi
mv "$staged_bundle" "$bundle"
printf '%s\n' "$bundle"
