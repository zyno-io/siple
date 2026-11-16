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
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
PKG="$ROOT/packaging/macos"
DIST="$ROOT/dist"
NAME="Siple"
APP="$DIST/$NAME.app"
TARGETS="${TARGETS:-aarch64-apple-darwin x86_64-apple-darwin}"
VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' Cargo.toml | head -1)"
[ -n "$VERSION" ] || { echo "cannot read version from Cargo.toml" >&2; exit 1; }
echo "==> $NAME $VERSION, targets: $TARGETS"

if [ "${SKIP_BUILD:-0}" != "1" ]; then
  for t in $TARGETS; do
    rustup target list --installed | grep -qx "$t" || { echo "missing target: rustup target add $t" >&2; exit 1; }
    echo "==> cargo build --release --target $t"
    MACOSX_DEPLOYMENT_TARGET=12.0 cargo build --release --locked --target "$t" -p siple -p siple-cli
  done
fi

rm -rf "$APP" "$DIST/siple-cli"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$DIST"

# --- binaries (lipo when more than one target) -------------------------------------------------
for bin in siple siple-cli; do
  inputs=()
  for t in $TARGETS; do inputs+=("$ROOT/target/$t/release/$bin"); done
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

if [ "${BUNDLE_ONLY:-0}" = "1" ]; then
  echo "==> BUNDLE_ONLY=1: unsigned bundle at $APP"
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
