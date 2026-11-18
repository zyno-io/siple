#!/usr/bin/env bash
# Builds "dist/Siple.app" (universal: aarch64 + x86_64), dist/Siple-<version>-macos.zip,
# dist/Siple-<version>-macos.dmg and a dist/siple-cli symlink into the bundle.
#
#   scripts/package-macos.sh
#
# Environment:
#   CODESIGN_IDENTITY  signing identity (e.g. "Developer ID Application: ..." or a stable local
#                      certificate). Unset = ad-hoc signature. With an identity the hardened
#                      runtime is enabled. A stable identity keeps macOS microphone (TCC) grants
#                      across rebuilds; an ad-hoc signature changes with every build.
#   TARGETS            space-separated rust targets (default: both Apple targets). A single target
#                      skips lipo (handy for a fast local build: TARGETS=aarch64-apple-darwin).
#   SKIP_BUILD=1       reuse the existing target/ binaries.
#   SIPLE_RELEASE=1    release build: fail if cargo-about is missing (THIRD-PARTY-NOTICES.html).
#   BUNDLE_ONLY=1      stop after assembling the unsigned bundle (no codesign, zip or dmg). The release
#                      workflow builds like this, then signs on a separate job that holds the secrets:
#                      scripts/sign-macos.sh, notarize-macos.sh, dist-macos.sh (see docs/RELEASING.md).
#   MAS=1              Mac App Store build: `--features store`, MACOSX_DEPLOYMENT_TARGET 13.0, store Info.plist
#                      patches, privacy manifest, output dist/mas/Siple.app. Always stops after the unsigned
#                      bundle (BUNDLE_ONLY is implied); sign + package with scripts/sign-macos-mas.sh.
#   MAC_BUILD          MAS only: CFBundleVersion (integer). Default: scripts/store-version.sh (dev build).
#                      CI passes BUNDLE_VERSION from store-version.sh for the tag.
#   CARGO_TARGET_DIR   cargo target dir (default: ./target); the script reads binaries from there.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
PKG="$ROOT/packaging/macos"
DIST="$ROOT/dist"
MAS="${MAS:-0}"
if [ "$MAS" = "1" ]; then DIST="$ROOT/dist/mas"; BUNDLE_ONLY=1; fi
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
NAME="Siple"
APP="$DIST/$NAME.app"
TARGETS="${TARGETS:-aarch64-apple-darwin x86_64-apple-darwin}"
VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' Cargo.toml | head -1)"
[ -n "$VERSION" ] || { echo "cannot read version from Cargo.toml" >&2; exit 1; }
echo "==> $NAME $VERSION, targets: $TARGETS"
if [ "$MAS" = "1" ]; then
  if [ -z "${MAC_BUILD:-}" ]; then
    MAC_BUILD="$("$ROOT/scripts/store-version.sh" "" 0 | sed -n 's/^BUNDLE_VERSION=//p')"
  fi
  [[ "$MAC_BUILD" =~ ^[0-9]+$ ]] || { echo "MAC_BUILD must be an integer, got '$MAC_BUILD'" >&2; exit 1; }
fi

if [ "${SKIP_BUILD:-0}" != "1" ]; then
  for t in $TARGETS; do
    rustup target list --installed | grep -qx "$t" || { echo "missing target: rustup target add $t" >&2; exit 1; }
    echo "==> cargo build --release --target $t"
    if [ "$MAS" = "1" ]; then
      # Store build: no update check; macOS 13 floor (App Sandbox + current App Store minimum).
      # SIPLE_BUNDLE_VERSION: siple-cli's embedded Info.plist gets the same CFBundleVersion as the app.
      MACOSX_DEPLOYMENT_TARGET=13.0 SDKROOT="$(xcrun --sdk macosx --show-sdk-path)" SIPLE_BUNDLE_VERSION="$MAC_BUILD" \
        cargo build --release --locked --target "$t" -p siple -p siple-cli --features store
    else
      MACOSX_DEPLOYMENT_TARGET=12.0 cargo build --release --locked --target "$t" -p siple -p siple-cli
    fi
  done
fi

rm -rf "$APP" "$DIST/siple-cli"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$DIST"

# --- binaries (lipo when more than one target) -------------------------------------------------
for bin in siple siple-cli; do
  inputs=()
  for t in $TARGETS; do inputs+=("$TARGET_DIR/$t/release/$bin"); done
  if [ "${#inputs[@]}" -gt 1 ]; then
    lipo -create "${inputs[@]}" -output "$APP/Contents/MacOS/$bin"
  else
    cp "${inputs[0]}" "$APP/Contents/MacOS/$bin"
  fi
done

