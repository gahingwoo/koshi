#!/usr/bin/env bash
# Build a standalone Koshi.app for macOS: compiles the release binary, then
# bundles every Homebrew dylib and GTK runtime resource (icon theme,
# gdk-pixbuf loaders, the GIO TLS module, GSettings schemas) it needs so the
# result runs on a Mac that never had Homebrew's GTK stack installed. See
# otool -L on the output binary/Frameworks - none of it should reference
# /opt/homebrew once this script finishes.
#
# Requires (via `brew install rust libsoup@3 dylibbundler librsvg`):
# cargo, pkg-config-visible gtk4/libadwaita/libsoup3, dylibbundler,
# rsvg-convert. Run from anywhere; paths below are relative to the repo root.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

BREW_PREFIX="$(brew --prefix)"
APP="target/macos/Koshi.app"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"

echo "==> Building release binary"
cargo build -r

echo "==> Generating app icon"
ICONSET="target/macos/icon.iconset"
rm -rf "$ICONSET" && mkdir -p "$ICONSET"
SRC_SVG="data/icons/scalable/apps/moe.nikableh.Koshi.svg"
for sz in 16 32 64 128 256 512 1024; do
  rsvg-convert -w "$sz" -h "$sz" "$SRC_SVG" -o "$ICONSET/tmp_${sz}.png"
done
cp "$ICONSET/tmp_16.png"   "$ICONSET/icon_16x16.png"
cp "$ICONSET/tmp_32.png"   "$ICONSET/icon_16x16@2x.png"
cp "$ICONSET/tmp_32.png"   "$ICONSET/icon_32x32.png"
cp "$ICONSET/tmp_64.png"   "$ICONSET/icon_32x32@2x.png"
cp "$ICONSET/tmp_128.png"  "$ICONSET/icon_128x128.png"
cp "$ICONSET/tmp_256.png"  "$ICONSET/icon_128x128@2x.png"
cp "$ICONSET/tmp_256.png"  "$ICONSET/icon_256x256.png"
cp "$ICONSET/tmp_512.png"  "$ICONSET/icon_256x256@2x.png"
cp "$ICONSET/tmp_512.png"  "$ICONSET/icon_512x512.png"
cp "$ICONSET/tmp_1024.png" "$ICONSET/icon_512x512@2x.png"
rm "$ICONSET"/tmp_*.png
iconutil -c icns "$ICONSET" -o target/macos/koshi.icns

echo "==> Creating bundle skeleton"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$APP/Contents/Frameworks"
cp target/macos/koshi.icns "$APP/Contents/Resources/koshi.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>Koshi</string>
	<key>CFBundleDisplayName</key>
	<string>Koshi</string>
	<key>CFBundleIdentifier</key>
	<string>moe.nikableh.Koshi</string>
	<key>CFBundleVersion</key>
	<string>$VERSION</string>
	<key>CFBundleShortVersionString</key>
	<string>$VERSION</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleExecutable</key>
	<string>koshi</string>
	<key>CFBundleIconFile</key>
	<string>koshi.icns</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.productivity</string>
</dict>
</plist>
PLIST

echo "==> Copying binary and dlopen'd modules"
cp target/release/koshi "$APP/Contents/MacOS/koshi-bin"
chmod +x "$APP/Contents/MacOS/koshi-bin"

mkdir -p "$APP/Contents/Resources/lib/gdk-pixbuf-2.0/2.10.0/loaders"
cp "$BREW_PREFIX/lib/gdk-pixbuf-2.0/2.10.0/loaders/"*.so \
  "$APP/Contents/Resources/lib/gdk-pixbuf-2.0/2.10.0/loaders/"
mkdir -p "$APP/Contents/Resources/lib/gio/modules"
cp "$BREW_PREFIX/lib/gio/modules/libgiognutls.so" "$APP/Contents/Resources/lib/gio/modules/"
cp "$BREW_PREFIX/opt/gdk-pixbuf/bin/gdk-pixbuf-query-loaders" "$APP/Contents/MacOS/gdk-pixbuf-query-loaders"
chmod +w "$APP/Contents/Resources/lib/gdk-pixbuf-2.0/2.10.0/loaders/"*.so \
  "$APP/Contents/Resources/lib/gio/modules/"*.so \
  "$APP/Contents/MacOS/gdk-pixbuf-query-loaders"

