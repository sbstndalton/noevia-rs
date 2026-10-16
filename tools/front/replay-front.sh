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
# its chat-list refusal only by one that also lists rust-project-chats (sbstndalton/noevia#1409),
# and its free-chat coordination and refusal only by one that lists rust-free-chats.
unset NOEVIA_RUST_AUTH_CONFIRMED NOEVIA_RUST_PROJECTS_CONFIRMED NOEVIA_RUST_PROJECT_CHATS_CONFIRMED NOEVIA_RUST_FREE_CHATS_CONFIRMED NOEVIA_RUST_USAGE_CONFIRMED NOEVIA_RUST_SAMPLING_SETTINGS_CONFIRMED NOEVIA_RUST_REASONING_SETTINGS_CONFIRMED NOEVIA_RUST_ROUTING_MODE_CONFIRMED NOEVIA_RUST_STATS_CONFIRMED NOEVIA_RUST_EXPORT_CONFIRMED NOEVIA_RUST_STORAGE_READS_CONFIRMED NOEVIA_RUST_STORAGE_BROWSE_CONFIRMED NOEVIA_RUST_CHATGPT_STATUS_CONFIRMED NOEVIA_RUST_PROJECT_WRITES_CONFIRMED NOEVIA_RUST_PROJECT_STORE_CONFIRMED NOEVIA_PROJECT_STORE_PORT NOEVIA_PROJECT_STORE_TOKEN
confirmed=""
store_confirmed=""
projects_confirmed=""
project_chats_confirmed=""
free_chats_confirmed=""
usage_confirmed=""
sampling_confirmed=""
reasoning_confirmed=""
routing_confirmed=""
stats_confirmed=""
storage_reads_confirmed=""
storage_browse_confirmed=""
export_confirmed=""
chatgpt_status_confirmed=""
project_writes_confirmed=""
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
  # Likewise the free-chat slice.
  if has_feature rust-free-chats; then free_chats_confirmed=1; fi
fi
# sbstndalton/noevia#1071: the front read routes have no switch of their own any more. Like
# build/web-supervisor.sh, a front with native auth must list every one of them (else this harness
# refuses before starting either process), and the core.ref oracle is told the front owns them.
# The export follows the projects switch (it reads the free-chat list from disk).
if [ -n "$confirmed" ]; then
  for f in rust-usage rust-sampling-settings rust-reasoning-settings rust-routing-mode rust-routing-default rust-stats rust-storage-reads rust-storage-browse rust-chatgpt-status; do
    has_feature "$f" || { echo "replay-front: NOEVIA_RUST_AUTH=1 needs a front with $f" >&2; exit 2; }
  done
  usage_confirmed=1
  sampling_confirmed=1
  reasoning_confirmed=1
  routing_confirmed=1
  stats_confirmed=1
  storage_reads_confirmed=1
  storage_browse_confirmed=1
  chatgpt_status_confirmed=1
fi
if [ -n "$projects_confirmed" ]; then
  { [ -n "$free_chats_confirmed" ] && has_feature rust-export; } || { echo "replay-front: NOEVIA_RUST_PROJECTS=1 needs a front with rust-export and rust-free-chats" >&2; exit 2; }
  export_confirmed=1
fi
# sbstndalton/noevia#1264, permanent like build/web-supervisor.sh: with projects confirmed Node writes
# projects.json and free-chats.json only through the front's loopback store, so both processes get
# UI_PORT+2000 and a secret made here for this run (never inherited).
store_port="" store_token=""
if [ -n "$projects_confirmed" ]; then
  has_feature project-store || { echo "replay-front: NOEVIA_RUST_PROJECTS=1 needs a front with project-store" >&2; exit 2; }
  store_port=$((port + 2000))
  store_token="$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')"
  [ "${#store_token}" -eq 64 ] || { echo "replay-front: could not make the project store secret" >&2; exit 2; }
  store_confirmed=1
fi
# sbstndalton/noevia#1264: the project writes ported so far; Node refuses them (core
# rust-projects.cjs projectWritesEnabledFrom) only once the front lists rust-project-writes, over
# confirmed projects.
if [ "${NOEVIA_RUST_PROJECT_WRITES:-}" = 1 ]; then
  [ -n "$projects_confirmed" ] || { echo "replay-front: project writes need NOEVIA_RUST_PROJECTS=1" >&2; exit 2; }
  has_feature rust-project-writes || { echo "replay-front: no rust-project-writes" >&2; exit 2; }
  project_writes_confirmed=1
fi
UI_PORT="$legacy" PUBLIC_ORIGIN="$origin" NOEVIA_RUST_AUTH_CONFIRMED="$confirmed" \
  NOEVIA_RUST_PROJECTS_CONFIRMED="$projects_confirmed" NOEVIA_RUST_PROJECT_CHATS_CONFIRMED="$project_chats_confirmed" NOEVIA_RUST_FREE_CHATS_CONFIRMED="$free_chats_confirmed" NOEVIA_RUST_USAGE_CONFIRMED="$usage_confirmed" NOEVIA_RUST_SAMPLING_SETTINGS_CONFIRMED="$sampling_confirmed" NOEVIA_RUST_REASONING_SETTINGS_CONFIRMED="$reasoning_confirmed" \
  NOEVIA_RUST_ROUTING_MODE_CONFIRMED="$routing_confirmed" NOEVIA_RUST_STATS_CONFIRMED="$stats_confirmed" NOEVIA_RUST_EXPORT_CONFIRMED="$export_confirmed" \
  NOEVIA_RUST_STORAGE_READS_CONFIRMED="$storage_reads_confirmed" NOEVIA_RUST_STORAGE_BROWSE_CONFIRMED="$storage_browse_confirmed" NOEVIA_RUST_CHATGPT_STATUS_CONFIRMED="$chatgpt_status_confirmed" NOEVIA_RUST_PROJECT_WRITES_CONFIRMED="$project_writes_confirmed" \
  NOEVIA_RUST_PROJECT_STORE_CONFIRMED="$store_confirmed" NOEVIA_PROJECT_STORE_PORT="$store_port" NOEVIA_PROJECT_STORE_TOKEN="$store_token" \
  "$(dirname "$front")/corpus-launcher" serve "$core" &
node_pid=$!
# CI's M4 negative control only: Node is told the front owns the project image routes, the front is
# started without the switch, so those writes must reach Node and be refused.
if [ "${REPLAY_FRONT_CONTROL_NODE_ONLY_PROJECTS:-}" = 1 ]; then
  PUBLIC_ORIGIN="$origin" NOEVIA_LEGACY_UPSTREAM="http://127.0.0.1:$legacy" env -u NOEVIA_RUST_PROJECTS -u NOEVIA_PROJECT_STORE_PORT -u NOEVIA_PROJECT_STORE_TOKEN "$front" &
else
  PUBLIC_ORIGIN="$origin" NOEVIA_LEGACY_UPSTREAM="http://127.0.0.1:$legacy" NOEVIA_PROJECT_STORE_PORT="$store_port" NOEVIA_PROJECT_STORE_TOKEN="$store_token" "$front" &
fi
front_pid=$!
trap 'kill "$node_pid" "$front_pid" 2>/dev/null || true' INT TERM EXIT
# Either one exiting ends the run (the replayer then sees the server go away).
while kill -0 "$node_pid" 2>/dev/null && kill -0 "$front_pid" 2>/dev/null; do
  sleep 1
done
exit 1
