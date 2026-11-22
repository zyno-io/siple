#!/usr/bin/env bash
# Builds dist/Siple-<version>-linux-<arch>.AppImage (<arch>: x86_64 or aarch64) from the Linux tarball of
# the same architecture. Builds are native: run it on the machine that built the tarball.
#
# A type 2 AppImage is one file: a static runtime (it mounts the image with FUSE, or extracts it
# with --appimage-extract-and-run) followed by a squashfs image of an AppDir. That is all this
# script assembles, by hand: a hand-built AppDir, `mksquashfs`, and `cat` onto the pinned runtime.
# There is no appimagetool and no linuxdeploy. The Siple binaries need only glibc, libgcc_s and ALSA from
# the host (scripts/linux-needed-check.sh), but winit, wgpu and egui dlopen the X11, Wayland and xkb
# libraries, which minimal desktops lack. So the script copies those from the build container into
# usr/lib (the AppImage convention: everything except the AppImageCommunity excludelist and glibc; the
# list and how it is derived are in scripts/appimage-libs.sh), with their licences and a generated
# BUNDLED-LIBRARIES.txt. linuxdeploy would do the same by guessing; appimagetool would be a second
# pinned binary that is itself an AppImage and downloads the runtime unless given one. AppRun puts
# usr/lib on LD_LIBRARY_PATH for Siple only and saves the host value, which every child process Siple
# starts gets back (siple_config::channel::apply_host_env): bundled libraries must not leak into
# xdg-open, the browser or [speech] hooks.
#
# Run it inside rust:1.100-bookworm (glibc 2.36) after scripts/package-linux.sh, the same way CI does:
#
#   scripts/package-appimage.sh [TARBALL]   # default dist/Siple-$VERSION-linux-<arch>.tar.gz
#   scripts/package-appimage.sh --self-test # stub binaries, no cargo: layout + AppRun dispatch (CI)
#
# Environment:
#   APPIMAGE_RUNTIME=<file>  offline runtime; the sha256, size and signature checks still apply, the
#                            signature is read from <file>.sig
#   SOURCE_DATE_EPOCH        squashfs timestamps (default: `git log -1 --format=%ct`, else 0)
# Needs: mksquashfs, curl, sha256sum, gpg, gpgv, tar; outside --self-test also objdump, dpkg-query, ldconfig
# and the runtime packages of the GUI libraries ($GUI_DEB_PACKAGES in scripts/appimage-libs.sh).
# The runtime of the architecture and its signature are pinned in scripts/appimage-pins.sh and verified before anything
# is executed.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# shellcheck source=scripts/appimage-pins.sh
. "$ROOT/scripts/appimage-pins.sh"
# shellcheck source=scripts/linux-arch.sh
. "$ROOT/scripts/linux-arch.sh"
# shellcheck source=scripts/appimage-libs.sh
. "$ROOT/scripts/appimage-libs.sh"
runtime_pins "$LINUX_ARCH"
APPID=io.zyno.siple
CACHE="$ROOT/target/appimage"

SELF_TEST=0
TARBALL=""
case "${1:-}" in
  --self-test) SELF_TEST=1 ;;
  "") ;;
  -*) echo "usage: $0 [TARBALL | --self-test]" >&2; exit 2 ;;
  *) TARBALL="$1" ;;
esac

die() { echo "ERROR: $*" >&2; exit 1; }

# --- the pinned runtime ----------------------------------------------------------------------------

# fetch URL SHA256 DEST: downloads unless DEST is there with the right hash; the hash is checked
# every time, before DEST is used.
fetch() {
  local url="$1" sha="$2" dest="$3" tmp
  if [ -f "$dest" ] && echo "$sha  $dest" | sha256sum -c - >/dev/null 2>&1; then
    return 0
  fi
  mkdir -p "$(dirname "$dest")"
  tmp="$(mktemp "$dest.XXXXXX")"
  if ! curl --proto '=https' --tlsv1.2 -fsSL -o "$tmp" "$url" ||
    ! echo "$sha  $tmp" | sha256sum -c - >/dev/null 2>&1; then
    rm -f "$tmp"
    die "cannot download $url with the pinned sha256 $sha"
  fi
  mv "$tmp" "$dest"
}

