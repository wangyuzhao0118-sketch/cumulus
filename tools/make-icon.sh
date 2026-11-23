#!/usr/bin/env bash
# 用法：tools/make-icon.sh assets/cu.svg
# 生成 assets/AppIcon.icns，之后 ./bundle.sh 会自动打进 App
set -euo pipefail
cd "$(dirname "$0")/.."
SVG="$1"
mkdir -p target/icon
swiftc -O tools/svg2png.swift -o target/icon/svg2png
SET=target/icon/AppIcon.iconset
rm -rf "$SET" && mkdir -p "$SET"
for s in 16 32 128 256 512; do
  target/icon/svg2png "$SVG" "$SET/icon_${s}x${s}.png" $s
  target/icon/svg2png "$SVG" "$SET/icon_${s}x${s}@2x.png" $((s * 2))
done
iconutil -c icns "$SET" -o assets/AppIcon.icns
cp "$SVG" assets/AppIcon.svg
echo "已生成 assets/AppIcon.icns（来自 $SVG）"
