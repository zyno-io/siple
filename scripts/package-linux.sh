#!/usr/bin/env bash
# Builds dist/Siple-<version>-linux-<arch>.tar.gz (<arch>: x86_64 or aarch64, that of the machine: native builds only). Run it inside rust:1.100-bookworm (glibc 2.36, so the
# tarball runs on Debian 12+ / Ubuntu 23.04+), the same way CI does (.github/workflows/release.yml).
# scripts/package-appimage.sh then builds the AppImage from this tarball.
#
#   scripts/package-linux.sh
#
# Environment:
#   SKIP_BUILD=1   reuse the existing target/release binaries.
#   SIPLE_RELEASE=1  release build: fail if cargo-about is missing (THIRD-PARTY-NOTICES.html).
# The tray is ksni (StatusNotifierItem over zbus) and notifications are notify-rust (zbus): both
# are pure Rust, so they add no NEEDED libraries (no GTK / libappindicator / libdbus).
# Fails if a binary's DT_NEEDED list contains anything outside the allowlist (linux-needed-check.sh).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# shellcheck source=scripts/linux-arch.sh
. "$ROOT/scripts/linux-arch.sh"
DIST="$ROOT/dist"
VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' Cargo.toml | head -1)"
[ -n "$VERSION" ] || { echo "cannot read version from Cargo.toml" >&2; exit 1; }
NAME="siple-$VERSION"
echo "==> Siple $VERSION (linux $LINUX_ARCH)"

if [ "${SKIP_BUILD:-0}" != "1" ]; then
  cargo build --release --locked -p siple -p siple-cli
fi

STAGE="$DIST/stage/$NAME"
rm -rf "$DIST/stage"
mkdir -p "$STAGE/bin" "$STAGE/share/applications" "$STAGE/share/pixmaps" "$STAGE/share/metainfo" "$STAGE/share/doc/siple"
for bin in siple siple-cli; do
  install -m 755 -s "target/release/$bin" "$STAGE/bin/$bin"
done
APPID=io.zyno.siple
install -m 644 "packaging/linux/$APPID.desktop" "$STAGE/share/applications/$APPID.desktop"
install -m 644 "packaging/linux/$APPID.metainfo.xml" "$STAGE/share/metainfo/$APPID.metainfo.xml"
install -m 644 packaging/icons/siple-1024.png "$STAGE/share/pixmaps/$APPID.png"
for d in README.md docs/CONFIG.md docs/API.md; do
  install -m 644 "$d" "$STAGE/share/doc/siple/$(basename "$d")"
done
install -m 644 LICENSE "$STAGE/LICENSE"
# Third-party notices: skipped with a warning in dev builds, fatal when SIPLE_RELEASE=1.
scripts/gen-licenses.sh --if-available "$STAGE/THIRD-PARTY-NOTICES.html"

# --- dependency guard -----------------------------------------------------------------------------
scripts/linux-needed-check.sh "$STAGE"/bin/*

OUT="$DIST/Siple-$VERSION-linux-$LINUX_ARCH.tar.gz"
tar -C "$DIST/stage" --owner=0 --group=0 -czf "$OUT" "$NAME"
rm -rf "$DIST/stage"
echo "==> $OUT ($(du -h "$OUT" | cut -f1))"