# fetch_runtime: sets RT and SIG to a verified runtime and its signature.
fetch_runtime() {
  if [ -n "${APPIMAGE_RUNTIME:-}" ]; then
    RT="$APPIMAGE_RUNTIME"
    SIG="$APPIMAGE_RUNTIME.sig"
    [ -f "$RT" ] || die "APPIMAGE_RUNTIME $RT does not exist"
    [ -f "$SIG" ] || die "$SIG does not exist"
  else
    RT="$CACHE/runtime-$LINUX_ARCH-$RUNTIME_RELEASE"
    SIG="$RT.sig"
    fetch "$RUNTIME_URL" "$RUNTIME_SHA256" "$RT"
    fetch "$RUNTIME_URL.sig" "$RUNTIME_SIG_SHA256" "$SIG"
  fi
  # Re-checked before every use, whatever the source.
  [ "$(wc -c <"$RT" | tr -d ' ')" = "$RUNTIME_SIZE" ] || die "the runtime is not $RUNTIME_SIZE bytes"
  echo "$RUNTIME_SHA256  $RT" | sha256sum -c - >/dev/null || die "the runtime does not match the pinned sha256"
  echo "$RUNTIME_SIG_SHA256  $SIG" | sha256sum -c - >/dev/null || die "the signature does not match the pinned sha256"
  local g
  g="$(mktemp -d)"
  chmod 700 "$g"
  # A private GNUPGHOME: nothing touches the user's keyring.
  if ! GNUPGHOME="$g" gpg --batch --dearmor <"$ROOT/packaging/appimage/type2-runtime-signing-pubkey.asc" >"$g/k.gpg" ||
    ! GNUPGHOME="$g" gpgv --status-fd 1 --keyring "$g/k.gpg" "$SIG" "$RT" 2>/dev/null |
      grep -q "^\[GNUPG:\] VALIDSIG $RUNTIME_KEY_FPR "; then
    rm -rf "$g"
    die "the runtime signature is not valid for key $RUNTIME_KEY_FPR"
  fi
  rm -rf "$g"
  echo "==> runtime $RUNTIME_RELEASE verified (sha256, size, signature by $RUNTIME_KEY_FPR)"
}

# --- the bundled GUI libraries ------------------------------------------------------------------

