#!/usr/bin/env bash
# Notarizes a signed .app or .dmg with Apple (rcodesign notary-submit --wait) and staples the
# ticket so Gatekeeper accepts it offline.
#
#   scripts/notarize-macos.sh dist/Siple.app
#   scripts/notarize-macos.sh dist/Siple-<version>-macos.dmg
#
# Environment:
#   APP_STORE_CONNECT_API_KEY_PATH  JSON from `rcodesign encode-app-store-connect-api-key`.
#   REQUIRE_SIGNING=1               fail instead of skipping when the key is not configured.
#
# Without the key this prints a notice and exits 0, so dev builds keep working.
set -euo pipefail

TARGET="${1:?usage: notarize-macos.sh <Siple.app|file.dmg>}"

if [[ -z "${APP_STORE_CONNECT_API_KEY_PATH:-}" ]]; then
  if [[ "${REQUIRE_SIGNING:-0}" == "1" ]]; then
    echo "error: APP_STORE_CONNECT_API_KEY_PATH not set and REQUIRE_SIGNING=1" >&2
    exit 1
  fi
  echo "Skipping notarization: APP_STORE_CONNECT_API_KEY_PATH not set (dev build)"
  exit 0
fi
[[ -e "$TARGET" ]] || { echo "error: $TARGET not found" >&2; exit 1; }

case "$TARGET" in
  *.app)
    # Apple takes a zip (or dmg/pkg), not a bare bundle.
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    ditto -c -k --norsrc --keepParent "$TARGET" "$tmp/notarize.zip"
    submit="$tmp/notarize.zip"
    ;;
  *.dmg) submit="$TARGET" ;;
  *)
    echo "error: unsupported target (expected .app or .dmg): $TARGET" >&2
    exit 1
    ;;
esac

echo "==> notarizing $TARGET"
rcodesign notary-submit --api-key-file "$APP_STORE_CONNECT_API_KEY_PATH" --wait --max-wait-seconds 3600 "$submit"
echo "==> stapling $TARGET"
rcodesign staple "$TARGET"
echo "Notarized and stapled: $TARGET"
