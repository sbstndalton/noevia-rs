#!/bin/sh
# Required synthetic actual-core replay. No condition can skip native PUT or its omission.
set -eu
core="${1:?actual core checkout}"
front="${2:?feature-compiled native front}"
replay="${3:?replay binary}"
out="${4:?new evidence directory}"
root="$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)"
mkdir -p "$out/seed"
"$replay" run --corpus "$root/contracts/http/corpus" --base http://127.0.0.1:18021 \
  --ignore-body hours --ignore-body peakHour/hour --seed "$out/seed" --work "$out/node" \
  --server-cmd "exec node '$core/tools/contract-corpus/serve.cjs'" \
  --report "$out/node-report.json" --state-out "$out/node-state.json"
"$replay" run --corpus "$root/contracts/http/corpus" --base http://127.0.0.1:18021 \
  --ignore-body hours --ignore-body peakHour/hour --seed "$out/seed" --work "$out/front" \
  --server-cmd "exec env NOEVIA_RUST_AUTH=1 NOEVIA_RUST_REASONING_SETTINGS=1 sh '$root/tools/front/replay-front.sh' '$core' '$front'" \
  --report "$out/positive-report.json" --state-out "$out/positive-state.json" --expect-state "$out/node-state.json"
if "$replay" run --corpus "$root/contracts/http/corpus" --base http://127.0.0.1:18021 \
  --ignore-body hours --ignore-body peakHour/hour --seed "$out/seed" --work "$out/omission" \
  --server-cmd "exec env NOEVIA_RUST_AUTH=1 NOEVIA_RUST_REASONING_SETTINGS=1 REPLAY_FRONT_CONTROL_NODE_ONLY_REASONING_SETTINGS=1 sh '$root/tools/front/replay-front.sh' '$core' '$front'" \
  --report "$out/omission-report.json" --state-out "$out/omission-state.json"; then
  echo 'reasoning omission replay passed unexpectedly' >&2; exit 1
fi
python3 - "$out/omission-report.json" "$out/omission-state.json" <<'PY'
import json,sys
rows=json.load(open(sys.argv[1]));state=json.load(open(sys.argv[2]))
puts=[r for r in rows if r['method']=='PUT' and r['path']=='/api/reasoning-settings' and r['expectedStatus']==200]
sessions=[r for r in rows if r['method']=='GET' and r['path']=='/api/auth/session' and r['expectedStatus']==200]
assert len(puts)==3 and all(r['status']==503 for r in puts),puts
assert len(sessions)==3 and all(r['status']==200 for r in sessions),sessions
assert not any(r.get('key')=='reasoning_effort_default' for r in state['db']['settings'])
assert not any(r.get('action')=='reasoning.default' for r in state['db']['audit_events'])
print('reasoning omission: 3 PUT503, 3 valid native sessions200, zero setting/audit writes')
PY