# bundle_gui_libs APPDIR: copies GUI_DLOPEN_SONAMES and their transitive NEEDED libraries (minus the
# excludelist, which includes glibc) from this machine's multiarch library directory into
# APPDIR/usr/lib, their Debian licences into usr/share/doc/siple/bundled-licenses/<package>/, and
# writes usr/share/doc/siple/BUNDLED-LIBRARIES.txt. Fails if a bundled library needs a library that is
# neither bundled nor excluded, or if a wanted library is missing from the container.
bundle_gui_libs() {
  local appdir="$1" lib doc excl multiarch dir
  lib="$appdir/usr/lib"
  doc="$appdir/usr/share/doc/siple"
  case "$LINUX_ARCH" in
    x86_64) multiarch=x86_64-linux-gnu ;;
    aarch64) multiarch=aarch64-linux-gnu ;;
  esac
  excl="$(excluded_sonames "$ROOT/packaging/appimage/excludelist")"
  is_excluded() { printf '%s\n' "$excl" | grep -qxF "$1"; }
  # find_lib SONAME: the file on this machine.
  find_lib() {
    for dir in "/usr/lib/$multiarch" "/lib/$multiarch"; do
      [ -e "$dir/$1" ] && { echo "$dir/$1"; return 0; }
    done
    return 1
  }
  install -d -m 755 "$lib" "$doc/bundled-licenses"
  local queue="$GUI_DLOPEN_SONAMES" bundled="" s src real n
  while [ -n "$queue" ]; do
    s="${queue%% *}"
    if [ "$queue" = "$s" ]; then queue=""; else queue="${queue#* }"; fi
    case " $bundled " in *" $s "*) continue ;; esac
    is_excluded "$s" && die "$s is on the excludelist but wanted as a GUI library"
    src="$(find_lib "$s")" || die "$s is not in /usr/lib/$multiarch (apt-get install $GUI_DEB_PACKAGES)"
    real="$(readlink -f "$src")"
    install -m 755 "$real" "$lib/$s"
    bundled="$bundled $s"
    for n in $(objdump -p "$real" | awk '/NEEDED/ {print $2}'); do
      is_excluded "$n" || queue="$queue $n"
    done
    queue="${queue# }"
  done
  # Verify the closure from the copies themselves: every NEEDED is bundled or excluded.
  local f
  for f in "$lib"/*; do
    for n in $(objdump -p "$f" | awk '/NEEDED/ {print $2}'); do
      [ -f "$lib/$n" ] || is_excluded "$n" ||
        die "bundled $(basename "$f") needs $n, which is neither bundled nor on the excludelist"
    done
  done
  # Provenance and licences.
  local out="$doc/BUNDLED-LIBRARIES.txt" pkg ver pkgs=""
  {
    echo "Libraries bundled in this AppImage (usr/lib)"
    echo "============================================"
    echo
    echo "Siple loads the X11, Wayland and xkb libraries at run time. They are copied unmodified from the"
    echo "Debian 12 (bookworm) build image so minimal desktops that lack them work. Everything on the"
    echo "AppImageCommunity excludelist (libX11, libxcb, libGL/EGL, libwayland-client, ALSA, glibc, ...) comes"
    echo "from the host instead. Licences: bundled-licenses/<package>/copyright (MIT/X11 style)."
    echo
    printf '%-28s %-24s %s\n' "library" "Debian package" "version"
  } >"$out"
  # shellcheck disable=SC2086  # a word list
  for s in $(printf '%s\n' $bundled | LC_ALL=C sort); do
    real="$(readlink -f "$(find_lib "$s")")"
    pkg_out="$(dpkg -S "$real" 2>&1)" || pkg_out="no output: $pkg_out"
    pkg="$(printf '%s\n' "$pkg_out" | grep -m1 ': /' | cut -d: -f1 || true)"
    [ -n "$pkg" ] || die "no Debian package owns $real (dpkg -S: $pkg_out)"
    ver="$(dpkg-query -W -f='${Version}' "$pkg")"
    printf '%-28s %-24s %s\n' "$s" "$pkg" "$ver" >>"$out"
    case " $pkgs " in *" $pkg "*) ;; *) pkgs="$pkgs $pkg" ;; esac
  done
  for pkg in $pkgs; do
    [ -f "/usr/share/doc/$pkg/copyright" ] || die "/usr/share/doc/$pkg/copyright is missing"
    install -d -m 755 "$doc/bundled-licenses/$pkg"
    install -m 644 "/usr/share/doc/$pkg/copyright" "$doc/bundled-licenses/$pkg/copyright"
  done
  chmod 644 "$out"
  echo "==> bundled:$bundled"
}

# --- the build -----------------------------------------------------------------------------------

# build_appimage TARBALL OUT VERSION WORK EPOCH [stub]: unpacks TARBALL under WORK, assembles the
# AppDir, writes the AppImage OUT. `stub`: the binaries are shell stubs (no NEEDED check).
build_appimage() {
  local tarball="$1" out="$2" version="$3" work="$4" epoch="$5" stub="${6:-}"
  local top="siple-$version" src="$work/src" appdir="$work/AppDir"
  rm -rf "$src" "$appdir" "$work/image.squashfs"
  mkdir -p "$src"
  tar -C "$src" -xzf "$tarball"
  [ "$(find "$src" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' ')" = 1 ] && [ -d "$src/$top" ] ||
    die "the tarball must hold exactly one top directory, $top/"
  local t="$src/$top"

  install -d -m 755 "$appdir/usr/bin" "$appdir/usr/share/applications" "$appdir/usr/share/metainfo" \
    "$appdir/usr/share/doc/siple"
  install -m 755 "$ROOT/packaging/appimage/AppRun" "$appdir/AppRun"
  install -m 755 "$t/bin/siple" "$t/bin/siple-cli" "$appdir/usr/bin/"
  install -m 644 "$ROOT/packaging/linux/$APPID.desktop" "$appdir/$APPID.desktop"
  install -m 644 "$ROOT/packaging/linux/hicolor/256x256/apps/$APPID.png" "$appdir/$APPID.png"
  ln -s "$APPID.png" "$appdir/.DirIcon"
  install -m 644 "$t/share/applications/$APPID.desktop" "$appdir/usr/share/applications/$APPID.desktop"
  install -m 644 "$t/share/metainfo/$APPID.metainfo.xml" "$appdir/usr/share/metainfo/$APPID.metainfo.xml"
  local size
  for size in 64x64 128x128 256x256 512x512; do
    install -d -m 755 "$appdir/usr/share/icons/hicolor/$size/apps"
    install -m 644 "$ROOT/packaging/linux/hicolor/$size/apps/$APPID.png" \
      "$appdir/usr/share/icons/hicolor/$size/apps/$APPID.png"
  done
  local d
  for d in README.md CONFIG.md API.md; do
    install -m 644 "$t/share/doc/siple/$d" "$appdir/usr/share/doc/siple/$d"
  done
  install -m 644 "$t/LICENSE" "$appdir/usr/share/doc/siple/LICENSE"
  # Absent in dev builds without cargo-about.
  if [ -f "$t/THIRD-PARTY-NOTICES.html" ]; then
    install -m 644 "$t/THIRD-PARTY-NOTICES.html" "$appdir/usr/share/doc/siple/THIRD-PARTY-NOTICES.html"
  fi
  install -m 644 "$ROOT/packaging/appimage/APPIMAGE-RUNTIME.txt" "$appdir/usr/share/doc/siple/APPIMAGE-RUNTIME.txt"

  # Guards.
  # (a) the GUI libraries: bundled from the container, never in a stub build, and none excluded
  local so
  if [ -z "$stub" ]; then
    bundle_gui_libs "$appdir"
    for so in $GUI_DLOPEN_SONAMES; do
      [ -f "$appdir/usr/lib/$so" ] || die "$so is not bundled"
    done
  else
    [ -z "$(find "$appdir" -name '*.so*')" ] || die "a stub AppDir bundles a library"
  fi
  [ -z "$(find "$appdir" -name '*.so*' ! -path "$appdir/usr/lib/*")" ] || die "a library outside usr/lib"
  local excl_list
  excl_list="$(excluded_sonames "$ROOT/packaging/appimage/excludelist")"
  for so in $(find "$appdir/usr/lib" -type f -name '*.so*' 2>/dev/null | sed 's|.*/||'); do
    ! printf '%s\n' "$excl_list" | grep -qxF "$so" || die "$so is on the excludelist and must not be bundled"
  done
  # (b) the host contract of the Siple binaries (a stub has no ELF headers)
  if [ -z "$stub" ]; then
    "$ROOT/scripts/linux-needed-check.sh" "$appdir/usr/bin/siple" "$appdir/usr/bin/siple-cli"
  fi
  # (c) the root desktop file and icon an integrator needs
  [ "$(find "$appdir" -maxdepth 1 -name '*.desktop' | wc -l | tr -d ' ')" = 1 ] || die "the AppDir needs exactly one root .desktop file"
  grep -qx "Icon=$APPID" "$appdir/$APPID.desktop" || die "the desktop file's Icon= is not $APPID"
  [ -f "$appdir/$APPID.png" ] || die "the AppDir has no root icon"
  [ -x "$appdir/AppRun" ] || die "AppRun is not executable"
  # (d) the only symlink is .DirIcon
  [ -z "$(find "$appdir" -type l ! -name .DirIcon)" ] || die "the AppDir holds a symlink besides .DirIcon"

  # The image. mksquashfs 4.5.1 refuses SOURCE_DATE_EPOCH together with the time options.
  mksquashfs -version | head -1
  env -u SOURCE_DATE_EPOCH mksquashfs "$appdir" "$work/image.squashfs" -comp zstd -all-root -noappend \
    -no-xattrs -mkfs-time "$epoch" -all-time "$epoch" -quiet -no-progress

  cat "$RT" "$work/image.squashfs" >"$out.tmp"
  chmod 0755 "$out.tmp"
  mv "$out.tmp" "$out"

  # Post-checks.
  [ "$(head -c "$RUNTIME_SIZE" "$out" | sha256sum | cut -d' ' -f1)" = "$RUNTIME_SHA256" ] ||
    die "the runtime inside $out is not the pinned one"
  [ "$("$out" --appimage-offset)" = "$RUNTIME_SIZE" ] || die "$out --appimage-offset is not $RUNTIME_SIZE"
  echo "==> $out ($(du -h "$out" | cut -f1), $(wc -c <"$out" | tr -d ' ') bytes)"
}

