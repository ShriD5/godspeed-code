#!/usr/bin/env bash
# Build and run an isolated macOS development bundle. Godspeed.app may remain
# open: this bundle has a separate LaunchServices/TCC identity, data directory,
# and engine IPC port.
# Set GODSPEED_DEV_BUILD_ONLY=1 to prepare the signed bundle without launching it.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
DEV_ROOT="$ROOT/target/macos-dev"
APP="$DEV_ROOT/Godspeed Dev.app"
CONTENTS="$APP/Contents"
DATA_DIR="${GODSPEED_DEV_DATA_DIR:-$DEV_ROOT/data}"
IPC_PORT="${GODSPEED_DEV_IPC_PORT:-49777}"
BUNDLE_ID="${GODSPEED_DEV_BUNDLE_ID:-site.shrithan.godspeed.dev}"

if pgrep -f -x "$CONTENTS/MacOS/godspeed" >/dev/null 2>&1; then
  echo "Godspeed Dev is already running. Quit it before rebuilding the signed bundle." >&2
  exit 1
fi

cd "$ROOT"
cargo build -p godspeed

mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources" "$DATA_DIR"
TARGET_DIR="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
install -m 755 "$TARGET_DIR/debug/godspeed" "$CONTENTS/MacOS/godspeed"
sed "s/__VERSION__/$VERSION/g" "$ROOT/dist/macos/Info-dev.plist" >"$CONTENTS/Info.plist"
plutil -replace CFBundleIdentifier -string "$BUNDLE_ID" "$CONTENTS/Info.plist"
plutil -replace LSEnvironment.GODSPEED_DATA_DIR -string "$DATA_DIR" "$CONTENTS/Info.plist"
plutil -replace LSEnvironment.GODSPEED_IPC_PORT -string "$IPC_PORT" "$CONTENTS/Info.plist"
if [[ -n "${CODEX_EXECUTABLE:-}" ]]; then
  plutil -insert LSEnvironment.CODEX_EXECUTABLE -string "$CODEX_EXECUTABLE" "$CONTENTS/Info.plist"
fi

mkdir -p "$CONTENTS/Resources/licenses"
cp "$ROOT/crates/voice/NOTICE.md" "$CONTENTS/Resources/licenses"/parakeet-v3.txt

if [[ ! -f "$CONTENTS/Resources/godspeed.icns" ]]; then
  ICONSET="$DEV_ROOT/godspeed-dev.iconset"
  mkdir -p "$ICONSET"
  for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$ROOT/dist/macos/icon-1024.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
    retina=$((size * 2))
    sips -z "$retina" "$retina" "$ROOT/dist/macos/icon-1024.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
  done
  iconutil -c icns "$ICONSET" -o "$CONTENTS/Resources/godspeed.icns"
fi

# A real Apple Development identity gives TCC a stable signing requirement
# across rebuilds. Set GODSPEED_DEV_CODESIGN_IDENTITY explicitly when more than
# one identity is installed; otherwise fall back to an ad-hoc signature.
IDENTITY="${GODSPEED_DEV_CODESIGN_IDENTITY:-}"
if [[ -z "$IDENTITY" ]]; then
  IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null | sed -n 's/.*"\(Apple Development:[^"]*\)".*/\1/p' | head -1)"
fi
if [[ -n "$IDENTITY" ]]; then
  codesign --entitlements "$ROOT/dist/macos/Godspeed.entitlements" --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$APP"
else
  codesign --entitlements "$ROOT/dist/macos/Godspeed.entitlements" --force --sign - --identifier "$BUNDLE_ID" "$APP"
  echo "warning: no Apple Development signing identity found; macOS may ask for permissions again after a rebuild" >&2
fi

if [[ "${GODSPEED_DEV_BUILD_ONLY:-0}" == "1" ]]; then
  echo "built: $APP" >&2
  exit 0
fi

echo "running Godspeed Dev (bundle $BUNDLE_ID, data $DATA_DIR, IPC $IPC_PORT)" >&2
# LaunchServices must own the process. Launching Contents/MacOS/godspeed directly
# makes TCC attribute Screen Recording to the terminal (Warp, Terminal, etc.).
# -W keeps the script attached until the app exits. Runtime logs remain in the
# isolated data directory (`target/macos-dev/data/logs/godspeed-headed.log`).
OPEN_ENV=(
  --env "GODSPEED_AUTO_UPDATE=0"
  --env "GODSPEED_DATA_DIR=$DATA_DIR"
  --env "GODSPEED_IPC_PORT=$IPC_PORT"
)
if [[ -n "${CODEX_EXECUTABLE:-}" ]]; then
  OPEN_ENV+=(--env "CODEX_EXECUTABLE=$CODEX_EXECUTABLE")
fi
if [[ -n "${GODSPEED_OPEN_ROUTE:-}" ]]; then
  OPEN_ENV+=(--env "GODSPEED_OPEN_ROUTE=$GODSPEED_OPEN_ROUTE")
fi
exec open -W "${OPEN_ENV[@]}" "$APP" --args "$@"
