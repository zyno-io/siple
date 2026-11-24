#!/bin/sh
# Copies the app's own rendered screenshots into public/shots/ as WebP.
#
#   npm run shots     (SIPLE_REPO defaults to this repo's root, three levels up)
#
# Source: <SIPLE_REPO>/crates/siple/tests/snapshots/*.png, the GUI snapshot
# tests' goldens (egui, rendered on macOS). Nothing is retouched: wide shots are
# re-encoded as-is; the phone versions are the app's narrow layout cropped to
# their top 1000px / 1240px (the empty lower part of the window is dropped). Needs ImageMagick 7
# (`magick`) with WebP support. Commit the output; the build does not run this.
set -eu
SRC="${SIPLE_REPO:-$(dirname "$0")/../../..}/crates/siple/tests/snapshots"
OUT="$(dirname "$0")/../public/shots"
mkdir -p "$OUT"

webp() { # <in.png> <out.webp> [extra magick args...]
  in=$1 out=$2; shift 2
  magick "$in" "$@" -strip -define webp:lossless=true -define webp:method=6 \
    -set comment "Siple GUI snapshot $(basename "$in") from github.com/zyno-io/siple crates/siple/tests/snapshots (unretouched)" "$out"
}

for theme in light dark; do
  for name in two_lines incoming transfer_consult transfer_composer blf_list; do
    webp "$SRC/${name}_${theme}_wide.png" "$OUT/${name}-${theme}.webp"
  done
  webp "$SRC/two_lines_${theme}_narrow.png" "$OUT/two_lines-${theme}-narrow.webp" -crop 840x1000+0+0 +repage
  # Phone versions of the [calls] viewer: the app's narrow layout, top 1340px.
  for name in incoming transfer_consult transfer_composer blf_list; do
    webp "$SRC/${name}_${theme}_narrow.png" "$OUT/${name}-${theme}-narrow.webp" -crop 840x1340+0+0 +repage
  done
  # The setup-link review screen ([[account]]): the wide render's centered
  # column, the same focus the Siple docs use (setup-links guide), one size for
  # every viewport.
  webp "$SRC/setup_review_${theme}_wide.png" "$OUT/setup_review-${theme}.webp" -crop 612x480+314+0 +repage
done
ls -l "$OUT"