EPOCH="${SOURCE_DATE_EPOCH:-$(git -C "$ROOT" log -1 --format=%ct 2>/dev/null || echo 0)}"

if [ "$SELF_TEST" = 0 ]; then
  VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' Cargo.toml | head -1)"
  [ -n "$VERSION" ] || die "cannot read version from Cargo.toml"
  DIST="$ROOT/dist"
  TARBALL="${TARBALL:-$DIST/Siple-$VERSION-linux-$LINUX_ARCH.tar.gz}"
  [ -f "$TARBALL" ] || die "$TARBALL not found (run scripts/package-linux.sh first)"
  OUT="$DIST/Siple-$VERSION-linux-$LINUX_ARCH.AppImage"
  echo "==> Siple $VERSION AppImage (linux $LINUX_ARCH) from $TARBALL"
  fetch_runtime
  WORK="$DIST/appimage-work"
  trap 'rm -rf "$WORK"' EXIT
  rm -rf "$WORK"
  mkdir -p "$WORK"
  build_appimage "$TARBALL" "$OUT" "$VERSION" "$WORK" "$EPOCH"
  exit 0
fi

# --- --self-test -----------------------------------------------------------------------------------
[ "$(uname -s)" = Linux ] && [ -z "${SIPLE_UNAME_M:-}" ] || die "--self-test runs the native Linux runtime"
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
VERSION=0.0.0
echo "==> AppImage self-test in $T"
fetch_runtime

