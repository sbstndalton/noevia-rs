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
# Like build/web-supervisor.sh: Node's write guard is confirmed only by a front that lists rust-auth
# (M3), and its projects.json coordination only by one that lists rust-projects (M4, which needs M3);
# its chat-list refusal only by one that also lists rust-project-chats (sbstndalton/noevia#1409).
unset NOEVIA_RUST_AUTH_CONFIRMED NOEVIA_RUST_PROJECTS_CONFIRMED NOEVIA_RUST_PROJECT_CHATS_CONFIRMED NOEVIA_RUST_USAGE_CONFIRMED NOEVIA_RUST_SAMPLING_SETTINGS_CONFIRMED NOEVIA_RUST_REASONING_SETTINGS_CONFIRMED NOEVIA_RUST_ROUTING_MODE_CONFIRMED
confirmed=""
projects_confirmed=""
project_chats_confirmed=""
usage_confirmed=""
sampling_confirmed=""
reasoning_confirmed=""
routing_confirmed=""
# Capture once on the first needed check, before confirming ownership.
features=""
features_loaded=""
has_feature() {
  if [ -z "$features_loaded" ]; then
    features=$("$front" --features) || { echo "replay-front: $front capability probe failed" >&2; exit 2; }
    features_loaded=1
  fi
  grep -qx "$1" <<EOF
$features
EOF
}
if [ "${NOEVIA_RUST_AUTH:-}" = 1 ]; then
  has_feature rust-auth || { echo "replay-front: $front has no rust-auth" >&2; exit 2; }
  confirmed=1
fi
if [ "${NOEVIA_RUST_PROJECTS:-}" = 1 ]; then
  [ -n "$confirmed" ] || { echo "replay-front: NOEVIA_RUST_PROJECTS=1 needs NOEVIA_RUST_AUTH=1" >&2; exit 2; }
  has_feature rust-projects || { echo "replay-front: $front has no rust-projects" >&2; exit 2; }
  projects_confirmed=1
  # A front without the chat-list slice is a supported pairing: Node keeps that route.
  if has_feature rust-project-chats; then project_chats_confirmed=1; fi
fi
if [ "${NOEVIA_RUST_USAGE:-}" = 1 ]; then
  [ -n "$confirmed" ] || { echo "replay-front: usage needs native auth" >&2; exit 2; }
  has_feature rust-usage || { echo "replay-front: no rust-usage" >&2; exit 2; }
  usage_confirmed=1
fi
if [ "${NOEVIA_RUST_SAMPLING_SETTINGS:-}" = 1 ]; then
  [ -n "$confirmed" ] || { echo "replay-front: sampling settings need native auth" >&2; exit 2; }
  has_feature rust-sampling-settings || { echo "replay-front: no rust-sampling-settings" >&2; exit 2; }
  sampling_confirmed=1
fi
if [ "${NOEVIA_RUST_REASONING_SETTINGS:-}" = 1 ]; then
  [ -n "$confirmed" ] || { echo "replay-front: reasoning default needs native auth" >&2; exit 2; }
  has_feature rust-reasoning-settings || { echo "replay-front: no rust-reasoning-settings" >&2; exit 2; }
  reasoning_confirmed=1
fi
if [ "${NOEVIA_RUST_ROUTING_MODE:-}" = 1 ]; then
  [ -n "$confirmed" ] || { echo "replay-front: routing settings need native auth" >&2; exit 2; }
  has_feature rust-routing-mode || { echo "replay-front: no rust-routing-mode" >&2; exit 2; }
  routing_confirmed=1
fi
UI_PORT="$legacy" PUBLIC_ORIGIN="$origin" NOEVIA_RUST_AUTH_CONFIRMED="$confirmed" \
  NOEVIA_RUST_PROJECTS_CONFIRMED="$projects_confirmed" NOEVIA_RUST_PROJECT_CHATS_CONFIRMED="$project_chats_confirmed" NOEVIA_RUST_USAGE_CONFIRMED="$usage_confirmed" NOEVIA_RUST_SAMPLING_SETTINGS_CONFIRMED="$sampling_confirmed" NOEVIA_RUST_REASONING_SETTINGS_CONFIRMED="$reasoning_confirmed" \
  NOEVIA_RUST_ROUTING_MODE_CONFIRMED="$routing_confirmed" \
  "$(dirname "$front")/corpus-launcher" serve "$core" &
node_pid=$!
# CI's M4 negative control only: Node is told the front owns the project image routes, the front is
# started without the switch, so those writes must reach Node and be refused.
if [ "${REPLAY_FRONT_CONTROL_NODE_ONLY_ROUTING_MODE:-}" = 1 ]; then
  # Valid native auth continues while confirmed Node refuses every routing configuration method.
  PUBLIC_ORIGIN="$origin" NOEVIA_LEGACY_UPSTREAM="http://127.0.0.1:$legacy" env -u NOEVIA_RUST_ROUTING_MODE "$front" &
elif [ "${REPLAY_FRONT_CONTROL_NODE_ONLY_REASONING_SETTINGS:-}" = 1 ]; then
  # Valid native identity remains; Node is still confirmed and must refuse the owned PUT.
  PUBLIC_ORIGIN="$origin" NOEVIA_LEGACY_UPSTREAM="http://127.0.0.1:$legacy" env -u NOEVIA_RUST_REASONING_SETTINGS "$front" &
elif [ "${REPLAY_FRONT_CONTROL_NODE_ONLY_SAMPLING_SETTINGS:-}" = 1 ]; then
  # Test-only omission: valid native auth continues; confirmed Node sampling must refuse.
  PUBLIC_ORIGIN="$origin" NOEVIA_LEGACY_UPSTREAM="http://127.0.0.1:$legacy" env -u NOEVIA_RUST_SAMPLING_SETTINGS "$front" &
elif [ "${REPLAY_FRONT_CONTROL_NODE_ONLY_USAGE:-}" = 1 ]; then
  # Test-only omission: Node reporting stays confirmed/refused, auth remains native.
  PUBLIC_ORIGIN="$origin" NOEVIA_LEGACY_UPSTREAM="http://127.0.0.1:$legacy" env -u NOEVIA_RUST_USAGE "$front" &
elif [ "${REPLAY_FRONT_CONTROL_NODE_ONLY_PROJECTS:-}" = 1 ]; then
  PUBLIC_ORIGIN="$origin" NOEVIA_LEGACY_UPSTREAM="http://127.0.0.1:$legacy" env -u NOEVIA_RUST_PROJECTS "$front" &
else
  PUBLIC_ORIGIN="$origin" NOEVIA_LEGACY_UPSTREAM="http://127.0.0.1:$legacy" "$front" &
fi
front_pid=$!
trap 'kill "$node_pid" "$front_pid" 2>/dev/null || true' INT TERM EXIT
# Either one exiting ends the run (the replayer then sees the server go away).
while kill -0 "$node_pid" 2>/dev/null && kill -0 "$front_pid" 2>/dev/null; do
  sleep 1
done
exit 1
