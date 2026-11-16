#!/usr/bin/env bash
# Produces the distributables from a (signed, stapled) bundle: dist/Siple-<version>-macos.zip and
# dist/Siple-<version>-macos.dmg. The dmg is signed and notarized when the signing environment is
# configured (see sign-macos.sh / notarize-macos.sh), otherwise left unsigned.
#
#   scripts/dist-macos.sh dist/Siple.app <version>
set -euo pipefail

APP="${1:?usage: dist-macos.sh <Siple.app> <version>}"
VERSION="${2:?usage: dist-macos.sh <Siple.app> <version>}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$(cd "$(dirname "$APP")" && pwd)"
ZIP="$DIST/Siple-$VERSION-macos.zip"
DMG="$DIST/Siple-$VERSION-macos.dmg"

rm -f "$ZIP" "$DMG"
ditto -c -k --norsrc --keepParent "$APP" "$ZIP"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
ditto --norsrc "$APP" "$stage/Siple.app"
ln -s /Applications "$stage/Applications"
hdiutil create -quiet -volname "Siple" -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$DMG"

"$ROOT/scripts/sign-macos.sh" "$DMG"
"$ROOT/scripts/notarize-macos.sh" "$DMG"

echo "zip:  $ZIP"
echo "dmg:  $DMG"
