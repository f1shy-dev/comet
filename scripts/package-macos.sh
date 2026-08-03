#!/usr/bin/env bash
# macOS packaging: build the release binary for the host arch and produce
#   target/package/comet-<version>-macos-<arch>.dmg          (user download)
#   target/package/comet-<version>-macos-<arch>-app.tar.gz   (auto-updater)
# containing Zeron.app (unsigned unless CODESIGN_IDENTITY is set).
#
# Usage: scripts/package-macos.sh
# Env:   CODESIGN_IDENTITY="Developer ID Application: …" to sign the bundle.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
ARCH="$(uname -m)" # arm64 on Apple silicon runners
OUT_DIR="$ROOT/target/package"
APP="$OUT_DIR/Zeron.app"
DMG="$OUT_DIR/comet-$VERSION-macos-$ARCH.dmg"
APP_TARBALL="$OUT_DIR/comet-$VERSION-macos-$ARCH-app.tar.gz"

"$ROOT/scripts/build-macos-app.sh"
find "$DMG" -depth -delete 2>/dev/null || true
find "$APP_TARBALL" -depth -delete 2>/dev/null || true

# The auto-updater artifact: keep the historical Comet.app path inside the
# tarball so already-installed Comet builds can consume the first Zeron
# release. The DMG still presents Zeron.app to new installs.
tar -czf "$APP_TARBALL" -s '/^Zeron\.app/Comet.app/' -C "$OUT_DIR" Zeron.app
echo "packaged: $APP_TARBALL"

hdiutil create -volname Zeron -srcfolder "$APP" -ov -format UDZO "$DMG"
echo "packaged: $DMG"
