#!/usr/bin/env bash
# Regenerates docs/PROBLEMS.md (the problem catalog) and docs/API.md = docs/API-intro.md with the
# problem table embedded at its `<!-- problem-kinds -->` line + the method catalog, all from the code.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
CLI="${SIPLE_CLI:-}"
if [ -z "$CLI" ]; then
  cargo build -q -p siple-cli
  CLI="$ROOT/target/debug/siple-cli"
fi
N="$("$CLI" methods --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
"$CLI" problems --markdown > docs/PROBLEMS.md
METHODS="$(mktemp)"
trap 'rm -f "$METHODS"' EXIT
"$CLI" methods --markdown > "$METHODS"
python3 - "$N" "$METHODS" <<'PY'
import sys

MARKER = "<!-- problem-kinds -->\n"
n, methods_path = sys.argv[1], sys.argv[2]
intro = open("docs/API-intro.md", encoding="utf-8").read().replace("{N} methods", n + " methods")
if intro.count(MARKER) != 1:
    sys.exit("docs/API-intro.md must contain exactly one line: " + MARKER.strip())
table = open("docs/PROBLEMS.md", encoding="utf-8").read()
methods = open(methods_path, encoding="utf-8").read()
with open("docs/API.md", "w", encoding="utf-8", newline="\n") as out:
    out.write(intro.replace(MARKER, table))
    out.write(methods)
PY
echo "wrote docs/PROBLEMS.md and docs/API.md ($N methods)"
