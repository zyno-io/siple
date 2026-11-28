#!/usr/bin/env bash
# Generates the offline cargo sources of the Flatpak manifest from Cargo.lock.
#
#   scripts/flatpak-cargo-sources.sh [--lock FILE] [--out FILE]
#     defaults: Cargo.lock, packaging/flatpak/cargo-sources.json
#   scripts/flatpak-cargo-sources.sh --self-test
#     runs the cross-check against tampered fixtures (each must be rejected); needs python >= 3.11 only
#
# Uses flatpak-cargo-generator from flatpak/flatpak-builder-tools, downloaded at a pinned commit and
# checked against a pinned SHA-256 (supply chain), then cross-checks its output strictly against the
# lock file (scripts/flatpak-cargo-check.py): per crate exactly one archive (URL, sha256, dest), one
# checksum file, plus the one cargo config with its known text; anything else and unknown source types
# fail. Git dependencies only from the repos and packages in its GIT_ALLOWED, pinned with rev.
# The output is a build product (gitignored): it is regenerated per build and per Flathub release.
#
# Needs python3 >= 3.11 (tomllib) and network access (PyPI for aiohttp and tomlkit, GitHub for the
# generator, crates.io index-free: the generator reads checksums from the lock file).
# In a container run it with a writable HOME (e.g. -e HOME=/tmp).
#
# Bump procedure for the pin: pick a commit of flatpak/flatpak-builder-tools, download
# cargo/flatpak-cargo-generator.py at that commit, read it, set GEN_COMMIT and GEN_SHA256 below
# (`shasum -a 256`), and run this script on the current Cargo.lock.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

GEN_COMMIT=41c20aa10819cdb2a4f3ca171758a96d1955c018
GEN_SHA256=0a2db6be87d75910facef28ab46d4d6460802e8419ab850d0caa6a364d26b380
GEN_URL="https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/$GEN_COMMIT/cargo/flatpak-cargo-generator.py"
# Python dependencies of the generator (its pyproject.toml: aiohttp, tomlkit; PyYAML is not imported),
# every transitive one hash-pinned (flatpak-cargo-requirements.txt, from the .in file next to it).
REQS="$ROOT/scripts/flatpak-cargo-requirements.txt"

SELF_TEST=0
LOCK="$ROOT/Cargo.lock"
OUT="$ROOT/packaging/flatpak/cargo-sources.json"
while [ $# -gt 0 ]; do
  case "$1" in
    --lock) LOCK="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --self-test) SELF_TEST=1; shift ;;
    *) echo "usage: $0 [--lock FILE] [--out FILE] | --self-test" >&2; exit 2 ;;
  esac
done
PY=
for c in python3 python3.13 python3.12 python3.11; do
  if command -v "$c" >/dev/null 2>&1 && "$c" -c 'import sys; sys.exit(sys.version_info < (3, 11))'; then PY="$c"; break; fi
done
[ -n "$PY" ] || { echo "error: python3 >= 3.11 is required (tomllib)" >&2; exit 1; }
if [ "$SELF_TEST" -eq 1 ]; then exec "$PY" -I "$ROOT/scripts/flatpak-cargo-check.py" --self-test; fi
[ -f "$LOCK" ] || { echo "error: $LOCK not found" >&2; exit 1; }

TOOLS="$ROOT/target/flatpak-tools"
mkdir -p "$TOOLS" "$(dirname "$OUT")"
GEN="$TOOLS/flatpak-cargo-generator-$GEN_COMMIT.py"

"$PY" -I - "$GEN" "$GEN_URL" "$GEN_SHA256" <<'PY'
import hashlib, os, sys, urllib.request
dest, url, want = sys.argv[1:4]
def sha(p):
    with open(p, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()
if os.path.exists(dest) and sha(dest) == want:
    sys.exit(0)
tmp = dest + ".part"
with urllib.request.urlopen(url, timeout=60) as r, open(tmp, "wb") as f:
    f.write(r.read())
got = sha(tmp)
if got != want:
    os.unlink(tmp)
    sys.exit(f"error: flatpak-cargo-generator SHA-256 mismatch: got {got}, pinned {want}")
os.replace(tmp, dest)
PY

# The cached venv is tied to the interpreter's major.minor (stored next to it): recreate it when that
# changes, when the stamp is missing (venvs from before it existed) or when it is below 3.11.
VENV="$TOOLS/venv"
PYVER="$("$PY" -c 'import sys; print("%d.%d" % sys.version_info[:2])')"
if [ ! -x "$VENV/bin/python" ] || [ "$(cat "$TOOLS/venv.pyver" 2>/dev/null || true)" != "$PYVER" ] \
  || ! "$VENV/bin/python" -c 'import sys; sys.exit(sys.version_info < (3, 11))'; then
  rm -rf "$VENV" "$TOOLS/venv.pyver"
  "$PY" -m venv "$VENV"
  echo "$PYVER" > "$TOOLS/venv.pyver"
fi
"$VENV/bin/python" -m pip install --quiet --disable-pip-version-check --require-hashes --only-binary=:all: -r "$REQS"

TMP="$OUT.tmp"
# Fresh git clone cache per run: the generator reuses ~/.cache/flatpak-cargo/<repo> without checking it
# for local edits, and it writes git packages' Cargo.toml from that checkout.
GITCACHE="$(mktemp -d)"
trap 'rm -f "$TMP"; rm -rf "$GITCACHE"' EXIT
XDG_CACHE_HOME="$GITCACHE" "$VENV/bin/python" -I "$GEN" "$LOCK" -o "$TMP"

"$PY" -I "$ROOT/scripts/flatpak-cargo-check.py" "$LOCK" "$TMP"

mv "$TMP" "$OUT"
echo "wrote $OUT"
