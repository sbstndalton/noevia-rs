#!/usr/bin/env bash
# Refresh contracts/http/corpus from the contract-corpus artifact of a noevia-core CI run
# (the "Record the synthetic HTTP contract corpus" step of the Server tests job).
#
#   tools/contracts/refresh-corpus.sh <noevia-core run id>
#
# The corpus is synthetic (throwaway data dir, mock model and Diary) and already normalised by
# the recorder; this re-checks that no raw cookie or bearer value slipped through before it lands.
set -euo pipefail
run="${1:?usage: refresh-corpus.sh <noevia-core actions run id>}"
root="$(cd "$(dirname "$0")/../.." && pwd)"
dest="$root/contracts/http/corpus"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
gh run download "$run" --repo sbstndalton/noevia-core --name contract-corpus --dir "$tmp/c"
[ -f "$tmp/c/manifest.json" ] || { echo "artifact has no manifest.json" >&2; exit 1; }
# A cookie value or bearer credential that is not a placeholder (a challenge such as
# `WWW-Authenticate: Bearer realm="cowork"` is not a credential).
python3 - "$tmp/c/exchanges" <<'PY'
import pathlib, re, sys
bad = re.compile(r'cowork_(session|csrf)=(?!<secret:|;|"|\s|$)|Bearer (?!<secret:|realm=)')
hits = [p.name for p in pathlib.Path(sys.argv[1]).glob("*.json") if bad.search(p.read_text())]
if hits:
    sys.exit("refusing: a raw cookie or bearer value is in " + ", ".join(hits[:5]))
PY
if ls "$tmp/c/exchanges"/*.dropped >/dev/null 2>&1; then
  echo "refusing: the recorder dropped exchanges (a secret survived normalisation)" >&2; exit 1
fi
rm -rf "$dest"
mkdir -p "$dest"
cp -R "$tmp/c/." "$dest/"
echo "$run" > "$dest/SOURCE_RUN"
echo "corpus: $(ls "$dest/exchanges" | wc -l | tr -d ' ') exchanges from noevia-core run $run"
