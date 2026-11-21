#!/usr/bin/env bash
# Renders the Flathub copy of the manifest for a published release.
#
#   scripts/flathub-manifest.sh vX.Y.Z [OUT_DIR] [--manifest FILE] [--no-verify]
#     OUT_DIR   default dist/flathub/
#     --manifest FILE   take the manifest from FILE instead of the tag (dry runs against an old tag,
#                       whose tree has no Flatpak manifest)
#     --no-verify       continue without `minisign` (the SHA256SUMS signature is NOT checked; a loud
#                       warning is printed). Without this flag a missing `minisign` is an error.
#
# Run it after the GitHub Release is published: the notices come from the release's signed SHA256SUMS.
# Writes OUT_DIR/io.zyno.siple.yml (git tag + commit source, THIRD-PARTY-NOTICES.html as a file source
# with its sha256) and OUT_DIR/cargo-sources.json (regenerated from the tag's Cargo.lock). Final tags only.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

TAG=""; OUT="dist/flathub"; MANIFEST_FILE=""; NO_VERIFY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --manifest) MANIFEST_FILE="$2"; shift 2 ;;
    --no-verify) NO_VERIFY=1; shift ;;
    -*) echo "usage: $0 vX.Y.Z [OUT_DIR] [--manifest FILE] [--no-verify]" >&2; exit 2 ;;
    *) if [ -z "$TAG" ]; then TAG="$1"; else OUT="$1"; fi; shift ;;
  esac
done
[[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "usage: $0 vX.Y.Z [OUT_DIR] (final tags only)" >&2; exit 2; }

REPO_URL=https://github.com/zyno-io/siple
COMMIT="$(git rev-parse "$TAG^{commit}")"
mkdir -p "$OUT"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# 1. The release's signed checksums give the notices file's sha256.
BASE="$REPO_URL/releases/download/$TAG"
curl -fsSL "$BASE/SHA256SUMS" -o "$WORK/SHA256SUMS"
curl -fsSL "$BASE/SHA256SUMS.minisig" -o "$WORK/SHA256SUMS.minisig"
if command -v minisign >/dev/null 2>&1; then
  git show "$TAG:packaging/minisign.pub" > "$WORK/minisign.pub"
  out="$(minisign -V -p "$WORK/minisign.pub" -m "$WORK/SHA256SUMS" -x "$WORK/SHA256SUMS.minisig")"
  grep -qF "siple $TAG SHA256SUMS" <<<"$out" || { echo "error: unexpected trusted comment: $out" >&2; exit 1; }
  echo "SHA256SUMS signature verified"
else
  if [ "$NO_VERIFY" -ne 1 ]; then
    echo "error: minisign is not installed, so the SHA256SUMS signature cannot be verified." >&2
    echo "       Install minisign (brew install minisign / apt install minisign), or pass --no-verify to skip the check." >&2
    exit 1
  fi
  echo "!!! WARNING: --no-verify: the SHA256SUMS signature was NOT verified. The notices checksum below is unauthenticated." >&2
  echo "!!! Do not submit this output to Flathub; re-run with minisign installed." >&2
fi
NOTICES_SHA="$(awk '$2 == "THIRD-PARTY-NOTICES.html" { print $1 }' "$WORK/SHA256SUMS")"
[[ "$NOTICES_SHA" =~ ^[0-9a-f]{64}$ ]] || { echo "error: no THIRD-PARTY-NOTICES.html line in the release's SHA256SUMS" >&2; exit 1; }

# 2. The manifest with its local source block replaced.
if [ -n "$MANIFEST_FILE" ]; then
  cp "$MANIFEST_FILE" "$WORK/manifest.yml"
else
  git show "$TAG:packaging/flatpak/io.zyno.siple.yml" > "$WORK/manifest.yml"
fi
awk -v url="$REPO_URL.git" -v tag="$TAG" -v commit="$COMMIT" -v ntc="$BASE/THIRD-PARTY-NOTICES.html" -v sha="$NOTICES_SHA" '
  /# BEGIN local-source/ { skip = 1; found = 1
    print "      - type: git"
    print "        url: " url
    print "        tag: " tag
    print "        commit: " commit
    print "      - type: file"
    print "        url: " ntc
    print "        sha256: " sha
    next }
  /# END local-source/ { skip = 0; next }
  !skip { print }
  END { if (!found) exit 3 }
' "$WORK/manifest.yml" > "$OUT/io.zyno.siple.yml" || { echo "error: no local-source block in the manifest" >&2; exit 1; }

# 3. cargo-sources.json from the tag's lock file.
git show "$TAG:Cargo.lock" > "$WORK/Cargo.lock"
scripts/flatpak-cargo-sources.sh --lock "$WORK/Cargo.lock" --out "$OUT/cargo-sources.json"

# 4. Lint without exceptions, when the linter is available.
if command -v flatpak-builder-lint >/dev/null 2>&1; then
  flatpak-builder-lint manifest "$OUT/io.zyno.siple.yml"
else
  echo "note: flatpak-builder-lint is not installed; run it on $OUT/io.zyno.siple.yml (docs/RELEASING.md)" >&2
fi

cat <<MSG

Rendered $OUT/io.zyno.siple.yml and $OUT/cargo-sources.json for $TAG ($COMMIT).
Next: copy both files into a branch of flathub/io.zyno.siple (first submission: a branch of a fork of
flathub/flathub based on new-pr) and open the pull request. See docs/RELEASING.md, "Flatpak (Flathub)".
MSG
