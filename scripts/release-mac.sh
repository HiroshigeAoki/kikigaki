#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT/crates/kikigaki"

VERSION=$(awk -F '"' '
  /^\[workspace\.package\]$/ { in_workspace = 1; next }
  /^\[/ { in_workspace = 0 }
  in_workspace && /^version = "/ { print $2; exit }
' "$ROOT/Cargo.toml")
if [[ -z "$VERSION" ]]; then
  echo "release-mac: could not read [workspace.package] version from $ROOT/Cargo.toml" >&2
  exit 1
fi
TAURI_VERSION=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "$ROOT/crates/kikigaki/tauri.conf.json")
if [[ "$TAURI_VERSION" != "$VERSION" ]]; then
  echo "release-mac: tauri.conf.json version ($TAURI_VERSION) != Cargo.toml version ($VERSION)" >&2
  exit 1
fi

IDENTITY_NAME="kikigaki"
BUNDLE_ID="dev.aoki.kikigaki"
PRODUCT_NAME="kikigaki"
KEYCHAIN="$HOME/Library/Keychains/kikigaki.keychain-db"

"$ROOT/scripts/fetch-onnxruntime.sh"

if [[ -n "${KIKIGAKI_KEYCHAIN_PASSWORD:-}" ]]; then
  security unlock-keychain -p "$KIKIGAKI_KEYCHAIN_PASSWORD" "$KEYCHAIN"
elif [[ -f "$KEYCHAIN" ]]; then
  echo "release-mac: KIKIGAKI_KEYCHAIN_PASSWORD is unset; trying the existing keychain session" >&2
  echo "release-mac: set it to unlock $KEYCHAIN non-interactively" >&2
fi

# Resolve exactly one matching certificate rather than substring-matching the keychain listing.
IDENTITY_LINE=$(security find-identity -v -p codesigning | grep -F "\"$IDENTITY_NAME\"" || true)
if [[ -z "$IDENTITY_LINE" ]]; then
  echo "release-mac: signing identity '$IDENTITY_NAME' not found in keychain (create it once via Keychain Access)" >&2
  exit 1
fi
if [[ $(printf '%s\n' "$IDENTITY_LINE" | wc -l | tr -d ' ') -ne 1 ]]; then
  echo "release-mac: signing identity '$IDENTITY_NAME' matched more than one keychain entry; disambiguate" >&2
  exit 1
fi
IDENTITY_SHA1=$(printf '%s\n' "$IDENTITY_LINE" | awk '{print $2}')
SIGN_CFG="{\"bundle\":{\"macOS\":{\"signingIdentity\":\"$IDENTITY_SHA1\"}}}"

if ! cargo tauri --version 2>/dev/null | grep -qx 'tauri-cli 2.11.4'; then
  cargo install tauri-cli --version =2.11.4 --locked
fi

# `--bundles app` only: tauri's DMG bundler (a create-dmg fork) drives Finder through AppleScript to
# lay out the volume window, which times out from a non-GUI session (ssh: "AppleEvent timed out
# (-1712)"). The DMG is built below with hdiutil instead, which needs no GUI and is deterministic.
cargo tauri build --bundles app --config "$SIGN_CFG" -- --locked --no-default-features --features punct

TARGET_DIR=$(cargo metadata --format-version=1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
APP_DIR="$TARGET_DIR/release/bundle/macos"
DMG_DIR="$TARGET_DIR/release/bundle/dmg"
APP="$APP_DIR/${PRODUCT_NAME}.app"
FINAL_DMG="$DMG_DIR/kikigaki-${VERSION}-macos-arm64.dmg"

if [[ ! -d "$APP" ]]; then
  echo "release-mac: $APP not found" >&2
  exit 1
fi
if [[ -e "$FINAL_DMG" ]]; then
  echo "release-mac: $FINAL_DMG already exists; refusing to overwrite a prior release artifact" >&2
  exit 1
fi

LIST_DIR=$(mktemp -d)
trap 'rm -rf -- "$LIST_DIR"' EXIT

# Enumerate every regular file and classify it with `file`, not an executable-bit proxy.
if ! find "$APP/Contents" -type f -print0 > "$LIST_DIR/all-files"; then
  echo "release-mac: failed to enumerate files under $APP/Contents" >&2
  exit 1
fi
MACHOS=()
while IFS= read -r -d '' f; do
  kind=$(file -b "$f") || { echo "release-mac: 'file' failed to classify $f" >&2; exit 1; }
  if [[ "$kind" == *"Mach-O"* ]]; then
    MACHOS+=("$f")
  fi
done < "$LIST_DIR/all-files"
if [[ ${#MACHOS[@]} -eq 0 ]]; then
  echo "release-mac: no Mach-O files found under $APP/Contents — classification is broken" >&2
  exit 1
fi
MAIN_EXE="$APP/Contents/MacOS/$PRODUCT_NAME"
ORT="$APP/Contents/Frameworks/libonnxruntime.dylib"
for required in "$MAIN_EXE" "$ORT"; do
  found=0
  for macho in "${MACHOS[@]}"; do
    if [[ "$macho" == "$required" ]]; then
      found=1
      break
    fi
  done
  if [[ $found -ne 1 ]]; then
    echo "release-mac: expected Mach-O $required was not scanned" >&2
    exit 1
  fi
done

echo "== ONNX Runtime hash gate =="
# fetch-onnxruntime.sh verified the staged vendor copy against the pinned upstream sha256. Tauri
# re-signs the copy it places in Contents/Frameworks, so compare the two with signatures removed:
# the bundled dylib must be byte-identical to the verified upstream file apart from its signature.
VENDOR_ORT="$ROOT/crates/kikigaki/vendor/onnxruntime/libonnxruntime.dylib"
VENDOR_SHA256=$(shasum -a 256 "$VENDOR_ORT" | awk '{print $1}')
if [[ "$VENDOR_SHA256" != "18e1f2535084522445927f21ccb4f903b093b47911045750e56b6ce2f2106d79" ]]; then
  echo "release-mac: staged vendor libonnxruntime.dylib sha256 mismatch" >&2
  exit 1
fi
cp "$VENDOR_ORT" "$LIST_DIR/ort-vendor.dylib"
cp "$ORT" "$LIST_DIR/ort-bundled.dylib"
codesign --remove-signature "$LIST_DIR/ort-vendor.dylib"
codesign --remove-signature "$LIST_DIR/ort-bundled.dylib"
if ! cmp -s "$LIST_DIR/ort-vendor.dylib" "$LIST_DIR/ort-bundled.dylib"; then
  echo "release-mac: bundled libonnxruntime.dylib differs from the verified vendor copy beyond its signature" >&2
  exit 1
fi

echo "== GPL symbol gate =="
for macho in "${MACHOS[@]}"; do
  nm_out=$(nm "$macho" 2>&1) || { echo "release-mac: nm failed on $macho: $nm_out" >&2; exit 1; }
  strings_out=$(strings "$macho" 2>&1) || { echo "release-mac: strings failed on $macho: $strings_out" >&2; exit 1; }
  if grep -q espeak_ <<<"$nm_out" || grep -q espeak_ <<<"$strings_out"; then
    echo "release-mac: espeak_ symbol/string found in $macho" >&2
    exit 1
  fi
  if grep -q espeak-ng-data <<<"$strings_out"; then
    echo "release-mac: espeak-ng-data string found in $macho" >&2
    exit 1
  fi
done

echo "== Dynamic-library allowlist gate =="
for macho in "${MACHOS[@]}"; do
  kind=$(file -b "$macho") || { echo "release-mac: 'file' failed to classify $macho" >&2; exit 1; }
  skip_own_install_name=0
  if [[ "$kind" == *"dynamically linked shared library"* ]]; then
    skip_own_install_name=1
  fi
  otool_out=$(otool -L "$macho" 2>&1) || { echo "release-mac: otool -L failed on $macho: $otool_out" >&2; exit 1; }
  while IFS= read -r line; do
    lib=$(awk '{print $1}' <<<"$line")
    if [[ -z "$lib" || "$lib" == "$macho:" ]]; then
      continue
    fi
    if [[ $skip_own_install_name -eq 1 ]]; then
      skip_own_install_name=0
      continue
    fi
    case "$lib" in
      /System/Library/* | /usr/lib/* | @rpath/libonnxruntime.* | @executable_path/../Frameworks/libonnxruntime.* | @loader_path/*libonnxruntime.*)
        ;;
      *)
        echo "release-mac: unexpected linked library in $macho: $line" >&2
        exit 1
        ;;
    esac
  done <<<"$otool_out"
done

echo "== No executable files under models/ =="
if [[ -d "$APP/Contents/Resources" ]]; then
  if ! find "$APP/Contents/Resources" -path '*/models/*' -type f -print0 > "$LIST_DIR/model-files"; then
    echo "release-mac: failed to enumerate bundled model files" >&2
    exit 1
  fi
  while IFS= read -r -d '' f; do
    kind=$(file -b "$f") || { echo "release-mac: 'file' failed to classify $f" >&2; exit 1; }
    if [[ "$kind" == *"Mach-O"* ]]; then
      echo "release-mac: executable Mach-O found under a models directory: $f" >&2
      exit 1
    fi
  done < "$LIST_DIR/model-files"
fi

echo "== macOS version gates =="
plist_min=$(/usr/libexec/PlistBuddy -c 'Print :LSMinimumSystemVersion' "$APP/Contents/Info.plist")
if [[ "$plist_min" != "13.0" ]]; then
  echo "release-mac: LSMinimumSystemVersion is $plist_min, expected 13.0" >&2
  exit 1
fi
otool_load_commands=$(otool -l "$MAIN_EXE" 2>&1) || { echo "release-mac: otool -l failed: $otool_load_commands" >&2; exit 1; }
if ! grep -A3 LC_BUILD_VERSION <<<"$otool_load_commands" | grep -q 'minos 13.0'; then
  echo "release-mac: main executable's LC_BUILD_VERSION is not minos 13.0" >&2
  exit 1
fi

echo "== Bundle metadata gates =="
plist_bundle_id=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$APP/Contents/Info.plist")
if [[ "$plist_bundle_id" != "$BUNDLE_ID" ]]; then
  echo "release-mac: CFBundleIdentifier is $plist_bundle_id, expected $BUNDLE_ID" >&2
  exit 1
fi
plist_ui_element=$(/usr/libexec/PlistBuddy -c 'Print :LSUIElement' "$APP/Contents/Info.plist")
if [[ "$plist_ui_element" != "true" ]]; then
  echo "release-mac: LSUIElement is not true" >&2
  exit 1
fi
mic_usage=$(/usr/libexec/PlistBuddy -c 'Print :NSMicrophoneUsageDescription' "$APP/Contents/Info.plist")
if [[ -z "$mic_usage" ]]; then
  echo "release-mac: NSMicrophoneUsageDescription is empty" >&2
  exit 1
fi
plist_short_version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")
if [[ "$plist_short_version" != "$VERSION" ]]; then
  echo "release-mac: CFBundleShortVersionString ($plist_short_version) != Cargo.toml version ($VERSION)" >&2
  exit 1
fi

echo "== Signing gates =="
codesign --verify --deep --strict "$APP"
requirement=$(codesign -d -r- "$APP" 2>&1) || { echo "release-mac: could not read designated requirement: $requirement" >&2; exit 1; }
if ! grep -qF "certificate leaf" <<<"$requirement"; then
  echo "release-mac: designated requirement doesn't pin a specific certificate leaf (looks ad-hoc)" >&2
  exit 1
fi
codesign_details=$(codesign -dvv "$APP" 2>&1) || { echo "release-mac: could not inspect signature: $codesign_details" >&2; exit 1; }
if ! grep -q '^Authority=kikigaki$' <<<"$codesign_details"; then
  echo "release-mac: signature Authority is not exactly $IDENTITY_NAME" >&2
  exit 1
fi
# `codesign -d -vvv` reports the flags on the CodeDirectory line: `... flags=0x10000(runtime) ...`
codesign_flags=$(codesign -d -vvv "$APP" 2>&1 | grep -oE 'flags=0x[0-9a-f]+\([^)]*\)' | head -n 1 || true)
if ! grep -q 'runtime' <<<"$codesign_flags"; then
  echo "release-mac: hardened runtime flag missing ($codesign_flags)" >&2
  exit 1
fi
# `--xml` (macOS 13+) prints a plain XML plist; older releases only know the deprecated `:-` form.
entitlements_xml=$(codesign -d --entitlements - --xml "$APP" 2>/dev/null) \
  || entitlements_xml=$(codesign -d --entitlements :- "$APP" 2>&1) \
  || { echo "release-mac: could not read entitlements: $entitlements_xml" >&2; exit 1; }
python3 - "$entitlements_xml" <<'PY' || { echo "release-mac: audio-input and disable-library-validation entitlements must both be literal true" >&2; exit 1; }
import plistlib
import sys

raw = sys.argv[1]
start = raw.find("<?xml")
if start < 0:
    start = raw.find("<plist")
end = raw.rfind("</plist>")
if start < 0 or end < 0:
    raise SystemExit(1)
doc = plistlib.loads(raw[start : end + len("</plist>")].encode())
required = (
    "com.apple.security.device.audio-input",
    "com.apple.security.cs.disable-library-validation",
)
raise SystemExit(0 if all(doc.get(key) is True for key in required) else 1)
PY

echo "== NOTICE/license bundling gate =="
RESOURCES="$APP/Contents/Resources"
for required in \
  "$RESOURCES/NOTICE" \
  "$RESOURCES/licenses/sherpa-onnx.LICENSE" \
  "$RESOURCES/licenses/onnxruntime.LICENSE" \
  "$RESOURCES/licenses/silero-vad.LICENSE" \
  "$RESOURCES/licenses/hayamimi.LICENSE" \
  "$RESOURCES/licenses/README"; do
  if [[ ! -s "$required" ]]; then
    echo "release-mac: required notice/license file missing or empty: $required" >&2
    exit 1
  fi
done

echo "== DMG =="
mkdir -p "$DMG_DIR"
STAGE="$LIST_DIR/dmg-root"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -quiet -volname "$PRODUCT_NAME" -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov "$FINAL_DMG"
hdiutil verify -quiet "$FINAL_DMG"
# The copy inside the image must still carry the exact signature that passed the gates above.
MOUNT="$LIST_DIR/mnt"
mkdir -p "$MOUNT"
hdiutil attach -quiet -nobrowse -readonly -mountpoint "$MOUNT" "$FINAL_DMG"
if [[ ! -d "$MOUNT/${PRODUCT_NAME}.app" ]]; then
  echo "release-mac: ${PRODUCT_NAME}.app is missing inside $FINAL_DMG" >&2
  exit 1
fi
codesign --verify --deep --strict "$MOUNT/${PRODUCT_NAME}.app"
hdiutil detach -quiet "$MOUNT"
shasum -a 256 "$FINAL_DMG" | tee "$FINAL_DMG.sha256"
echo "release-mac: $FINAL_DMG ready"
