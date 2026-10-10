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
if grep -rEl 'cowork_(session|csrf)=[^<;"[:space:]]|Bearer [^<"]' "$tmp/c/exchanges" >/dev/null 2>&1; then
  echo "refusing: a raw cookie or bearer value is in the artifact" >&2; exit 1
fi
if ls "$tmp/c/exchanges"/*.dropped >/dev/null 2>&1; then
  echo "refusing: the recorder dropped exchanges (a secret survived normalisation)" >&2; exit 1
fi
rm -rf "$dest"
mkdir -p "$dest"
cp -R "$tmp/c/." "$dest/"
echo "$run" > "$dest/SOURCE_RUN"
echo "corpus: $(ls "$dest/exchanges" | wc -l | tr -d ' ') exchanges from noevia-core run $run"