# A fake tarball: shell stubs instead of binaries.
S="$T/tar/siple-$VERSION"
mkdir -p "$S/bin" "$S/share/applications" "$S/share/metainfo" "$S/share/doc/siple"
printf '#!/bin/sh\necho "siple 0.0.0 $*"\n' >"$S/bin/siple"
printf '#!/bin/sh\necho "siple-cli 0.0.0 $*"\n' >"$S/bin/siple-cli"
chmod 755 "$S/bin/siple" "$S/bin/siple-cli"
cp "packaging/linux/$APPID.desktop" "$S/share/applications/"
cp "packaging/linux/$APPID.metainfo.xml" "$S/share/metainfo/"
for d in README.md CONFIG.md API.md; do echo "$d" >"$S/share/doc/siple/$d"; done
echo license >"$S/LICENSE"
tar -C "$T/tar" --owner=0 --group=0 -czf "$T/siple.tar.gz" "siple-$VERSION"

mkdir -p "$T/w1" "$T/w2" "$T/tmp"
A="$T/Siple.AppImage"
build_appimage "$T/siple.tar.gz" "$A" "$VERSION" "$T/w1" "$EPOCH" stub

# The magic of a type 2 AppImage.
[ "$(od -An -c -j8 -N3 "$A" | tr -d ' ')" = 'AI002' ] || die "no AI\\x02 magic at offset 8"

