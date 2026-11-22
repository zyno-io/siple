# shellcheck shell=bash
# Sourced by package-appimage.sh and appimage-smoke.sh: which libraries the AppImage bundles.
#
# winit, wgpu and egui open the X11, Wayland and xkb libraries with dlopen (they are not in the
# binaries' NEEDED lists), so a minimal desktop that lacks them used to crash at startup. The AppImage
# convention: bundle everything except the libraries on the AppImageCommunity excludelist
# (packaging/appimage/excludelist, pinned to a commit in its header) and glibc.
#
# GUI_DLOPEN_SONAMES: the sonames the dependencies try, with the Debian bookworm package that ships
# each (the build image, rust:<ver>-bookworm). Read from the sources of the pinned crates
# (Cargo.lock): xkbcommon-dl 0.4.2 (libxkbcommon.so.0, libxkbcommon-x11.so.0), x11-dl 2.21 as used by
# winit 0.30 (libXcursor.so.1, libXi.so.6), wayland-sys 0.31 (libwayland-cursor.so.0,
# libwayland-egl.so.1). The others they load (libX11, libX11-xcb, libxcb, libwayland-client, libEGL,
# libGL) are on the excludelist. Their transitive dependencies are resolved with objdump at build time.
# Keep crates/siple/src/gui_libs.rs (the startup preflight) in step with this list.
# shellcheck disable=SC2034  # used by the scripts that source this file
GUI_DLOPEN_SONAMES="libxkbcommon.so.0 libxkbcommon-x11.so.0 libXcursor.so.1 libXi.so.6 libwayland-cursor.so.0 libwayland-egl.so.1"
GUI_DEB_PACKAGES="libxkbcommon0 libxkbcommon-x11-0 libxcb-xkb1 libxcursor1 libxi6 libwayland-egl1 libwayland-cursor0"

# The excluded sonames, one per line: the excludelist without comments, plus the aarch64 loader (glibc).
# excluded_sonames EXCLUDELIST
excluded_sonames() {
  { sed -e 's/#.*//' -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' "$1"; echo ld-linux-aarch64.so.1; } | grep -v '^$' | sort -u
}
