#!/usr/bin/env bash
# Packages the desktop app for release.
# Usage: scripts/package-desktop.sh <linux-x86_64|macos-universal|windows-x86_64> <tag>
# The binary is taken from target/release, or from DESKTOP_BIN_DIR.
# Output files go to dist/, or to DIST_DIR.
# macOS: MACOS_SIGN_IDENTITY signs the .app with that identity (ad-hoc otherwise).
set -euo pipefail

platform="$1"
tag="${2:-dev}"
version="${tag#v}"
root="$(cd "$(dirname "$0")/.." && pwd)"
bin_dir="${DESKTOP_BIN_DIR:-$root/target/release}"
dist="${DIST_DIR:-$root/dist}"
mkdir -p "$dist"

case "$platform" in
  linux-x86_64)
    appdir="$(mktemp -d)/Termoak.AppDir"
    mkdir -p "$appdir/usr/bin" "$appdir/usr/share/icons/hicolor/512x512/apps"
    cp "$bin_dir/termoak-desktop" "$appdir/usr/bin/"
    rsvg-convert -w 512 -h 512 "$root/assets/icon.svg" -o "$appdir/termoak.png"
    cp "$appdir/termoak.png" "$appdir/usr/share/icons/hicolor/512x512/apps/termoak.png"
    cat > "$appdir/termoak.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Termoak
Comment=SSH client with AI
Exec=termoak-desktop
Icon=termoak
Categories=Network;System;TerminalEmulator;
Terminal=false
DESKTOP
    ln -s usr/bin/termoak-desktop "$appdir/AppRun"
    tool="$(mktemp -d)/appimagetool"
    curl -fsSL -o "$tool" https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
    chmod +x "$tool"
    ARCH=x86_64 "$tool" --appimage-extract-and-run "$appdir" "$dist/Termoak-linux-x86_64.AppImage"
    tar czf "$dist/termoak-desktop-$tag-linux-x86_64.tar.gz" -C "$bin_dir" termoak-desktop
    ;;
  macos-universal)
    work="$(mktemp -d)"
    app="$work/Termoak.app"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cp "$bin_dir/termoak-desktop" "$app/Contents/MacOS/"
    if command -v rsvg-convert >/dev/null || brew install librsvg >/dev/null 2>&1; then
      iconset="$work/AppIcon.iconset"
      mkdir -p "$iconset"
      for s in 16 32 64 128 256 512; do
        rsvg-convert -w "$s" -h "$s" "$root/assets/icon.svg" -o "$iconset/icon_${s}x${s}.png"
        rsvg-convert -w $((s * 2)) -h $((s * 2)) "$root/assets/icon.svg" -o "$iconset/icon_${s}x${s}@2x.png"
      done
      iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
    fi
    cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Termoak</string>
  <key>CFBundleDisplayName</key><string>Termoak</string>
  <key>CFBundleIdentifier</key><string>com.termoak.Termoak</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleExecutable</key><string>termoak-desktop</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>CFBundleURLTypes</key>
  <array><dict><key>CFBundleURLSchemes</key><array><string>termoak</string></array></dict></array>
</dict>
</plist>
PLIST
    # MACOS_SIGN_IDENTITY: a code signing identity of this Mac's keychain
    # ("Developer ID Application: ..." or a self-signed certificate). With a
    # stable identity, "Always Allow" on the keychain question survives
    # updates; ad-hoc signatures change with every build (docs/RELEASING.md).
    sign_identity="${MACOS_SIGN_IDENTITY:--}"
    if [ "$sign_identity" = "-" ]; then
      echo "warning: ad-hoc signature; macOS will ask again for the keychain after every update (set MACOS_SIGN_IDENTITY)" >&2
      codesign --force --deep --sign - "$app" || true
    else
      codesign --force --deep --options runtime --sign "$sign_identity" "$app"
      codesign --verify --strict --verbose=2 "$app"
    fi
    tar czf "$dist/Termoak-macos-universal.app.tar.gz" -C "$work" Termoak.app
    hdiutil create -volname Termoak -srcfolder "$app" -ov -format UDZO "$dist/Termoak-$tag-macos-universal.dmg"
    ;;
  windows-x86_64)
    cp "$bin_dir/termoak-desktop.exe" "$dist/Termoak-windows-x86_64.exe"
    zip_file="$dist/termoak-desktop-$tag-windows-x86_64.zip"
    if command -v 7z >/dev/null; then
      (cd "$bin_dir" && 7z a "$zip_file" termoak-desktop.exe)
    else
      (cd "$bin_dir" && zip -q "$zip_file" termoak-desktop.exe)
    fi
    ;;
  *)
    echo "unknown platform: $platform" >&2
    exit 1
    ;;
esac
ls -la "$dist"
