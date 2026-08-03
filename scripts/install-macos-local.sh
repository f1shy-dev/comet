#!/usr/bin/env bash
# Build and install the daily-use macOS app while keeping the background engine
# rooted in this Cargo checkout.
#
# Default result:
#   /Applications/Zeron.app                  packaged headed UI
#   target/release/comet headless            launchd-managed IPC engine
#
# Usage: scripts/install-macos-local.sh [--no-daemon] [--no-open]
# Env:   COMET_APP_INSTALL_DIR=/path         override /Applications

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_DIR="${COMET_APP_INSTALL_DIR:-/Applications}"
INSTALL_DAEMON=1
OPEN_APP=1

for arg in "$@"; do
  case "$arg" in
    --no-daemon) INSTALL_DAEMON=0 ;;
    --no-open) OPEN_APP=0 ;;
    -h|--help)
      sed -n '2,11p' "$0"
      exit 0
      ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

"$ROOT/scripts/build-macos-app.sh"

BUILT_APP="$ROOT/target/package/Zeron.app"
INSTALLED_APP="$INSTALL_DIR/Zeron.app"
LEGACY_APP="$INSTALL_DIR/Comet.app"
BACKUP_DIR="$ROOT/target/package/installed-backups"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
BACKUP_APP="$BACKUP_DIR/Zeron-$STAMP.app"
ORIGINAL_APP="$INSTALLED_APP"

mkdir -p "$INSTALL_DIR" "$BACKUP_DIR"

# Stop the headed process before replacing its bundle. A missing/running-under-
# another-name app is fine; the command is deliberately best-effort.
osascript -e 'tell application id "dev.comet.native" to quit' >/dev/null 2>&1 || true

# AppleScript returns as soon as the quit request is delivered. Wait for the
# process to actually leave so its embedded engine has released the IPC port
# before launchd starts the checkout-backed headless engine below.
APP_EXECUTABLE="$INSTALLED_APP/Contents/MacOS/comet"
for ((attempt = 0; attempt < 100; attempt++)); do
  if ! pgrep -f "$APP_EXECUTABLE" >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done
if pgrep -f "$APP_EXECUTABLE" >/dev/null 2>&1; then
  echo "Comet did not quit within 10 seconds; leaving the installed app untouched." >&2
  exit 1
fi

if [[ -e "$INSTALLED_APP" ]]; then
  mv "$INSTALLED_APP" "$BACKUP_APP"
  echo "backed up: $BACKUP_APP"
elif [[ -e "$LEGACY_APP" ]]; then
  BACKUP_APP="$BACKUP_DIR/Comet-$STAMP.app"
  ORIGINAL_APP="$LEGACY_APP"
  mv "$LEGACY_APP" "$BACKUP_APP"
  echo "backed up legacy app: $BACKUP_APP"
fi

if ! ditto "$BUILT_APP" "$INSTALLED_APP"; then
  if [[ -e "$BACKUP_APP" && ! -e "$INSTALLED_APP" ]]; then
    mv "$BACKUP_APP" "$ORIGINAL_APP"
  fi
  exit 1
fi

if [[ "$INSTALL_DAEMON" == 1 ]]; then
  # Invoking daemon install through the checkout binary makes launchd's
  # ProgramArguments point at target/release/comet, not the app-bundled copy.
  "$ROOT/target/release/comet" daemon install
fi

if [[ "$OPEN_APP" == 1 ]]; then
  open "$INSTALLED_APP"
fi

echo "installed: $INSTALLED_APP"
if [[ "$INSTALL_DAEMON" == 1 ]]; then
  echo "ipc engine: $ROOT/target/release/comet headless"
fi
