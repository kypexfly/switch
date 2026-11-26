#!/bin/sh
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-"$root/target"}
app="$root/dist/Switch.app"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n 1)

cd "$root"
cargo build --release -p switch-desktop -p switch-cli

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$root/crates/desktop/assets/Switch.icns" "$app/Contents/Resources/Switch.icns"
cp "$target_dir/release/switch-desktop" "$app/Contents/MacOS/switch-desktop"
cp "$target_dir/release/swx" "$app/Contents/MacOS/swx"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Switch</string>
    <key>CFBundleDisplayName</key>
    <string>Switch</string>
    <key>CFBundleIdentifier</key>
    <string>dev.switch.desktop</string>
    <key>CFBundleExecutable</key>
    <string>switch-desktop</string>
    <key>CFBundleIconFile</key>
    <string>Switch</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${version:-0.0.0}</string>
    <key>CFBundleVersion</key>
    <string>${version:-0.0.0}</string>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

codesign --force --sign - "$app/Contents/MacOS/swx"
codesign --force --sign - "$app"
echo "Built $app"
