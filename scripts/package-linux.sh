#!/usr/bin/env bash
# Builds dist/Siple-<version>-linux-x86_64.tar.gz. Run it inside rust:1.94-bookworm (glibc 2.36, so the
# tarball runs on Debian 12+ / Ubuntu 23.04+): scripts/linux-box.sh package does exactly that.
#
#   scripts/package-linux.sh
#
# Environment:
#   SKIP_BUILD=1   reuse the existing target/release binaries.
#   SIPLE_RELEASE=1  release build: fail if cargo-about is missing (THIRD-PARTY-NOTICES.html).
# The tray is ksni (StatusNotifierItem over zbus) and notifications are notify-rust (zbus): both
# are pure Rust, so they add no NEEDED libraries (no GTK / libappindicator / libdbus).
# Fails if a binary's DT_NEEDED list contains anything outside the allowlist below.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
DIST="$ROOT/dist"
VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' Cargo.toml | head -1)"
[ -n "$VERSION" ] || { echo "cannot read version from Cargo.toml" >&2; exit 1; }
NAME="siple-$VERSION"
echo "==> Siple $VERSION (linux x86_64)"

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
ALLOWED="libasound.so.2 libc.so.6 libm.so.6 libgcc_s.so.1 ld-linux-x86-64.so.2"
bad=0
maxglibc=""
for bin in "$STAGE"/bin/*; do
  needed="$(objdump -p "$bin" | awk '/NEEDED/ {print $2}')"
  echo "==> $(basename "$bin") NEEDED: $(echo $needed)"
  for n in $needed; do
    case " $ALLOWED " in
      *" $n "*) ;;
      *) echo "ERROR: $(basename "$bin") needs unexpected library $n" >&2; bad=1 ;;
    esac
  done
  g="$(objdump -T "$bin" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)"
  [ -z "$maxglibc" ] || g="$(printf '%s\n%s\n' "$maxglibc" "$g" | sort -V | tail -1)"
  maxglibc="$g"
done
[ "$bad" = 0 ] || { echo "FAILED: NEEDED allowlist check ({$ALLOWED})" >&2; exit 1; }
echo "==> NEEDED allowlist check passed; highest GLIBC_ version used: GLIBC_$maxglibc"

OUT="$DIST/Siple-$VERSION-linux-x86_64.tar.gz"
tar -C "$DIST/stage" --owner=0 --group=0 -czf "$OUT" "$NAME"
rm -rf "$DIST/stage"
echo "==> $OUT ($(du -h "$OUT" | cut -f1))"
