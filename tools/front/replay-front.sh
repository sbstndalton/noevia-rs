#!/bin/sh
# M1 replay gate: Node (noevia-core tools/contract-corpus/serve.cjs, the mocks the corpus was
# recorded with) on loopback UI_PORT+1000, noevia-server on UI_PORT in front of it, the way the
# web image runs them with NOEVIA_FRONT=rust. Used as tools/replay's --server-cmd:
#
#   replay run ... --base http://127.0.0.1:18021 --native-move-state --state-out native-state.json \
#     --server-cmd "exec sh tools/front/replay-front.sh <noevia-core> target/debug/noevia-server"
#
# <noevia-core> is the checkout at contracts/http/front-core.ref, the Core generation paired with
# this front (the release guard refuses mixed feature owners: this front serves the feature routes
# only once that Core's feature runtime has registered). core.ref stays the recording oracle that
# Node-direct reference runs and negative controls boot.
#
# replay sets UI_DATA_DIR / UI_PORT / UI_HOST and stops the whole process group afterwards.
set -eu
core="${1:?usage: replay-front.sh <noevia-core dir> <noevia-server binary>}"
front="${2:?usage: replay-front.sh <noevia-core dir> <noevia-server binary>}"
port="${UI_PORT:?UI_PORT is required}"
legacy=$((port + 1000))
# Node must see the origin the browser (here: the replayer) uses, which is the front's; so must the
# front, which always answers sign-in itself (M3, permanent since sbstndalton/noevia#1071; the
# Node is told so through NOEVIA_RUST_AUTH=1 and its confirmation, and serve.cjs then runs it as
# behind that front: read-only for the account tables).
origin="${PUBLIC_ORIGIN:-http://127.0.0.1:$port}"
# Like build/web-supervisor.sh: the front always owns sign-in (M3) and the project routes (M4: the
# images, a project's chat list, the free-chat list, the export and the project store), since
# sbstndalton/noevia#1071 retired NOEVIA_RUST_AUTH and NOEVIA_RUST_PROJECTS. A front that lacks one
# of those features is refused. Node is still told NOEVIA_RUST_AUTH=1 / NOEVIA_RUST_PROJECTS=1 and
# every confirmation, which cores from before the retirement needed to run as behind this front
# (the paired core ignores them).
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
# The front always owns sign-in (#1071): one without rust-auth is refused, whatever the caller set.
has_feature rust-auth || { echo "replay-front: $front has no rust-auth" >&2; exit 2; }
confirmed=1
for f in rust-projects rust-project-chats rust-free-chats rust-export project-store; do
  has_feature "$f" || { echo "replay-front: $front has no $f (the project routes are the front's, #1071)" >&2; exit 2; }
done
projects_confirmed=1
project_chats_confirmed=1
free_chats_confirmed=1
export_confirmed=1
# sbstndalton/noevia#1071: the front read routes have no switch of their own any more. Like
# build/web-supervisor.sh, the front must list every one of them (else this harness
# refuses before starting either process), and Node is told the front owns them.
if [ -n "$confirmed" ]; then
  for f in rust-usage rust-sampling-settings rust-reasoning-settings rust-routing-mode rust-routing-default rust-stats rust-storage-reads rust-storage-browse rust-chatgpt-status; do
    has_feature "$f" || { echo "replay-front: $front has no $f" >&2; exit 2; }
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
# sbstndalton/noevia#1264, permanent like build/web-supervisor.sh: Node writes projects.json and
# free-chats.json only through the front's loopback store, so both processes get UI_PORT+2000 and a
# secret made here for this run (never inherited).
store_port=$((port + 2000))
store_token="$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')"
[ "${#store_token}" -eq 64 ] || { echo "replay-front: could not make the project store secret" >&2; exit 2; }
# Like the web supervisor: one start id for both processes (sbstndalton/noevia#1071).
start_id="${NOEVIA_REPLAY_NATIVE_MOVE_START:?replay-front requires replay --native-move-state}"
[ "${#start_id}" -eq 32 ] && ! printf '%s' "$start_id" | grep -q '[^0-9a-f]' || { echo "replay-front: could not make the start id" >&2; exit 2; }
store_confirmed=1
# Context GET/POST are permanently native (#1071). Retired caller values cannot opt out.
has_feature rust-project-writes || { echo "replay-front: no rust-project-writes" >&2; exit 2; }
unset NOEVIA_RUST_PROJECT_WRITES NOEVIA_RUST_PROJECT_WRITES_CONFIRMED
UI_PORT="$legacy" PUBLIC_ORIGIN="$origin" NOEVIA_RUST_AUTH=1 NOEVIA_RUST_AUTH_CONFIRMED="$confirmed" NOEVIA_RUST_PROJECTS=1 \
  NOEVIA_RUST_PROJECTS_CONFIRMED="$projects_confirmed" NOEVIA_RUST_PROJECT_CHATS_CONFIRMED="$project_chats_confirmed" NOEVIA_RUST_FREE_CHATS_CONFIRMED="$free_chats_confirmed" NOEVIA_RUST_USAGE_CONFIRMED="$usage_confirmed" NOEVIA_RUST_SAMPLING_SETTINGS_CONFIRMED="$sampling_confirmed" NOEVIA_RUST_REASONING_SETTINGS_CONFIRMED="$reasoning_confirmed" \
  NOEVIA_RUST_ROUTING_MODE_CONFIRMED="$routing_confirmed" NOEVIA_RUST_STATS_CONFIRMED="$stats_confirmed" NOEVIA_RUST_EXPORT_CONFIRMED="$export_confirmed" \
  NOEVIA_RUST_STORAGE_READS_CONFIRMED="$storage_reads_confirmed" NOEVIA_RUST_STORAGE_BROWSE_CONFIRMED="$storage_browse_confirmed" NOEVIA_RUST_CHATGPT_STATUS_CONFIRMED="$chatgpt_status_confirmed" \
  NOEVIA_RUST_PROJECT_STORE_CONFIRMED="$store_confirmed" NOEVIA_PROJECT_STORE_PORT="$store_port" NOEVIA_PROJECT_STORE_TOKEN="$store_token" NOEVIA_START_ID="$start_id" \
  "$(dirname "$front")/corpus-launcher" front-serve "$core" &
node_pid=$!
PUBLIC_ORIGIN="$origin" NOEVIA_LEGACY_UPSTREAM="http://127.0.0.1:$legacy" NOEVIA_PROJECT_STORE_PORT="$store_port" NOEVIA_PROJECT_STORE_TOKEN="$store_token" NOEVIA_START_ID="$start_id" "$front" &
front_pid=$!
trap 'kill "$node_pid" "$front_pid" 2>/dev/null || true' INT TERM EXIT
# Either one exiting ends the run (the replayer then sees the server go away).
while kill -0 "$node_pid" 2>/dev/null && kill -0 "$front_pid" 2>/dev/null; do
  sleep 1
done
exit 1
