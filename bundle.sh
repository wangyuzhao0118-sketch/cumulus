#!/usr/bin/env bash
# 编译 release 版并打包成 Cumulus.app（有 assets/AppIcon.icns 时一起打进去）
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release
APP="target/Cumulus.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/cumulus "$APP/Contents/MacOS/cumulus"
ICON=""
if [ -f assets/AppIcon.icns ]; then
  cp assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
  ICON="<key>CFBundleIconFile</key><string>AppIcon</string>"
fi
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Cumulus</string>
  <key>CFBundleDisplayName</key><string>Cumulus</string>
  <key>CFBundleIdentifier</key><string>local.cumulus</string>
  <key>CFBundleExecutable</key><string>cumulus</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  $ICON
  <key>NSHighResolutionCapable</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSAppTransportSecurity</key><dict><key>NSAllowsLocalNetworking</key><true/></dict>
</dict></plist>
PLIST
codesign --force --sign - "$APP" >/dev/null 2>&1 || true
echo "已生成：$(pwd)/$APP"