# The runtime extracts the image without FUSE (the CI container has none); TMPDIR keeps the extraction
# (the runtime never removes it) inside the test directory.
export TMPDIR="$T/tmp" APPIMAGE_EXTRACT_AND_RUN=1
expect_out() { # expect_out WANT CMD...
  local want="$1" got
  shift
  got="$("$@")" || die "$* failed"
  [ "$got" = "$want" ] || die "$*: got '$got', want '$want'"
}
expect_out "siple 0.0.0 --version" "$A" --version
expect_out "siple-cli 0.0.0 x y" "$A" --cli x y
ln -s "$A" "$T/siple-cli"
expect_out "siple-cli 0.0.0 --version" "$T/siple-cli" --version
# shellcheck disable=SC2016  # $0 is for the inner shell
expect_out "siple-cli 0.0.0 --version" bash -c 'exec -a siple-cli "$0" --version' "$A"
unset APPIMAGE_EXTRACT_AND_RUN
expect_out "siple 0.0.0 --version" "$A" --appimage-extract-and-run --version

# The layout is the golden list.
mkdir "$T/x"
(cd "$T/x" && "$A" --appimage-extract >/dev/null)
got="$(cd "$T/x/squashfs-root" && find . ! -name . | sort)"
want="$(
  cat <<'EOF' | sort
./.DirIcon
./AppRun
./io.zyno.siple.desktop
./io.zyno.siple.png
./usr
./usr/bin
./usr/bin/siple
./usr/bin/siple-cli
./usr/share
./usr/share/applications
./usr/share/applications/io.zyno.siple.desktop
./usr/share/doc
./usr/share/doc/siple
./usr/share/doc/siple/API.md
./usr/share/doc/siple/APPIMAGE-RUNTIME.txt
./usr/share/doc/siple/CONFIG.md
./usr/share/doc/siple/LICENSE
./usr/share/doc/siple/README.md
./usr/share/icons
./usr/share/icons/hicolor
./usr/share/icons/hicolor/128x128
./usr/share/icons/hicolor/128x128/apps
./usr/share/icons/hicolor/128x128/apps/io.zyno.siple.png
./usr/share/icons/hicolor/256x256
./usr/share/icons/hicolor/256x256/apps
./usr/share/icons/hicolor/256x256/apps/io.zyno.siple.png
./usr/share/icons/hicolor/512x512
./usr/share/icons/hicolor/512x512/apps
./usr/share/icons/hicolor/512x512/apps/io.zyno.siple.png
./usr/share/icons/hicolor/64x64
./usr/share/icons/hicolor/64x64/apps
./usr/share/icons/hicolor/64x64/apps/io.zyno.siple.png
./usr/share/metainfo
./usr/share/metainfo/io.zyno.siple.metainfo.xml
EOF
)"
[ "$got" = "$want" ] || {
  echo "$got" >"$T/got.txt"
  echo "$want" >"$T/want.txt"
  diff "$T/want.txt" "$T/got.txt" >&2 || true
  die "the AppDir layout differs from the golden list"
}

# Reproducible: the same inputs give the same file.
build_appimage "$T/siple.tar.gz" "$T/Again.AppImage" "$VERSION" "$T/w2" "$EPOCH" stub
[ "$(sha256sum <"$A")" = "$(sha256sum <"$T/Again.AppImage")" ] || die "two builds differ"

echo "==> AppImage self-test passed"
