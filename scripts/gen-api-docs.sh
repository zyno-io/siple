#!/usr/bin/env bash
# Regenerates docs/API.md = docs/API-intro.md + the method catalog from the code.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
CLI="${SIPLE_CLI:-}"
if [ -z "$CLI" ]; then
  cargo build -q -p siple-cli
  CLI="$ROOT/target/debug/siple-cli"
fi
N="$("$CLI" methods --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
{
  sed "s/{N} methods/$N methods/" docs/API-intro.md
  "$CLI" methods --markdown
} > docs/API.md
echo "wrote docs/API.md ($N methods)"
