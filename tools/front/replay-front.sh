#!/bin/sh
# M1 replay gate: Node (noevia-core tools/contract-corpus/serve.cjs, the mocks the corpus was
# recorded with) on loopback UI_PORT+1000, noevia-server on UI_PORT in front of it, the way the
# web image runs them with NOEVIA_FRONT=rust. Used as tools/replay's --server-cmd:
#
#   replay run ... --base http://127.0.0.1:18021 \
#     --server-cmd "exec sh tools/front/replay-front.sh <noevia-core> target/debug/noevia-server"
#
# replay sets UI_DATA_DIR / UI_PORT / UI_HOST and stops the whole process group afterwards.
set -eu
core="${1:?usage: replay-front.sh <noevia-core dir> <noevia-server binary>}"
front="${2:?usage: replay-front.sh <noevia-core dir> <noevia-server binary>}"
port="${UI_PORT:?UI_PORT is required}"
legacy=$((port + 1000))
# Node must see the origin the browser (here: the replayer) uses, which is the front's; so must the
# front, which answers sign-in itself with NOEVIA_RUST_AUTH=1 (M3; serve.cjs then runs Node as
# behind that front: read-only for the account tables).
origin="${PUBLIC_ORIGIN:-http://127.0.0.1:$port}"
# Like build/web-supervisor.sh: Node's write guard is confirmed only by a front that lists rust-auth.
unset NOEVIA_RUST_AUTH_CONFIRMED
confirmed=""
if [ "${NOEVIA_RUST_AUTH:-}" = 1 ]; then
  "$front" --features | grep -qx rust-auth || { echo "replay-front: $front has no rust-auth" >&2; exit 2; }
  confirmed=1
fi
UI_PORT="$legacy" PUBLIC_ORIGIN="$origin" NOEVIA_RUST_AUTH_CONFIRMED="$confirmed" \
  node "$core/tools/contract-corpus/serve.cjs" &
node_pid=$!
PUBLIC_ORIGIN="$origin" NOEVIA_LEGACY_UPSTREAM="http://127.0.0.1:$legacy" "$front" &
front_pid=$!
trap 'kill "$node_pid" "$front_pid" 2>/dev/null || true' INT TERM EXIT
# Either one exiting ends the run (the replayer then sees the server go away).
while kill -0 "$node_pid" 2>/dev/null && kill -0 "$front_pid" 2>/dev/null; do
  sleep 1
done
exit 1