# --- licence + third-party notices (fatal when SIPLE_RELEASE=1 and cargo-about is missing) -----
cp "$ROOT/LICENSE" "$APP/Contents/Resources/LICENSE"
"$ROOT/scripts/gen-licenses.sh" --if-available "$APP/Contents/Resources/THIRD-PARTY-NOTICES.html"

# --- Info.plist --------------------------------------------------------------------------------
sed "s/@VERSION@/$VERSION/g" "$PKG/Info.plist.in" > "$APP/Contents/Info.plist"
if [ "$MAS" = "1" ]; then
  PB=/usr/libexec/PlistBuddy
  P="$APP/Contents/Info.plist"
  "$PB" -c "Set :LSMinimumSystemVersion 13.0" "$P"
  "$PB" -c "Set :CFBundleVersion $MAC_BUILD" "$P"
  "$PB" -c "Set :LSApplicationCategoryType public.app-category.business" "$P"
  # Export compliance: false = exempt. Siple implements only standard algorithms (TLS via rustls/ring, SRTP
  # AES/HMAC-SHA1), nothing proprietary, and its source is publicly available. `true` would require an App
  # Store Connect encryption declaration and ITSEncryptionExportComplianceCode, or validation fails (90592).
  "$PB" -c "Add :ITSAppUsesNonExemptEncryption bool false" "$P"
  echo "==> MAS: CFBundleVersion=$MAC_BUILD, LSMinimumSystemVersion=13.0"
fi
plutil -lint "$APP/Contents/Info.plist" >/dev/null

# --- icon: PNG -> .iconset -> .icns ------------------------------------------------------------
ICONSET="$(mktemp -d)/Siple.iconset"
mkdir -p "$ICONSET"
SRC="$ROOT/packaging/icons/siple-1024.png"
for sz in 16 32 128 256 512; do
  sips -z $sz $sz "$SRC" --out "$ICONSET/icon_${sz}x${sz}.png" >/dev/null
  sips -z $((sz * 2)) $((sz * 2)) "$SRC" --out "$ICONSET/icon_${sz}x${sz}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Siple.icns"
rm -rf "$(dirname "$ICONSET")"

if [ "$MAS" = "1" ]; then
  cp "$PKG/PrivacyInfo.xcprivacy" "$APP/Contents/Resources/PrivacyInfo.xcprivacy"
  # Strip quarantine / resource-fork xattrs and normalise modes: the App Store rejects stray attributes.
  find "$APP" -print0 | xargs -0 xattr -c   # not `xattr -cr`: some xattr builds lack -r
  chmod -R u+rwX,go+rX "$APP"
fi

if [ "${BUNDLE_ONLY:-0}" = "1" ]; then
  echo "==> BUNDLE_ONLY (MAS=$MAS): unsigned bundle at $APP"
  exit 0
fi

# --- sign (inside-out: helper binary, main binary, then the bundle) ---------------------------
IDENT="${CODESIGN_IDENTITY:--}"
SIGN=(codesign --force --timestamp=none --entitlements "$PKG/entitlements.plist")
if [ "$IDENT" != "-" ]; then
  SIGN=(codesign --force --timestamp --options runtime --entitlements "$PKG/entitlements.plist")
  echo "==> signing with identity: $IDENT (hardened runtime)"
else
  echo "==> signing ad-hoc (set CODESIGN_IDENTITY for a stable identity)"
fi
"${SIGN[@]}" --sign "$IDENT" --identifier io.zyno.siple-cli "$APP/Contents/MacOS/siple-cli"
"${SIGN[@]}" --sign "$IDENT" "$APP"
codesign --verify --deep --strict "$APP"

# --- zip + dmg + CLI symlink ---------------------------------------------------------------------
# Release signing (Developer ID + notarization) is separate: see scripts/sign-macos.sh. The dmg is
# signed/notarized by dist-macos.sh only when the APPLE_* environment is present.
ZIP="$DIST/$NAME-$VERSION-macos.zip"
"$ROOT/scripts/dist-macos.sh" "$APP" "$VERSION"
ln -s "$NAME.app/Contents/MacOS/siple-cli" "$DIST/siple-cli"

echo
echo "app:  $APP"
echo "zip:  $ZIP"
echo "dmg:  ${ZIP%.zip}.dmg"
echo "cli:  $DIST/siple-cli -> $NAME.app/Contents/MacOS/siple-cli"
echo "      (the symlink only exists in dist/; in the zip, run <$NAME.app>/Contents/MacOS/siple-cli"
echo "       or symlink it onto your PATH: ln -s \"/Applications/$NAME.app/Contents/MacOS/siple-cli\" /usr/local/bin/)"
file "$APP/Contents/MacOS/siple" | sed 's/^/      /'
