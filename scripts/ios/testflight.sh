#!/bin/bash
# Archive the Godspeed iPhone app (Release) and upload it to App Store Connect
# for TestFlight. Docs: docs/godspeed/TESTFLIGHT.md.
#
#   scripts/ios/testflight.sh               # build core, archive, upload
#   scripts/ios/testflight.sh --no-upload   # archive + export the .ipa locally only
#
# - Build number: a UTC timestamp, YYYYMMDD.HHMM (always increasing; override
#   with BUILD_NUMBER=…). MARKETING_VERSION stays as in the project (1.0).
# - Signing: the archive uses automatic (development) signing with
#   -allowProvisioningUpdates and the App Store Connect API key; the export
#   re-signs manually with the keychain's Apple Distribution certificate and
#   App Store profiles that `asc-setup.sh profiles` (re)creates first (the
#   key isn't allowed cloud-managed distribution signing). The key comes from
#   $GODSPEED_ASC_ENV (default ~/.config/outbound/keys.env: ASC_KEY_ID,
#   ASC_ISSUER_ID, ASC_KEY_PATH) and is never printed.
# - Heavy steps (cargo, xcodebuild) go through
#   ~/.local/share/godspeed-ops/heavy.sh when it exists (one at a time on a
#   16 GB Mac).
# - Output: $GODSPEED_TF_OUT (default /Volumes/Lexar/build/godspeed-testflight
#   if that drive is mounted, else <repo>/target/testflight): the .xcarchive,
#   the export and logs.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
UPLOAD=1
for arg in "$@"; do
  case "$arg" in
    --no-upload) UPLOAD=0 ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

ENV_FILE="${GODSPEED_ASC_ENV:-$HOME/.config/outbound/keys.env}"
read_key() { grep -E "^(export )?$1=" "$ENV_FILE" | tail -1 | cut -d= -f2- | sed -e 's/^["'\'']//' -e 's/["'\'']$//'; }
[[ -f "$ENV_FILE" ]] || { echo "error: $ENV_FILE not found (App Store Connect API key)" >&2; exit 1; }
ASC_KEY_ID="$(read_key ASC_KEY_ID)"
ASC_ISSUER_ID="$(read_key ASC_ISSUER_ID)"
ASC_KEY_PATH="$(read_key ASC_KEY_PATH)"
ASC_KEY_PATH="${ASC_KEY_PATH/#\~/$HOME}"
for v in ASC_KEY_ID ASC_ISSUER_ID ASC_KEY_PATH; do
  [[ -n "${!v}" ]] || { echo "error: $v missing in $ENV_FILE" >&2; exit 1; }
done
[[ -f "$ASC_KEY_PATH" ]] || { echo "error: the ASC_KEY_PATH file doesn't exist" >&2; exit 1; }
AUTH=(-allowProvisioningUpdates -authenticationKeyPath "$ASC_KEY_PATH" -authenticationKeyID "$ASC_KEY_ID" -authenticationKeyIssuerID "$ASC_ISSUER_ID")

BUILD_NUMBER="${BUILD_NUMBER:-$(date -u +%Y%m%d.%H%M)}"
if [[ -z "${GODSPEED_TF_OUT:-}" ]]; then
  if [[ -d /Volumes/Lexar/build ]]; then GODSPEED_TF_OUT=/Volumes/Lexar/build/godspeed-testflight; else GODSPEED_TF_OUT="$ROOT/target/testflight"; fi
fi
OUT="$GODSPEED_TF_OUT/$BUILD_NUMBER"
ARCHIVE="$OUT/Godspeed.xcarchive"
DERIVED="$GODSPEED_TF_OUT/DerivedData"
mkdir -p "$OUT"

HEAVY="$HOME/.local/share/godspeed-ops/heavy.sh"
heavy() {
  if [[ -x "$HEAVY" && -z "${GODSPEED_IN_HEAVY:-}" ]]; then GODSPEED_IN_HEAVY=1 "$HEAVY" "$@"; else "$@"; fi
}

echo "==> Godspeed iOS $BUILD_NUMBER -> $OUT"

echo "==> Rust core (iphoneos, mobile-dist)"
heavy env CONFIGURATION=Release "$ROOT/scripts/ios/build-core.sh" iphoneos > "$OUT/core.log" 2>&1 \
  || { tail -30 "$OUT/core.log"; echo "error: Rust core build failed (log: $OUT/core.log)" >&2; exit 1; }

echo "==> xcodebuild archive"
heavy xcodebuild archive \
  -project "$ROOT/apps/ios/Godspeed.xcodeproj" \
  -scheme Godspeed \
  -configuration Release \
  -destination 'generic/platform=iOS' \
  -archivePath "$ARCHIVE" \
  -derivedDataPath "$DERIVED" \
  "${AUTH[@]}" \
  CURRENT_PROJECT_VERSION="$BUILD_NUMBER" \
  GODSPEED_SKIP_CORE=1 \
  > "$OUT/archive.log" 2>&1 \
  || { grep -E "error:|BUILD FAILED|ARCHIVE FAILED" "$OUT/archive.log" | head -30; echo "error: archive failed (log: $OUT/archive.log)" >&2; exit 1; }

PLIST="$ARCHIVE/Products/Applications/Godspeed.app/Info.plist"
echo "    $(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$PLIST") $(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$PLIST") ($(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$PLIST"))"

echo "==> App Store provisioning profiles"
"$ROOT/scripts/ios/asc-setup.sh" profiles > "$OUT/profiles.log" 2>&1 \
  || { cat "$OUT/profiles.log"; echo "error: couldn't create the App Store profiles" >&2; exit 1; }

OPTIONS="$OUT/ExportOptions.plist"
cp "$ROOT/scripts/ios/ExportOptions.plist" "$OPTIONS"
if [[ "$UPLOAD" == 0 ]]; then
  plutil -replace destination -string export "$OPTIONS"
  echo "==> export .ipa (no upload)"
else
  echo "==> export + upload to App Store Connect"
fi
heavy xcodebuild -exportArchive \
  -archivePath "$ARCHIVE" \
  -exportOptionsPlist "$OPTIONS" \
  -exportPath "$OUT/export" \
  "${AUTH[@]}" \
  > "$OUT/export.log" 2>&1 \
  || { grep -iE "error|fail" "$OUT/export.log" | head -30; echo "error: export failed (log: $OUT/export.log)" >&2; exit 1; }

if [[ "$UPLOAD" == 0 ]]; then
  echo "==> done: $(ls "$OUT"/export/*.ipa)"
else
  echo "==> uploaded build $BUILD_NUMBER. Processing takes 5-30 minutes; then:"
  echo "    scripts/ios/asc-setup.sh submit --build $BUILD_NUMBER"
fi