echo "==> Bundling dylibs (dylibbundler)"
# Every Mach-O we're shipping must go through ONE dylibbundler invocation
# with -od: it wipes the destination dir on each run, so a second pass would
# discard whatever the first pass copied for files not in that pass's -x list.
XARGS=(-x "$APP/Contents/MacOS/koshi-bin" -x "$APP/Contents/MacOS/gdk-pixbuf-query-loaders")
for f in "$APP"/Contents/Resources/lib/gdk-pixbuf-2.0/2.10.0/loaders/*.so \
         "$APP"/Contents/Resources/lib/gio/modules/*.so; do
  XARGS+=(-x "$f")
done
dylibbundler -od -b "${XARGS[@]}" \
  -d "$APP/Contents/Frameworks" \
  -p @executable_path/../Frameworks/ \
  -s "$BREW_PREFIX/lib" \
  -s "$BREW_PREFIX/lib/gdk-pixbuf-2.0/2.10.0/loaders" \
  < /dev/null

echo "==> De-duplicating LC_RPATH entries dylibbundler can leave behind"
# dylibbundler occasionally adds the same @executable_path/../Frameworks/
# rpath to a file twice (seen on the librsvg gdk-pixbuf loader); dyld refuses
# to load a Mach-O with a duplicate LC_RPATH, so strip repeats down to one.
find "$APP" -type f | while read -r f; do
  file "$f" 2>/dev/null | grep -q "Mach-O" || continue
  dupes=$(otool -l "$f" 2>/dev/null | grep -A2 "cmd LC_RPATH" | grep "path " | sed -E 's/^ *path (.*) \(offset.*/\1/' | sort | uniq -d || true)
  [ -z "$dupes" ] && continue
  while IFS= read -r rpath; do
    chmod +w "$f"
    install_name_tool -delete_rpath "$rpath" "$f"
    codesign --force --sign - "$f" 2>/dev/null
  done <<< "$dupes"
done

echo "==> Bundling Adwaita/hicolor icon themes and GSettings schemas"
# Neither gtk4 nor libadwaita depends on adwaita-icon-theme, so a prefix that
# has never had another GTK app built against it (a fresh CI runner, most
# often) won't have it unless it was installed explicitly - fail clearly here
# instead of a bare `cp: No such file or directory`.
if [ ! -d "$BREW_PREFIX/share/icons/Adwaita" ]; then
  echo "error: $BREW_PREFIX/share/icons/Adwaita not found - run:" >&2
  echo "  brew install adwaita-icon-theme" >&2
  exit 1
fi
mkdir -p "$APP/Contents/Resources/share/icons"
cp -RL "$BREW_PREFIX/share/icons/Adwaita" "$APP/Contents/Resources/share/icons/"
cp -RL "$BREW_PREFIX/share/icons/hicolor" "$APP/Contents/Resources/share/icons/"

mkdir -p "$APP/Contents/Resources/share/glib-2.0/schemas"
cp "$BREW_PREFIX/share/glib-2.0/schemas/org.gtk.gtk4.Settings."*.gschema.xml \
  "$APP/Contents/Resources/share/glib-2.0/schemas/"
glib-compile-schemas "$APP/Contents/Resources/share/glib-2.0/schemas/"

echo "==> Writing launcher"
cat > "$APP/Contents/MacOS/koshi" <<'LAUNCHER'
#!/bin/bash
# Points the bundled GTK4/libadwaita stack at the resources shipped
# alongside it instead of Homebrew's /opt/homebrew.
set -e

HERE="$(cd "$(dirname "$0")" && pwd)"
RES="$HERE/../Resources"
CACHE_DIR="$HOME/Library/Caches/moe.nikableh.Koshi"
mkdir -p "$CACHE_DIR"

# gdk-pixbuf's loaders.cache embeds absolute paths, which depend on where
# this .app happens to be installed - regenerate it against the bundled
# loaders every launch (a handful of small files, this is instant).
GDK_PIXBUF_MODULEDIR="$RES/lib/gdk-pixbuf-2.0/2.10.0/loaders" \
  "$HERE/gdk-pixbuf-query-loaders" > "$CACHE_DIR/loaders.cache"

export GDK_PIXBUF_MODULE_FILE="$CACHE_DIR/loaders.cache"
export GIO_EXTRA_MODULES="$RES/lib/gio/modules"
export GSETTINGS_SCHEMA_DIR="$RES/share/glib-2.0/schemas"
export XDG_DATA_DIRS="$RES/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"

exec "$HERE/koshi-bin" "$@"
LAUNCHER
chmod +x "$APP/Contents/MacOS/koshi"

echo "==> Done: $APP ($(du -sh "$APP" | cut -f1))"
