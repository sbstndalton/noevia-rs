#!/bin/sh
# Required synthetic actual-core replay. No condition can skip the native PUT.
set -eu
core="${1:?actual core checkout (contracts/http/core.ref)}"
paired="${2:?paired core checkout (contracts/http/front-core.ref)}"
front="${3:?feature-compiled native front}"
replay="${4:?replay binary}"
out="${5:?new evidence directory}"
root="$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)"
mkdir -p "$out/seed"
"$replay" run --corpus "$root/contracts/http/corpus" --base http://127.0.0.1:18021 \
  --ignore-body hours --ignore-body peakHour/hour --seed "$out/seed" --work "$out/node" \
  --server-cmd "exec '$(dirname "$replay")/corpus-launcher' serve '$core'" \
  --report "$out/node-report.json" --state-out "$out/node-state.json"
"$replay" run --corpus "$root/contracts/http/corpus" --base http://127.0.0.1:18021 \
  --ignore-body hours --ignore-body peakHour/hour --seed "$out/seed" --work "$out/front" \
  --native-move-state --server-cmd "exec env NOEVIA_RUST_AUTH=1 sh '$root/tools/front/replay-front.sh' '$paired' '$front'" \
  --state-ignore '*projects.json.lock' --state-ignore '*free-chats.json.lock' --state-ignore '*chat-transcripts.lock' --state-ignore '*chat-changes.jsonl' \
  --report "$out/positive-report.json" --state-out "$out/positive-state.json" --expect-state "$out/node-state.json"
# The omission control left with the reasoning switch (sbstndalton/noevia#1071): the front can no
# longer omit the PUT, and core's own tests prove Node refuses it.
