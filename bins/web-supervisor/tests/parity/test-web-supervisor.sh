#!/usr/bin/env bash
# Tests build/web-supervisor.sh (the web image entrypoint) with stub `node` and noevia-server
# processes. Needs bash >= 4.3 (wait -n), like the image; skips on macOS's bash 3.2. CI runs it on
# Linux.
#
# sbstndalton/noevia#1071: sign-in and the account, the front read routes and the project routes and
# store are the Rust front's permanently, so the supervisor starts nothing unless NOEVIA_FRONT=rust
# and a noevia-server whose --features lists rust-auth and every front route feature.
# NOEVIA_RUST_AUTH and NOEVIA_RUST_PROJECTS are retired: whatever their value, they only warn and
# reach neither process.
set -u
here="$(cd -- "$(dirname -- "$0")" && pwd)"
sup="$here/web-supervisor.sh"
# noevia-rs bins/web-supervisor: the same suite drives the native supervisor when WEB_SUPERVISOR_BIN
# names it (tests/parity.rs), and the bash reference copied beside this file otherwise.
if [ -n "${WEB_SUPERVISOR_BIN:-}" ]; then sup_cmd=("$WEB_SUPERVISOR_BIN"); else sup_cmd=("${sup_cmd[@]}"); fi
if [ "${BASH_VERSINFO[0]}" -lt 4 ] || { [ "${BASH_VERSINFO[0]}" -eq 4 ] && [ "${BASH_VERSINFO[1]}" -lt 3 ]; }; then
  echo "skip - bash ${BASH_VERSION} has no wait -n (the image's bash does)"; exit 0
fi
work="$(mktemp -d "${TMPDIR:-/tmp}/web-supervisor-test.XXXXXX")"
trap 'rm -rf "$work"' EXIT
fails=0; passes=0
check() { if eval "$2"; then passes=$((passes+1)); echo "ok - $1"; else fails=$((fails+1)); echo "FAIL - $1"; fi; }

# The features build/web-supervisor.sh requires (FRONT_ROUTE_FEATURES), and a front that has them.
ROUTE_FEATURES="rust-chatgpt-status rust-storage-reads rust-storage-browse rust-export rust-free-chats rust-usage rust-stats rust-routing-default rust-sampling-settings rust-reasoning-settings rust-routing-mode rust-projects rust-project-chats rust-project-writes project-store"
BASE_FEATURES="code-net-guard rust-auth $ROUTE_FEATURES"
RETIRED="NOEVIA_RUST_AUTH NOEVIA_RUST_USAGE NOEVIA_RUST_SAMPLING_SETTINGS NOEVIA_RUST_REASONING_SETTINGS NOEVIA_RUST_ROUTING_MODE NOEVIA_RUST_ROUTING_DEFAULT NOEVIA_RUST_STATS NOEVIA_RUST_EXPORT NOEVIA_RUST_CHATGPT_STATUS NOEVIA_RUST_STORAGE_READS NOEVIA_RUST_STORAGE_BROWSE NOEVIA_RUST_PROJECT_WRITES NOEVIA_RUST_PROJECTS NOEVIA_RUST_PROJECT_CHATS NOEVIA_RUST_FREE_CHATS NOEVIA_RUST_PROJECT_STORE"

real_node="$(command -v node)"
mkdir -p "$work/bin"
# Each stub records its environment, then lives until killed or until its "die" file appears.
stub() {
  cat > "$1" <<STUB
#!/usr/bin/env bash
name=$2
if [ "\${1:-}" = -e ]; then exec "$real_node" "\$@"; fi
if [ "\${1:-}" = --features ]; then printf '%s\\n' \${STUB_FEATURES-$BASE_FEATURES}; exit 0; fi
[ "\$name" = node ] && [ -n "\${STUB_NODE_DELAY:-}" ] && sleep "\$STUB_NODE_DELAY"
echo "\$name UI_HOST=\${UI_HOST:-} UI_PORT=\${UI_PORT:-} UPSTREAM=\${NOEVIA_LEGACY_UPSTREAM:-} ARGS=\$* CCONF=\${NOEVIA_RUST_PROJECT_CHATS_CONFIRMED:-} FCONF=\${NOEVIA_RUST_FREE_CHATS_CONFIRMED:-} PCONF=\${NOEVIA_RUST_PROJECTS_CONFIRMED:-} CONF=\${NOEVIA_RUST_AUTH_CONFIRMED:-}" >> "$work/log"
retired=""
for v in $RETIRED; do c="\${v}_CONFIRMED"; [ -n "\${!c:-}" ] && retired="\$retired \$c"; done
echo "\$name RETIRED=\$retired" >> "$work/log"
echo "\$name PROJECTS=\${NOEVIA_RUST_PROJECTS-unset}" >> "$work/log"
echo "\$name WCONF=\${NOEVIA_RUST_PROJECT_WRITES_CONFIRMED:-} WRITES=\${NOEVIA_RUST_PROJECT_WRITES-unset}" >> "$work/log"
echo "\$name SSTORE=\${NOEVIA_RUST_PROJECT_STORE_CONFIRMED:-} SPORT=\${NOEVIA_PROJECT_STORE_PORT:-} STOKLEN=\${#NOEVIA_PROJECT_STORE_TOKEN}" >> "$work/log"
echo "\$name AUTH=\${NOEVIA_RUST_AUTH-unset}" >> "$work/log"
echo "\$name LCLAIM=\${NOEVIA_LEGACY_CLAIM_BIN:-}" >> "$work/log"
echo "\$name PID=\$\$" >> "$work/log"
echo "\$name START=\${NOEVIA_START_ID-unset}" >> "$work/log"
echo "\$name KCONF=\${NOEVIA_FRONT_CLIENT_IP:-}" >> "$work/log"
trap 'echo "\$name TERM" >> "$work/log"; if [ "\$name" = front ] && [ "\${STUB_FRONT_STUCK:-}" = 1 ]; then while :; do sleep 0.05; done; fi; if [ "\$name" = front ] && [ "\${STUB_FRONT_DRAIN:-}" = 1 ]; then sleep 0.15; echo "front JOINED" >> "$work/log"; fi; if [ "\$name" = node ] && [ "\${STUB_NODE_DRAIN:-}" = 1 ]; then sleep 3; echo "node HOOKS-DONE" >> "$work/log"; fi; exit 0' TERM
# The supervisor TERMs the survivor as soon as one child exits. A front told to die at once would
# race Node's startup logging (Node killed mid-write, its later lines lost: #1822), so a dying
# front waits (bounded) until Node's last startup line is in the log.
if [ "\$name" = front ] && [ -f "$work/die-front" ]; then
  for _ in \$(seq 1 200); do grep -q "^node KCONF=" "$work/log" 2>/dev/null && break; sleep 0.05; done
fi
for _ in \$(seq 1 300); do
  [ -f "$work/die-\$name" ] && { echo "\$name died" >> "$work/log"; exit 3; }
  sleep 0.05
done
STUB
  chmod +x "$1"
}
stub "$work/bin/node" node
stub "$work/noevia-server" front
export PATH="$work/bin:$PATH"
# The live deployment; a test overrides any of it by naming the key again (env: last one wins).
run() { (cd "$work" && env NOEVIA_FRONT=rust NOEVIA_SERVER_BIN="$work/noevia-server" "$@" "${sup_cmd[@]}"); }
reset() { : > "$work/log"; rm -f "$work"/die-* "$work/rc"; }
wait_log() { for _ in $(seq 1 100); do grep -q "$1" "$work/log" 2>/dev/null && return 0; sleep 0.05; done; return 1; }
without() { local out="" f; for f in $BASE_FEATURES; do [ "$f" = "$1" ] || out="$out $f"; done; echo "$out"; }

# --- Fail closed before either process starts -----------------------------------------------
reset
touch "$work/die-front"
(cd "$work" && env -u NOEVIA_FRONT NOEVIA_SERVER_BIN="$work/noevia-server" "${sup_cmd[@]}") >/dev/null 2>"$work/err"; rc=$?
check "an unset NOEVIA_FRONT starts as rust (#2094), not refused as node" '[ $rc -eq 3 ] && ! grep -q "NOEVIA_FRONT" "$work/err" && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
reset; touch "$work/die-front"
run NOEVIA_FRONT= >/dev/null 2>"$work/err"; rc=$?
check "an empty NOEVIA_FRONT also starts as rust" '[ $rc -eq 3 ] && ! grep -q "NOEVIA_FRONT" "$work/err" && grep -q "^front " "$work/log"'
reset
run NOEVIA_FRONT=node >/dev/null 2>"$work/err"; rc=$?
check "NOEVIA_FRONT=node is refused, nothing started" '[ $rc -eq 2 ] && grep -q "#1071" "$work/err" && [ ! -s "$work/log" ]'
reset
run NOEVIA_FRONT=bogus >/dev/null 2>"$work/err"; rc=$?
check "an unknown NOEVIA_FRONT is refused" '[ $rc -eq 2 ] && grep -q "must be rust" "$work/err" && [ ! -s "$work/log" ]'
# A Node front cannot be selected by turning either retired switch off.
for value in "" 0 1; do
  reset
  run NOEVIA_FRONT=node NOEVIA_RUST_AUTH="$value" NOEVIA_RUST_PROJECTS="$value" >/dev/null 2>"$work/err"; rc=$?
  check "NOEVIA_FRONT=node with NOEVIA_RUST_AUTH/NOEVIA_RUST_PROJECTS='$value' is refused, nothing started" '[ $rc -eq 2 ] && grep -q "NOEVIA_FRONT=node is no longer supported" "$work/err" && [ ! -s "$work/log" ]'
done
reset
run NOEVIA_SERVER_BIN="$work/missing" >/dev/null 2>"$work/err"; rc=$?
check "no noevia-server binary is refused, nothing started" '[ $rc -eq 2 ] && grep -q "no noevia-server" "$work/err" && [ ! -s "$work/log" ]'
reset
run STUB_FEATURES="$(without code-net-guard)" COWORK_CODE_NET_ADDR=egress >/dev/null 2>"$work/err"; rc=$?
check "COWORK_CODE_NET_ADDR with a front without the guard is refused" '[ $rc -eq 2 ] && grep -q "no code-net-guard" "$work/err" && [ ! -s "$work/log" ]'
reset
run STUB_FEATURES="$(without rust-auth)" >/dev/null 2>"$work/err"; rc=$?
check "a front without rust-auth is refused, nothing started" '[ $rc -eq 2 ] && grep -q "no rust-auth" "$work/err" && [ ! -s "$work/log" ]'
reset
run STUB_FEATURES= >/dev/null 2>"$work/err"; rc=$?
check "a front whose --features prints nothing is refused" '[ $rc -eq 2 ] && [ ! -s "$work/log" ]'
# #1071: every front route feature is required; a missing one fails closed and is named.
for feature in $ROUTE_FEATURES; do
  reset
  run STUB_FEATURES="$(without "$feature")" >/dev/null 2>"$work/err"; rc=$?
  check "a front without $feature is refused, nothing started" '[ $rc -eq 2 ] && grep -q "lacks the front route feature(s): $feature;" "$work/err" && [ ! -s "$work/log" ]'
done
reset
run STUB_FEATURES="code-net-guard rust-auth rust-usage" >/dev/null 2>"$work/err"; rc=$?
check "every missing feature is listed at once" '[ $rc -eq 2 ] && grep -q "rust-chatgpt-status rust-storage-reads rust-storage-browse rust-export rust-free-chats rust-stats .* rust-projects rust-project-chats rust-project-writes project-store;" "$work/err" && [ ! -s "$work/log" ]'
# A manually mixed installed core/front pair fails before either process starts. The
# core result comes from its exported module at runtime, not an inherited environment value.
reset
run STUB_FEATURES="$BASE_FEATURES rust-web-address-read" >/dev/null 2>"$work/err"; rc=$?
check "web-address: new front over old core is refused before launch" '[ $rc -eq 2 ] && grep -q "web-address GET ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
mkdir -p "$work/server"
printf "%s\n" "module.exports = { WEB_ADDRESS_READ_OWNER: 'noevia-web-address-read-rust/1' };" > "$work/server/rust-web-address.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "web-address: ported core over old front is refused before launch" '[ $rc -eq 2 ] && grep -q "web-address GET ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-web-address-read" >/dev/null 2>"$work/err"; rc=$?
check "web-address: matching exported core and compiled front start together" '[ $rc -eq 3 ] && grep -q "^node " "$work/log" && grep -q "^front " "$work/log"'
printf "%s\n" "module.exports = { WEB_ADDRESS_READ_OWNER: 'noevia-web-address-read-rust/1', WEB_ADDRESS_WRITE_OWNER: 'noevia-web-address-write-rust/1' };" > "$work/server/rust-web-address.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-web-address-read" >/dev/null 2>"$work/err"; rc=$?
check "web-address POST: ported core over read-only front is refused before launch" '[ $rc -eq 2 ] && grep -q "web-address POST ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-web-address-read rust-web-address-write" >/dev/null 2>"$work/err"; rc=$?
check "web-address POST: matching writer pair starts together" '[ $rc -eq 3 ] && grep -q "^node " "$work/log" && grep -q "^front " "$work/log"'
printf "%s\n" "module.exports = { WEB_ADDRESS_READ_OWNER: 'noevia-web-address-read-rust/1' };" > "$work/server/rust-web-address.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-web-address-read rust-web-address-write" >/dev/null 2>"$work/err"; rc=$?
check "web-address POST: writer front over read-only core is refused before launch" '[ $rc -eq 2 ] && grep -q "web-address POST ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
printf "%s\n" "module.exports = { WEB_ADDRESS_READ_OWNER: 'wrong' };" > "$work/server/rust-web-address.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-web-address-read" WEB_ADDRESS_READ_OWNER=noevia-web-address-read-rust/1 >/dev/null 2>"$work/err"; rc=$?
check "web-address: inherited owner value cannot spoof a wrong core" '[ $rc -eq 2 ] && grep -q "web-address GET ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
rm -f "$work/server/rust-web-address.cjs"
# sbstndalton/noevia#2303: the calibration job (state file, boot recovery, hold) moves as one owner, one
# generation (2: Core decides every calibration load first).
reset
run STUB_FEATURES="$BASE_FEATURES rust-calibration-worker-v2" >/dev/null 2>"$work/err"; rc=$?
check "calibration worker: native front over a Core that still runs the job is refused before launch" '[ $rc -eq 2 ] && grep -q "calibration worker ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
printf "%s\n" "module.exports = { CALIBRATION_WORKER_OWNER: 'noevia-calibration-worker-rust/2' };" > "$work/server/rust-calibration-routes.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "calibration worker: Core that stopped running the job over an old front is refused before launch" '[ $rc -eq 2 ] && grep -q "calibration worker ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset
run STUB_FEATURES="$BASE_FEATURES rust-calibration-worker" >/dev/null 2>"$work/err"; rc=$?
check "calibration worker: a generation-1 front over a generation-2 Core is refused before launch" '[ $rc -eq 2 ] && grep -q "calibration worker ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
printf "%s\n" "module.exports = { CALIBRATION_WORKER_OWNER: 'noevia-calibration-worker-rust/1' };" > "$work/server/rust-calibration-routes.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-calibration-worker-v2" >/dev/null 2>"$work/err"; rc=$?
check "calibration worker: a generation-2 front over a generation-1 Core is refused before launch" '[ $rc -eq 2 ] && grep -q "calibration worker ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
printf "%s\n" "module.exports = { CALIBRATION_WORKER_OWNER: 'noevia-calibration-worker-rust/2' };" > "$work/server/rust-calibration-routes.cjs"
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-calibration-worker-v2" >/dev/null 2>"$work/err"; rc=$?
check "calibration worker: matching exported Core and compiled front start together" '[ $rc -eq 3 ] && grep -q "^node " "$work/log" && grep -q "^front " "$work/log"'
printf "%s\n" "module.exports = { CALIBRATION_WORKER_OWNER: 'wrong' };" > "$work/server/rust-calibration-routes.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-calibration-worker-v2" CALIBRATION_WORKER_OWNER=noevia-calibration-worker-rust/2 >/dev/null 2>"$work/err"; rc=$?
check "calibration worker: inherited owner value cannot spoof a wrong Core" '[ $rc -eq 2 ] && grep -q "calibration worker ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
rm -f "$work/server/rust-calibration-routes.cjs"

# The budget watchdog moves as one owner.
reset
run STUB_FEATURES="$BASE_FEATURES rust-budget-watch-v3" >/dev/null 2>"$work/err"; rc=$?
check "budget watch: native front over a Core that still runs the job is refused before launch" '[ $rc -eq 2 ] && grep -q "budget watch ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
printf "%s\n" "module.exports = { BUDGET_WATCH_OWNER: 'noevia-budget-watch-rust/3' };" > "$work/server/rust-budget-watch.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "budget watch: Core that stopped running the job over an old front is refused before launch" '[ $rc -eq 2 ] && grep -q "budget watch ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-budget-watch-v3" >/dev/null 2>"$work/err"; rc=$?
check "budget watch: matching exported Core and compiled front start together" '[ $rc -eq 3 ] && grep -q "^node " "$work/log" && grep -q "^front " "$work/log"'
reset
run STUB_FEATURES="$BASE_FEATURES rust-budget-watch-v2" >/dev/null 2>"$work/err"; rc=$?
check "budget watch: v2 front over v3 Core is refused" '[ $rc -eq 2 ]'
printf "%s\n" "module.exports = { BUDGET_WATCH_OWNER: 'noevia-budget-watch-rust/2' };" > "$work/server/rust-budget-watch.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-budget-watch-v3" >/dev/null 2>"$work/err"; rc=$?
check "budget watch: v3 front over v2 Core is refused" '[ $rc -eq 2 ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-budget-watch-v2" >/dev/null 2>"$work/err"; rc=$?
check "budget watch: matched native v2 remains a rollback pair" '[ $rc -eq 3 ]'
printf "%s\n" "module.exports = { BUDGET_WATCH_OWNER: 'noevia-budget-watch-rust/3' };" > "$work/server/rust-budget-watch.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-budget-watch" >/dev/null 2>"$work/err"; rc=$?
check "budget watch: unfenced v1 front over v3 Core is refused" '[ $rc -eq 2 ] && [ ! -s "$work/log" ]'
printf "%s\n" "module.exports = { BUDGET_WATCH_OWNER: 'noevia-budget-watch-rust/1' };" > "$work/server/rust-budget-watch.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-budget-watch-v3" >/dev/null 2>"$work/err"; rc=$?
check "budget watch: v3 front over unfenced v1 Core is refused" '[ $rc -eq 2 ] && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-budget-watch" >/dev/null 2>"$work/err"; rc=$?
check "budget watch: matched native v1 pair remains a rollback pair" '[ $rc -eq 3 ]'
printf "%s\n" "module.exports = { BUDGET_WATCH_OWNER: 'wrong' };" > "$work/server/rust-budget-watch.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-budget-watch-v3" BUDGET_WATCH_OWNER=noevia-budget-watch-rust/3 >/dev/null 2>"$work/err"; rc=$?
check "budget watch: inherited owner value cannot spoof a wrong Core" '[ $rc -eq 2 ] && grep -q "budget watch ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
rm -f "$work/server/rust-budget-watch.cjs"

# Leftover native pending, recorded and damaged fences do not gate legacy rollback.
mkdir -p "$work/data/budget-watch-synthetic"
printf '%s\n' '{"model":"pending","recorded":false}' > "$work/data/budget-watch-synthetic/pending.json"
printf '%s\n' '{"model":"recorded","recorded":true}' > "$work/data/budget-watch-synthetic/recorded.json"
printf '%s\n' 'damaged' > "$work/data/budget-watch-synthetic/unknown.json.damaged-1"
reset; touch "$work/die-front"
run UI_DATA_DIR="$work/data" >/dev/null 2>"$work/err"; rc=$?
check "budget watch: legacy rollback starts with leftover fence files present" '[ $rc -eq 3 ] && grep -q "^node " "$work/log" && grep -q "^front " "$work/log" && [ -f "$work/data/budget-watch-synthetic/pending.json" ] && [ -f "$work/data/budget-watch-synthetic/recorded.json" ]'

reset
run UI_PORT=65000 >/dev/null 2>&1; rc=$?
check "a UI_PORT with no room for +1000 is refused" '[ $rc -eq 2 ] && [ ! -s "$work/log" ]'

# Compiled/exported ownership, both directions and at restart; env cannot forge an export.
reset
run STUB_FEATURES="$BASE_FEATURES rust-diary-backup-worker" >/dev/null 2>"$work/err"; rc=$?
check "diary backup: new front over old Core refused before launch" '[ $rc -eq 2 ] && grep -q "diary backup worker ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
printf "%s\n" "module.exports = { DIARY_BACKUP_WORKER_OWNER: 'noevia-diary-backup-worker-rust/1' };" > "$work/server/rust-diary.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "diary backup: new Core over old front refused" '[ $rc -eq 2 ] && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-diary-backup-worker" >/dev/null 2>"$work/err"; rc=$?
check "diary backup: paired worker starts" '[ $rc -eq 3 ] && grep -q "^node " "$work/log" && grep -q "^front " "$work/log"'
printf "%s\n" "module.exports = { DIARY_BACKUP_WORKER_OWNER: 'wrong' };" > "$work/server/rust-diary.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-diary-backup-worker" DIARY_BACKUP_WORKER_OWNER=noevia-diary-backup-worker-rust/1 >/dev/null 2>"$work/err"; rc=$?
check "diary backup: env owner cannot spoof installed Core on restart" '[ $rc -eq 2 ] && [ ! -s "$work/log" ]'
rm -f "$work/server/rust-diary.cjs"

# Source-journal pairing probes the actual installed Node export and Rust binary --features.
mkdir -p "$work/server"
cat > "$work/server/project-store-client.cjs" <<'JS'
const SOURCE_JOBS_CLIENT_MARKER = 'noevia-source-jobs-client/1';
module.exports = { SOURCE_JOBS_CLIENT_MARKER };
JS
reset
run >/dev/null 2>"$work/err"; rc=$?
check "new core with old Rust source store is refused before either starts" '[ $rc -eq 2 ] && grep -q "requires noevia-source-jobs-store/1" "$work/err" && [ ! -s "$work/log" ]'
reset
printf '%s\n' "function sourceJobsFor(user) { return { user }; }" "module.exports = { sourceJobsFor };" > "$work/server/project-store-client.cjs"
run >/dev/null 2>"$work/err"; rc=$?
check "ported core with stripped marker is refused beside old Rust" '[ $rc -eq 2 ] && grep -q "does not export its canonical capability" "$work/err" && [ ! -s "$work/log" ]'
reset
printf '%s\n' "/* SOURCE_JOBS_CLIENT_MARKER */" "module.exports = {};" > "$work/server/project-store-client.cjs"
run STUB_FEATURES="$BASE_FEATURES noevia-source-jobs-store/1" >/dev/null 2>"$work/err"; rc=$?
check "comment-only core marker cannot satisfy runtime capability" '[ $rc -eq 2 ] && grep -q "does not export its canonical capability" "$work/err" && [ ! -s "$work/log" ]'
reset
rm -f "$work/server/project-store-client.cjs"
run STUB_FEATURES="$BASE_FEATURES noevia-source-jobs-store/1" >/dev/null 2>"$work/err"; rc=$?
check "new Rust source store with old core is refused before either starts" '[ $rc -eq 2 ] && grep -q "requires a paired core" "$work/err" && [ ! -s "$work/log" ]'
cat > "$work/server/project-store-client.cjs" <<'JS'
const SOURCE_JOBS_CLIENT_MARKER = 'noevia-source-jobs-client/1';
module.exports = { SOURCE_JOBS_CLIENT_MARKER };
JS
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES noevia-source-jobs-store/1" >/dev/null 2>"$work/err"; rc=$?
check "matching source client and store start both processes" '[ $rc -eq 3 ] && grep -q "^node UI_HOST=" "$work/log" && grep -q "^front UI_HOST=" "$work/log"'
rm -f "$work/server/project-store-client.cjs"
# #2218: a rust-research Core and front pair up.
printf '%s\n' "  if (require('./rust-research.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES noevia-research-jobs-store/1 rust-research" >/dev/null 2>"$work/err"; rc=$?
check "rust-research pair with the store compiled and no Core client starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
reset
run STUB_FEATURES="$BASE_FEATURES noevia-research-jobs-store/1" >/dev/null 2>"$work/err"; rc=$?
check "rust-research Core with an old front that only has the store is refused" '[ $rc -eq 2 ] && grep -q "deep research route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
rm -f "$work/server/index.cjs"
# --- The live configuration ------------------------------------------------------------------
reset; touch "$work/die-front"
retired_env=(); for v in $RETIRED; do retired_env+=("$v=1" "${v}_CONFIRMED=1"); done
run "${retired_env[@]}" UI_PORT=8021 >/dev/null 2>"$work/err"; rc=$?
check "a full front starts both; Node gets no auth, project or free-chat confirmation" 'grep -q "^node .* CCONF= FCONF= PCONF= CONF=$" "$work/log" && grep -q "^front .* CCONF= FCONF= PCONF= CONF=$" "$work/log"'
check "no retired confirmation reaches either process, inherited or not" 'grep -q "^node RETIRED=$" "$work/log" && grep -q "^front RETIRED=$" "$work/log"'
check "a leftover retired switch earns a warning and changes nothing" 'grep -q "NOEVIA_RUST_STATS is retired" "$work/err" && grep -q "NOEVIA_RUST_EXPORT is retired" "$work/err" && grep -q "NOEVIA_RUST_AUTH is retired" "$work/err" && grep -q "NOEVIA_RUST_PROJECTS is retired" "$work/err"'
check "the retired sign-in and projects switches reach neither process" 'grep -q "^node AUTH=unset$" "$work/log" && grep -q "^front AUTH=unset$" "$work/log" && grep -q "^node PROJECTS=unset$" "$work/log" && grep -q "^front PROJECTS=unset$" "$work/log"'
check "...and the run itself went ahead" '[ $rc -eq 3 ]'

# #1071: NOEVIA_RUST_AUTH is retired. Whatever is left in the environment (the old live 1, a 0 or
# empty) warns, starts both and reaches neither process; the front still owns sign-in.
for v in 1 0 ""; do
  reset; touch "$work/die-front"
  run NOEVIA_RUST_AUTH="$v" NOEVIA_RUST_AUTH_CONFIRMED=1 >/dev/null 2>"$work/err"; rc=$?
  check "a leftover NOEVIA_RUST_AUTH='$v' warns, is passed to neither process, and the run goes ahead" '[ $rc -eq 3 ] && grep -q "NOEVIA_RUST_AUTH is retired" "$work/err" && grep -q "^node AUTH=unset$" "$work/log" && grep -q "^front AUTH=unset$" "$work/log" && grep -q "^node .* CONF=$" "$work/log"'
done
# #1071: NOEVIA_RUST_PROJECTS is retired. Whatever is left in the environment (the old live 1, a 0
# or empty) warns, starts both and reaches neither process; its old confirmations never reach Node,
# and the project store is still set up.
for v in 1 0 ""; do
  reset; touch "$work/die-front"
  run NOEVIA_RUST_PROJECTS="$v" NOEVIA_RUST_PROJECTS_CONFIRMED=1 NOEVIA_RUST_PROJECT_CHATS_CONFIRMED=1 NOEVIA_RUST_FREE_CHATS_CONFIRMED=1 NOEVIA_RUST_PROJECT_STORE_CONFIRMED=1 >/dev/null 2>"$work/err"; rc=$?
  check "a leftover NOEVIA_RUST_PROJECTS='$v' warns, is passed to neither process, and the run goes ahead" '[ $rc -eq 3 ] && grep -q "NOEVIA_RUST_PROJECTS is retired" "$work/err" && grep -q "^node PROJECTS=unset$" "$work/log" && grep -q "^front PROJECTS=unset$" "$work/log" && grep -q "^node .* CCONF= FCONF= PCONF= CONF=$" "$work/log" && grep -q "^node SSTORE= SPORT=" "$work/log"'
done
reset; touch "$work/die-front"
run >/dev/null 2>"$work/err"; rc=$?
check "without NOEVIA_RUST_AUTH or NOEVIA_RUST_PROJECTS both start, with no warning about either" '[ $rc -eq 3 ] && grep -q "^node AUTH=unset$" "$work/log" && ! grep -q "NOEVIA_RUST_AUTH" "$work/err" && ! grep -q "NOEVIA_RUST_PROJECTS" "$work/err"'

# Context ownership is permanent: the capability is required in ROUTE_FEATURES,
# and neither old selector nor a spoofed confirmation reaches either process.
for v in "" 0 1 true; do
  reset; touch "$work/die-front"
  run NOEVIA_RUST_PROJECT_WRITES="$v" NOEVIA_RUST_PROJECT_WRITES_CONFIRMED=1 >/dev/null 2>"$work/err"; rc=$?
  check "retired context selector '$v' warns and cannot restore Node ownership" '[ $rc -eq 3 ] && grep -q "NOEVIA_RUST_PROJECT_WRITES is retired" "$work/err" && grep -q "^node WCONF= WRITES=unset$" "$work/log" && grep -q "^front WCONF= WRITES=unset$" "$work/log"'
done
reset
run STUB_FEATURES="$(without rust-project-writes)" NOEVIA_RUST_PROJECT_WRITES=0 NOEVIA_RUST_PROJECT_WRITES_CONFIRMED=1 >/dev/null 2>"$work/err"; rc=$?
check "spoofed context confirmation cannot bypass the mandatory compiled capability" '[ $rc -eq 2 ] && grep -q "lacks the front route feature(s): rust-project-writes;" "$work/err" && [ ! -s "$work/log" ]'

# The project store (#1264), permanent: always set up, a per-start secret and UI_PORT+2000 for both
# processes; never from the environment; refused without project-store (a front route feature,
# above). The retired NOEVIA_RUST_PROJECT_STORE and its confirmation are not passed on.
for v in "" 0 1; do
  reset; touch "$work/die-front"
  run UI_PORT=8021 NOEVIA_RUST_PROJECT_STORE="$v" NOEVIA_PROJECT_STORE_TOKEN=inherited-secret-0123456789abcdef0123456789 NOEVIA_PROJECT_STORE_PORT=1 NOEVIA_RUST_PROJECT_STORE_CONFIRMED=1 >/dev/null 2>&1
  check "store (retired switch '$v'): both get UI_PORT+2000 and a fresh 64-character secret, never the inherited one or a confirmation" 'grep -q "^node SSTORE= SPORT=10021 STOKLEN=64$" "$work/log" && grep -q "^front SSTORE= SPORT=10021 STOKLEN=64$" "$work/log"'
done
# The start id (#1071): one fresh 32-hex id per start, the same for both processes, never inherited.
start_of() { sed -n "s/^$1 START=//p" "$work/log"; }
reset; touch "$work/die-front"
run NOEVIA_START_ID=0123456789abcdef0123456789abcdef >/dev/null 2>&1
first="$(start_of node)"
check "start id: both processes get the same fresh 32-hex id, not the inherited one" '[[ "$first" =~ ^[0-9a-f]{32}$ ]] && [ "$(start_of front)" = "$first" ] && [ "$first" != 0123456789abcdef0123456789abcdef ]'
reset; touch "$work/die-front"
run >/dev/null 2>&1
check "start id: the next start gets a different one" '[[ "$(start_of node)" =~ ^[0-9a-f]{32}$ ]] && [ "$(start_of node)" != "$first" ] && [ "$(start_of front)" = "$(start_of node)" ]'
reset
run UI_PORT=64000 >/dev/null 2>"$work/err"; rc=$?
check "a UI_PORT with no room for UI_PORT+2000 is refused, nothing started" '[ $rc -eq 2 ] && grep -q "no room for the project store port" "$work/err" && [ ! -s "$work/log" ]'

# The client address (noevia-core server/auth.cjs, #1578): Node is told to believe X-Noevia-Client-Ip
# only when the front reports client-address-header (it strips client copies of the header).
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES client-address-header" >/dev/null 2>&1
check "a front with client-address-header confirms the client address to Node only" 'grep -q "^node KCONF=1$" "$work/log" && grep -q "^front KCONF=$" "$work/log"'
reset; touch "$work/die-front"
run NOEVIA_FRONT_CLIENT_IP=1 >/dev/null 2>"$work/err"
check "a front without it leaves Node unconfirmed (an inherited value is dropped) and still starts" 'grep -q "^node KCONF=$" "$work/log" && grep -q "no client-address-header" "$work/err"'

# #2027: only the verified front can be supplied as the filesystem claim helper.
reset; touch "$work/die-front"
run NOEVIA_LEGACY_CLAIM_BIN=/untrusted/inherited STUB_FEATURES="$BASE_FEATURES legacy-root-claim" >/dev/null 2>&1
check "claim-capable front supplies its own absolute helper path to Node only" 'grep -Fxq "node LCLAIM=$work/noevia-server" "$work/log" && grep -qx "front LCLAIM=" "$work/log"'
reset; touch "$work/die-front"
run NOEVIA_LEGACY_CLAIM_BIN=/untrusted/inherited >/dev/null 2>&1
check "older front cannot pass an inherited claim helper path" 'grep -qx "node LCLAIM=" "$work/log" && grep -qx "front LCLAIM=" "$work/log"'

# #1822 regression: a front that dies at once must not cut a slow-starting Node's startup log short
# (the supervisor TERMs the survivor the moment one child exits).
reset; touch "$work/die-front"
run STUB_NODE_DELAY=0.4 >/dev/null 2>&1
check "a front dying at once does not truncate a slow Node's startup log" 'grep -q "^node WCONF= WRITES=unset$" "$work/log" && grep -q "^node KCONF=" "$work/log"'

# Feature ownership must agree in both directions before either process launches.
mkdir -p "$work/server"
printf '%s\n' "const setup = factory.createReadOnlyFeatures({});" > "$work/server/index.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "native feature Core with old front refuses before launch" '[ $rc -eq 2 ] && grep -q "feature ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-feature-owner" >/dev/null 2>"$work/err"; rc=$?
check "matching native feature pair starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
rm "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-feature-owner" >/dev/null 2>"$work/err"; rc=$?
check "native feature front with old Core refuses before launch" '[ $rc -eq 2 ] && grep -q "feature ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
# Diary route ownership (sbstndalton/noevia#2185) must agree in both directions before launch.
printf '%s\n' "  if (require('./rust-diary.cjs').refuseOwned(req, res, {})) return;" > "$work/server/index.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "Diary-refusing Core with an old front refuses before launch" '[ $rc -eq 2 ] && grep -q "Diary route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-diary" >/dev/null 2>"$work/err"; rc=$?
check "matching native Diary pair starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
rm "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-diary" >/dev/null 2>"$work/err"; rc=$?
check "rust-diary front with old Core refuses before launch" '[ $rc -eq 2 ] && grep -q "Diary route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
# Model read route ownership (sbstndalton/noevia#2187) must agree in both directions before launch.
printf '%s\n' "  if (require('./rust-models.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "model-read-refusing Core with an old front refuses before launch" '[ $rc -eq 2 ] && grep -q "model read route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-model-reads" >/dev/null 2>"$work/err"; rc=$?
check "matching native model read pair starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
rm "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-model-reads" >/dev/null 2>"$work/err"; rc=$?
check "rust-model-reads front with old Core refuses before launch" '[ $rc -eq 2 ] && grep -q "model read route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run >/dev/null 2>"$work/err"; rc=$?
check "old model read pair (old Core, old front) still starts for rollback" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
# Project source read route ownership (sbstndalton/noevia#2191) must agree in both directions before launch.
printf '%s\n' "  if (require('./rust-project-reads.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "project-read-refusing Core with an old front refuses before launch" '[ $rc -eq 2 ] && grep -q "project source read ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-project-reads" >/dev/null 2>"$work/err"; rc=$?
check "matching native project source read pair starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
rm "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-project-reads" >/dev/null 2>"$work/err"; rc=$?
check "rust-project-reads front with old Core refuses before launch" '[ $rc -eq 2 ] && grep -q "project source read ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run >/dev/null 2>"$work/err"; rc=$?
check "old project source read pair (old Core, old front) still starts for rollback" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
# Project create/delete route ownership (sbstndalton/noevia#2191 round 2) must agree in both directions.
printf '%s\n' "  if (require('./rust-project-lifecycle.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "project-lifecycle-refusing Core with an old front refuses before launch" '[ $rc -eq 2 ] && grep -q "project create/delete ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-project-lifecycle" >/dev/null 2>"$work/err"; rc=$?
check "matching native project create/delete pair starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
rm "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-project-lifecycle" >/dev/null 2>"$work/err"; rc=$?
check "rust-project-lifecycle front with old Core refuses before launch" '[ $rc -eq 2 ] && grep -q "project create/delete ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run >/dev/null 2>"$work/err"; rc=$?
check "old project create/delete pair (old Core, old front) still starts for rollback" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
# Project storage write ownership (sbstndalton/noevia#2191 round 3) must agree in both directions.
printf '%s\n' "  kinds: require('./rust-project-storage.cjs').CORE_KINDS," > "$work/server/index.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "project-storage Core with an old front refuses before launch" '[ $rc -eq 2 ] && grep -q "project storage write ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-project-storage" >/dev/null 2>"$work/err"; rc=$?
check "matching native project storage pair starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
rm "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-project-storage" >/dev/null 2>"$work/err"; rc=$?
check "rust-project-storage front with old Core refuses before launch" '[ $rc -eq 2 ] && grep -q "project storage write ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run >/dev/null 2>"$work/err"; rc=$?
check "old project storage pair (old Core, old front) still starts for rollback" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
# Since noevia-core b6e2238 index.cjs loads the split through its outbox view.
storage_outbox_link="const indexOutbox = require('./rust-index-outbox.cjs').createIndexOutbox({});"
storage_outbox_body="const { CORE_KINDS } = require('./rust-project-storage.cjs');"
printf '%s\n' "$storage_outbox_link" > "$work/server/index.cjs"; printf '%s\n' "$storage_outbox_body" > "$work/server/rust-index-outbox.cjs"
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-project-storage" >/dev/null 2>"$work/err"; rc=$?
check "a storage split loaded through the outbox view pairs with the native front" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
reset
run >/dev/null 2>"$work/err"; rc=$?
check "a storage split loaded through the outbox view refuses an old front" '[ $rc -eq 2 ] && grep -q "project storage write ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
printf '%s\n' "// $storage_outbox_body" > "$work/server/rust-index-outbox.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-project-storage" >/dev/null 2>"$work/err"; rc=$?
check "an outbox view naming the split only in a comment is no split (native front refused)" '[ $rc -eq 2 ] && grep -q "project storage write ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
printf '%s\n' "// $storage_outbox_link" > "$work/server/index.cjs"; printf '%s\n' "$storage_outbox_body" > "$work/server/rust-index-outbox.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-project-storage" >/dev/null 2>"$work/err"; rc=$?
check "an outbox view index.cjs does not load is no split (native front refused)" '[ $rc -eq 2 ] && grep -q "project storage write ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run >/dev/null 2>"$work/err"; rc=$?
check "an outbox view index.cjs does not load leaves an old pair that still starts" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
printf '%s\n' "  // kinds: require('./rust-project-storage.cjs').CORE_KINDS," > "$work/server/index.cjs"; rm -f "$work/server/rust-index-outbox.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-project-storage" >/dev/null 2>"$work/err"; rc=$?
check "a direct storage require only in a comment is no split (native front refused)" '[ $rc -eq 2 ] && grep -q "project storage write ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
rm -f "$work/server/index.cjs"
# Project file delete ownership (sbstndalton/noevia#2191 round 3) must agree in both directions.
printf '%s\n' "  if (require('./rust-project-files.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "project-file-refusing Core with an old front refuses before launch" '[ $rc -eq 2 ] && grep -q "project file delete ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-project-files" >/dev/null 2>"$work/err"; rc=$?
check "matching native project file delete pair starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
rm "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-project-files" >/dev/null 2>"$work/err"; rc=$?
check "rust-project-files front with old Core refuses before launch" '[ $rc -eq 2 ] && grep -q "project file delete ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
# Document page reader ownership (sbstndalton/noevia#2191 round 4) must agree in both directions.
printf '%s\n' "  if (require('./rust-project-document-pages.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "document-pages-refusing Core with an old front refuses before launch" '[ $rc -eq 2 ] && grep -q "project document page reader ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-project-document-pages" >/dev/null 2>"$work/err"; rc=$?
check "matching native document page reader pair starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
rm "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-project-document-pages" >/dev/null 2>"$work/err"; rc=$?
check "rust-project-document-pages front with old Core refuses before launch" '[ $rc -eq 2 ] && grep -q "project document page reader ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
# Conversation import ownership (sbstndalton/noevia#2191 round 4) must agree in both directions.
printf '%s\n' "  if (require('./rust-conversation-import.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "conversation-import-refusing Core with an old front refuses before launch" '[ $rc -eq 2 ] && grep -q "conversation import ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-conversation-import" >/dev/null 2>"$work/err"; rc=$?
check "matching native conversation import pair starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
rm "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-conversation-import" >/dev/null 2>"$work/err"; rc=$?
check "rust-conversation-import front with old Core refuses before launch" '[ $rc -eq 2 ] && grep -q "conversation import ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
# Project document upload ownership (sbstndalton/noevia#2191 round 5) must agree in both directions.
printf '%s\n' "  if (require('./rust-project-documents.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "document-upload-refusing Core with an old front refuses before launch" '[ $rc -eq 2 ] && grep -q "project document upload ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-document-extraction rust-project-documents" >/dev/null 2>"$work/err"; rc=$?
check "matching native document upload pair starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
rm "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-document-extraction rust-project-documents" >/dev/null 2>"$work/err"; rc=$?
check "rust-project-documents front with old Core refuses before launch" '[ $rc -eq 2 ] && grep -q "project document upload ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
# Source job ownership (sbstndalton/noevia#2191 round 6) must agree in both directions.
printf '%s\n' "  if (require('./rust-source-jobs.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "source-job-refusing Core with an old front refuses before launch" '[ $rc -eq 2 ] && grep -q "source job ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-source-jobs" >/dev/null 2>"$work/err"; rc=$?
check "matching native source job pair starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
rm "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-source-jobs" >/dev/null 2>"$work/err"; rc=$?
check "rust-source-jobs front with old Core refuses before launch" '[ $rc -eq 2 ] && grep -q "source job ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
# Project source operations ownership (sbstndalton/noevia#2191 round 8) must agree in both directions.
printf '%s\n' "  if (require('./rust-project-source-work.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "source-work-refusing Core with an old front refuses before launch" '[ $rc -eq 2 ] && grep -q "project source operations ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-project-source-work" >/dev/null 2>"$work/err"; rc=$?
check "matching native source operations pair starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
# A matching pair whose Core still ships any piece of the worker path is refused: the pair is read, not just
# its two switches (so the case above is a control with something to be refused for).
for worker_file in source-work.cjs folder-import.cjs pdf-reduce.cjs; do
  reset; : > "$work/server/$worker_file"
  run STUB_FEATURES="$BASE_FEATURES rust-project-source-work" >/dev/null 2>"$work/err"; rc=$?
  check "a native source operations pair whose Core keeps server/$worker_file refuses before launch" '[ $rc -eq 2 ] && grep -q "project source operations ownership mismatch" "$work/err" && grep -q "'"$worker_file"'" "$work/err" && [ ! -s "$work/log" ]'
  rm -f "$work/server/$worker_file"
done
mkdir -p "$work/server/routes"
printf '%s\n' "if (req.headers['x-noevia-source-work']) {}" > "$work/server/routes/projects.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-project-source-work" >/dev/null 2>"$work/err"; rc=$?
check "a native source operations pair whose Core still answers the worker header refuses before launch" '[ $rc -eq 2 ] && grep -q "still answers the worker call" "$work/err" && [ ! -s "$work/log" ]'
rm -f "$work/server/routes/projects.cjs"
rm "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-project-source-work" >/dev/null 2>"$work/err"; rc=$?
check "rust-project-source-work front with old Core refuses before launch" '[ $rc -eq 2 ] && grep -q "project source operations ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
# The host-wide large-document lock (sbstndalton/noevia#2191 round 7, #697) must agree in both directions.
printf '%s\n' "const fileLock = require('./large-document-file-lock.cjs');" > "$work/server/large-document-lock.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "host-lock Core with an in-process-lock front refuses before launch" '[ $rc -eq 2 ] && grep -q "large-document lock mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-large-document-host-lock" >/dev/null 2>"$work/err"; rc=$?
check "matching host large-document lock pair starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
rm "$work/server/large-document-lock.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-large-document-host-lock" >/dev/null 2>"$work/err"; rc=$?
check "host-lock front with an in-process-lock Core refuses before launch" '[ $rc -eq 2 ] && grep -q "large-document lock mismatch" "$work/err" && [ ! -s "$work/log" ]'
# F1 reads the actual export; duplicate and unknown runtime tokens must refuse.
printf '%s\n' "module.exports = { RUST_AUTO_ROLES_WRITE_OWNED: 'rust-auto-roles-write' };" > "$work/server/rust-auto-roles-write.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "auto-roles write: new Core refuses old front" '[ $rc -eq 2 ] && grep -q "auto-roles write" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-auto-roles-write" >/dev/null 2>"$work/err"; rc=$?
check "auto-roles write: paired export starts both processes" '[ $rc -eq 3 ] && grep -q "^node " "$work/log" && grep -q "^front " "$work/log"'
rm -f "$work/server/rust-auto-roles-write.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-auto-roles-write" >/dev/null 2>"$work/err"; rc=$?
check "auto-roles write: new front refuses old Core" '[ $rc -eq 2 ] && grep -q "auto-roles write" "$work/err" && [ ! -s "$work/log" ]'
for malformed in absent unknown duplicate comment template invalid; do
  for front in old new; do
    case "$malformed" in
      absent) printf "%s\n" "module.exports = {};" ;;
      unknown) printf "%s\n" "module.exports = { RUST_AUTO_ROLES_WRITE_OWNED: 'rust-auto-roles-write-v2' };" ;;
      duplicate) printf "%s\n" "module.exports = { RUST_AUTO_ROLES_WRITE_OWNED: 'rust-auto-roles-write' }; const other = 'rust-auto-roles-write';" ;;
      comment) printf "%s\n" "/* module.exports = { RUST_AUTO_ROLES_WRITE_OWNED: 'rust-auto-roles-write' }; */" ;;
      template) printf '%s\n' 'const text = `not an export; "rust-auto-roles-write"`;' ;;
      invalid) printf '%s\n' 'module.exports = {' ;;
    esac > "$work/server/rust-auto-roles-write.cjs"
    features="$BASE_FEATURES"; [ "$front" = old ] || features="$features rust-auto-roles-write"
    reset
    run STUB_FEATURES="$features" >/dev/null 2>"$work/err"; rc=$?
    check "auto-roles write: $malformed Core marker refuses with $front front" '[ $rc -eq 2 ] && grep -q "auto-roles write" "$work/err" && [ ! -s "$work/log" ]'
  done
done
for core in old new; do
  rm -f "$work/server/rust-auto-roles-write.cjs"
  [ "$core" = old ] || printf "%s\n" "module.exports = { RUST_AUTO_ROLES_WRITE_OWNED: 'rust-auto-roles-write' };" > "$work/server/rust-auto-roles-write.cjs"
  for markers in 'rust-auto-roles-write-v2' 'rust-auto-roles-write rust-auto-roles-write' 'rust-auto-roles-write rust-auto-roles-write-v2'; do
    reset
    run STUB_FEATURES="$BASE_FEATURES $markers" >/dev/null 2>"$work/err"; rc=$?
    check "auto-roles write: malformed front tokens refuse with $core Core ($markers)" '[ $rc -eq 2 ] && grep -q "auto-roles write" "$work/err" && [ ! -s "$work/log" ]'
  done
done
rm -f "$work/server/rust-auto-roles-write.cjs"
printf '%s\n' "module.exports = { get RUST_AUTO_ROLES_WRITE_OWNED() { for (;;) {} } };" > "$work/server/rust-auto-roles-write.cjs"
reset
started=$(date +%s)
run STUB_FEATURES="$BASE_FEATURES rust-auto-roles-write" >/dev/null 2>"$work/err"; rc=$?
elapsed=$(( $(date +%s) - started ))
check "auto-roles Core whose export never returns refuses within the bound" '[ $rc -eq 2 ] && grep -q "auto-roles write" "$work/err" && [ ! -s "$work/log" ] && [ $elapsed -lt 30 ]'
rm -f "$work/server/rust-auto-roles-write.cjs"
# The public pages and the plugin directory (sbstndalton/noevia#2190): two pairs, each checked in
# both directions and independently of the other (one group may roll back alone).
for pair in "rust-preset-read.cjs rust-preset-read preset GET" "rust-calibration-routes.cjs rust-calibration-routes calibration start/read" "rust-idle-unload-read.cjs rust-idle-unload-read idle unload read" "rust-model-manager-scan.cjs rust-model-manager-scan model folder scan read" "rust-autotune-resume.cjs rust-autotune-resume auto-tune resume" "rust-autotune-cancel.cjs rust-autotune-cancel auto-tune cancellation" "rust-autotune-read.cjs rust-autotune-read auto-tune status GET" "rust-tool-approvals.cjs rust-tool-approvals tool approval decision" "rust-public-pages.cjs rust-public-pages public page" "rust-task-reads.cjs rust-task-reads task read" "rust-autotune-settings.cjs rust-autotune-settings auto-tune settings" "rust-calibration-cancel.cjs rust-calibration-cancel calibration cancellation" "rust-autotune-clear.cjs rust-autotune-clear autotune dismissal" "rust-model-state-reads.cjs rust-model-state-reads model state read" "rust-plugin-directory.cjs rust-plugin-directory plugin directory" "rust-decision-settings.cjs rust-decision-settings decision settings" "rust-storage-integrations.cjs rust-storage-integrations storage integration" "rust-vault-mirror-preferences.cjs rust-vault-mirror-preferences vault mirror preference" "rust-storage-connection.cjs rust-storage-connection storage connection" "rust-provider-test.cjs rust-provider-test provider test" "rust-chatgpt-oauth.cjs rust-chatgpt-oauth ChatGPT sign-in" "rust-provider-models.cjs rust-provider-models provider model list" "rust-providers.cjs rust-providers provider registry" "rust-providers-reseal.cjs rust-providers-reseal provider key reseal" "rust-google-drive.cjs rust-google-drive Google Drive" "rust-offsite-backup.cjs rust-offsite-backup off-site backup" "rust-mcp-credentials.cjs rust-mcp-credentials MCP credential" "rust-health.cjs rust-health health check" "rust-auto-roles.cjs rust-auto-roles auto-roles read" "rust-workspace.cjs rust-workspace workspace list" "rust-diary-routes.cjs rust-diary-routes Diary connector and recovery" "rust-diary-local-exchange.cjs rust-diary-local-exchange Diary local exchange" "rust-mcp-directory.cjs rust-mcp-directory MCP directory" "rust-toolbox-picker.cjs rust-toolbox-picker toolbox picker" "rust-toolboxes-permitted.cjs rust-toolboxes-permitted permitted tools" "rust-connectors.cjs rust-connectors connectors" "rust-project-config.cjs rust-project-config project configuration" "rust-admin-users.cjs rust-admin-users account deletion" "rust-research.cjs rust-research deep research" "rust-chat-search.cjs rust-chat-search chat search" "rust-chat-export.cjs rust-chat-export chat download" "rust-autotune-untuned.cjs rust-autotune-untuned untuned discovery" "rust-model-evidence-recheck.cjs rust-model-evidence-recheck model evidence recheck" "rust-model-assignments.cjs rust-model-assignments model assignments read" "rust-model-downloads-read.cjs rust-model-downloads-read model download GET"; do
  read -r p_marker p_cap p_label <<<"$pair"
  printf '%s\n' "  if (require('./$p_marker').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
  reset
  run >/dev/null 2>"$work/err"; rc=$?
  check "$p_label: refusing Core with an old front refuses before launch" '[ $rc -eq 2 ] && grep -q "$p_label route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
  reset; touch "$work/die-front"
  run STUB_FEATURES="$BASE_FEATURES $p_cap" >/dev/null 2>"$work/err"; rc=$?
  check "$p_label: matching native pair starts both processes (the other group old/old)" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
  rm "$work/server/index.cjs"
  reset
  run STUB_FEATURES="$BASE_FEATURES $p_cap" >/dev/null 2>"$work/err"; rc=$?
  check "$p_label: native front with old Core refuses before launch" '[ $rc -eq 2 ] && grep -q "$p_label route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
done
# Inference machines pair on Core's exported client marker, not an index.cjs require (noevia-core 16e260f).
machines_label="inference machines route ownership mismatch"
printf '%s\n' "const MACHINES_CLIENT_MARKER = 'noevia-inference-machines-client/1';" "module.exports = { MACHINES_CLIENT_MARKER };" > "$work/server/rust-inference-machines.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "inference machines: marked Core with an old front refuses before launch" '[ $rc -eq 2 ] && grep -q "$machines_label" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-inference-machines" >/dev/null 2>"$work/err"; rc=$?
check "inference machines: marked Core with the native front starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
printf '%s\n' "const MACHINES_CLIENT_MARKER = 'noevia-inference-machines-client/2';" "module.exports = { MACHINES_CLIENT_MARKER };" > "$work/server/rust-inference-machines.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-inference-machines" >/dev/null 2>"$work/err"; rc=$?
check "inference machines: an unknown marker is no native Core (native front refused)" '[ $rc -eq 2 ] && grep -q "$machines_label" "$work/err" && [ ! -s "$work/log" ]'
printf '%s\n' "// const MACHINES_CLIENT_MARKER = 'noevia-inference-machines-client/1';" "module.exports = {};" > "$work/server/rust-inference-machines.cjs"
printf '%s\n' "  if (require('./rust-inference-machines.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-inference-machines" >/dev/null 2>"$work/err"; rc=$?
check "inference machines: an index.cjs require without the exported marker is no native Core" '[ $rc -eq 2 ] && grep -q "$machines_label" "$work/err" && [ ! -s "$work/log" ]'
rm -f "$work/server/rust-inference-machines.cjs" "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-inference-machines" >/dev/null 2>"$work/err"; rc=$?
check "inference machines: native front with old Core refuses before launch" '[ $rc -eq 2 ] && grep -q "$machines_label" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run >/dev/null 2>"$work/err"; rc=$?
check "inference machines: old pair still starts for rollback" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
# Chat port S4: the framing suggestion pair, in both directions (its own loop, beside the list above).
for pair in "rust-chat-framing-suggest.cjs rust-chat-framing-suggest framing suggestion"; do
  read -r p_marker p_cap p_label <<<"$pair"
  printf '%s\n' "  if (require('./$p_marker').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
  reset
  run >/dev/null 2>"$work/err"; rc=$?
  check "$p_label: refusing Core with an old front refuses before launch" '[ $rc -eq 2 ] && grep -q "$p_label route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
  reset; touch "$work/die-front"
  run STUB_FEATURES="$BASE_FEATURES $p_cap" >/dev/null 2>"$work/err"; rc=$?
  check "$p_label: matching native pair starts both processes (the other group old/old)" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
  rm "$work/server/index.cjs"
  reset
  run STUB_FEATURES="$BASE_FEATURES $p_cap" >/dev/null 2>"$work/err"; rc=$?
  check "$p_label: native front with old Core refuses before launch" '[ $rc -eq 2 ] && grep -q "$p_label route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
done
printf '%s\n' "  if (require('./rust-public-pages.cjs').refuseOwned(req, res, {})) return;" "  if (require('./rust-plugin-directory.cjs').refuseOwned(req, res, {})) return;" > "$work/server/index.cjs"
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-public-pages rust-plugin-directory" >/dev/null 2>"$work/err"; rc=$?
check "both #2190 pairs native start both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
rm "$work/server/index.cjs"
# Round 2 (#2190): the decision settings pair beside the round 1 pairs, and alone.
printf '%s\n' "  if (require('./rust-public-pages.cjs').refuseOwned(req, res, {})) return;" "  if (require('./rust-plugin-directory.cjs').refuseOwned(req, res, {})) return;" "  if (require('./rust-decision-settings.cjs').refuseOwned(req, res, {})) return;" > "$work/server/index.cjs"
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-public-pages rust-plugin-directory rust-decision-settings" >/dev/null 2>"$work/err"; rc=$?
check "all three #2190 pairs native start both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
reset
run STUB_FEATURES="$BASE_FEATURES rust-public-pages rust-plugin-directory" >/dev/null 2>"$work/err"; rc=$?
check "decision settings Core without the front capability refuses even with the other pairs native" '[ $rc -eq 2 ] && grep -q "decision settings route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
rm "$work/server/index.cjs"
# Round 3 (#2190): the storage integration pair beside the earlier pairs, and alone.
printf '%s\n' "  if (require('./rust-public-pages.cjs').refuseOwned(req, res, {})) return;" "  if (require('./rust-plugin-directory.cjs').refuseOwned(req, res, {})) return;" "  if (require('./rust-decision-settings.cjs').refuseOwned(req, res, {})) return;" "  if (require('./rust-storage-integrations.cjs').refuseOwned(req, res, {})) return;" > "$work/server/index.cjs"
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-public-pages rust-plugin-directory rust-decision-settings rust-storage-integrations" >/dev/null 2>"$work/err"; rc=$?
check "all four #2190 pairs native start both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
reset
run STUB_FEATURES="$BASE_FEATURES rust-public-pages rust-plugin-directory rust-decision-settings" >/dev/null 2>"$work/err"; rc=$?
check "storage integration Core without the front capability refuses even with the other pairs native" '[ $rc -eq 2 ] && grep -q "storage integration route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
rm "$work/server/index.cjs"
# Round 3 (#2190): the vault-mirror preference pair beside the storage pair, and alone.
printf '%s\n' "  if (require('./rust-storage-integrations.cjs').refuseOwned(req, res, {})) return;" "  if (require('./rust-vault-mirror-preferences.cjs').refuseOwned(req, res, {})) return;" > "$work/server/index.cjs"
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-storage-integrations rust-vault-mirror-preferences" >/dev/null 2>"$work/err"; rc=$?
check "the storage and vault-mirror pairs native start both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
reset
run STUB_FEATURES="$BASE_FEATURES rust-storage-integrations" >/dev/null 2>"$work/err"; rc=$?
check "vault mirror Core without the front capability refuses beside a native storage pair" '[ $rc -eq 2 ] && grep -q "vault mirror preference route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
rm "$work/server/index.cjs"

# Round 4 (#2190): the storage connection pair beside the storage integration pair, and alone.
printf '%s\n' "  if (require('./rust-storage-integrations.cjs').refuseOwned(req, res, {})) return;" "  if (require('./rust-storage-connection.cjs').refuseOwned(req, res, {})) return;" > "$work/server/index.cjs"
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-storage-integrations rust-storage-connection" >/dev/null 2>"$work/err"; rc=$?
check "the storage integration and connection pairs native start both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
reset
run STUB_FEATURES="$BASE_FEATURES rust-storage-integrations" >/dev/null 2>"$work/err"; rc=$?
check "storage connection Core without the front capability refuses beside a native integration pair" '[ $rc -eq 2 ] && grep -q "storage connection route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
rm "$work/server/index.cjs"

# Round 4 (#2190): the provider test pair beside the storage connection pair, and alone.
printf '%s\n' "  if (require('./rust-storage-connection.cjs').refuseOwned(req, res, {})) return;" "  if (require('./rust-provider-test.cjs').refuseOwned(req, res, {})) return;" > "$work/server/index.cjs"
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-storage-connection rust-provider-test" >/dev/null 2>"$work/err"; rc=$?
check "the storage connection and provider test pairs native start both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
reset
run STUB_FEATURES="$BASE_FEATURES rust-storage-connection" >/dev/null 2>"$work/err"; rc=$?
check "provider test Core without the front capability refuses beside a native connection pair" '[ $rc -eq 2 ] && grep -q "provider test route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
rm "$work/server/index.cjs"
# Model sizing route ownership (sbstndalton/noevia#2189) must agree in both directions before launch,
# separately from the #2187 model reads (each generation pairs on its own).
printf '%s\n' "  if (require('./rust-model-sizing.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "model-sizing-refusing Core with an old front refuses before launch" '[ $rc -eq 2 ] && grep -q "model sizing route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-model-sizing" >/dev/null 2>"$work/err"; rc=$?
check "matching native model sizing pair starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
printf '%s\n' "  if (require('./rust-models.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" "  if (require('./rust-model-sizing.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-model-reads" >/dev/null 2>"$work/err"; rc=$?
check "an M2-A Core with an M1-only front refuses before launch" '[ $rc -eq 2 ] && grep -q "model sizing route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-model-reads rust-model-sizing" >/dev/null 2>"$work/err"; rc=$?
check "the M1 and M2-A Core with a front that has both starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
printf '%s\n' "  if (require('./rust-models.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-model-reads rust-model-sizing" >/dev/null 2>"$work/err"; rc=$?
check "an M2-A front beside an M1-only Core refuses before launch" '[ $rc -eq 2 ] && grep -q "model sizing route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
rm "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-model-sizing" >/dev/null 2>"$work/err"; rc=$?
check "rust-model-sizing front with old Core refuses before launch" '[ $rc -eq 2 ] && grep -q "model sizing route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run >/dev/null 2>"$work/err"; rc=$?
check "old model sizing pair (old Core, old front) still starts for rollback" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'

# Model maintenance route ownership (sbstndalton/noevia#2210) must agree in both directions before launch,
# separately from the model reads (#2187) and the sizing reads (#2189).
printf '%s\n' "  if (require('./rust-model-maintenance.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "model-maintenance-refusing Core with an old front refuses before launch" '[ $rc -eq 2 ] && grep -q "model maintenance route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-model-maintenance" >/dev/null 2>"$work/err"; rc=$?
check "matching native model maintenance pair starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
printf '%s\n' "  if (require('./rust-models.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" "  if (require('./rust-model-sizing.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-model-reads rust-model-sizing rust-model-maintenance" >/dev/null 2>"$work/err"; rc=$?
check "a maintenance front beside a Core with only the reads and sizing refuses before launch" '[ $rc -eq 2 ] && grep -q "model maintenance route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
printf '%s\n' "  if (require('./rust-models.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" "  if (require('./rust-model-sizing.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" "  if (require('./rust-model-maintenance.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;" > "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-model-reads rust-model-sizing" >/dev/null 2>"$work/err"; rc=$?
check "a maintenance Core beside a front with only the reads and sizing refuses before launch" '[ $rc -eq 2 ] && grep -q "model maintenance route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-model-reads rust-model-sizing rust-model-maintenance" >/dev/null 2>"$work/err"; rc=$?
check "reads, sizing and maintenance on both sides start both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
rm "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-model-maintenance" >/dev/null 2>"$work/err"; rc=$?
check "rust-model-maintenance front with old Core refuses before launch" '[ $rc -eq 2 ] && grep -q "model maintenance route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run >/dev/null 2>"$work/err"; rc=$?
check "old model maintenance pair (old Core, old front) still starts for rollback" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'

# Embedding background: one-way guard; old Core never authorizes native work.
printf '%s\n' "module.exports = { RUST_EMBEDDING_OWNED: 'rust-embedding-background-v1' };" > "$work/server/rust-embedding-background.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "embedding Core with old front refuses before launch" '[ $rc -eq 2 ] && grep -q "requires rust-embedding-background-v1" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-embedding-background-v1" >/dev/null 2>"$work/err"; rc=$?
check "paired embedding Core/front starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
rm "$work/server/rust-embedding-background.cjs"
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-embedding-background-v1" >/dev/null 2>"$work/err"; rc=$?
check "new embedding front with old Core starts for proof-gated legacy mode" '[ $rc -eq 3 ] && grep -q "^node " "$work/log"'
reset
mkdir -p "$work/server/ui-data"
(exec 8>"$work/server/ui-data/embedding-pair.lock"; flock -x 8; touch "$work/pair-held"; while [ -f "$work/pair-held" ]; do sleep 0.05; done) &
pair_holder=$!
for _ in $(seq 1 100); do [ -f "$work/pair-held" ] && break; sleep 0.05; done
run >/dev/null 2>"$work/err"; rc=$?
check "replacement pair refuses until previous pair is quiescent" '[ $rc -eq 2 ] && grep -q "not quiescent" "$work/err" && [ ! -s "$work/log" ]'
rm "$work/pair-held"; wait "$pair_holder"

# The chat journal tail and the mirror and brain schedules (rust-chat-mirror-v1): both sides together or neither.
printf '%s\n' "module.exports = { RUST_CHAT_MIRROR_OWNED: 'rust-chat-mirror-v1' };" > "$work/server/rust-chat-mirror.cjs"
printf '%s\n' "const chatMirror = require('./rust-chat-mirror.cjs');" > "$work/server/index.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "a chat mirror Core beside an old front refuses before launch (the journal would go unread)" '[ $rc -eq 2 ] && grep -q "chat mirror schedule ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-chat-mirror-v1" >/dev/null 2>"$work/err"; rc=$?
check "a paired chat mirror Core and front start both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
reset
run STUB_FEATURES="rust-chat-mirror-v1x $BASE_FEATURES" >/dev/null 2>"$work/err"; rc=$?
check "a lookalike capability name is not the capability" '[ $rc -eq 2 ] && grep -q "chat mirror schedule ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
printf '%s\n' "module.exports = { RUST_CHAT_MIRROR_OWNED: 'something-else' };" > "$work/server/rust-chat-mirror.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-chat-mirror-v1" >/dev/null 2>"$work/err"; rc=$?
check "an index.cjs that requires the module without its marker refuses before launch" '[ $rc -eq 2 ] && grep -q "invalid chat mirror ownership marker" "$work/err" && [ ! -s "$work/log" ]'
rm "$work/server/rust-chat-mirror.cjs" "$work/server/index.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-chat-mirror-v1" >/dev/null 2>"$work/err"; rc=$?
check "a chat mirror front beside an old Core refuses before launch (a second schedule would run)" '[ $rc -eq 2 ] && grep -q "chat mirror schedule ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run >/dev/null 2>"$work/err"; rc=$?
check "the old chat mirror pair (old Core, old front) still starts for rollback" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'

# The preset reload baseline (noevia-rs model_maint/reload.rs): a bridge Core needs rust-preset-baseline;
# a rust-preset-baseline front beside an older Core is shared mode and starts.
printf '%s\n' "const PRESET_BASELINE_BRIDGE = 'noevia-preset-baseline-bridge/1';" > "$work/server/preset-reload.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "a preset-baseline bridge Core with an old front refuses before launch" '[ $rc -eq 2 ] && grep -q "has no rust-preset-baseline" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-preset-baseline" >/dev/null 2>"$work/err"; rc=$?
check "a preset-baseline bridge Core with a rust-preset-baseline front starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
rm "$work/server/preset-reload.cjs"
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-preset-baseline" >/dev/null 2>"$work/err"; rc=$?
check "a rust-preset-baseline front beside an older Core (shared mode) starts both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'

# Round 5 (#2190): the ChatGPT sign-in pair beside the provider test pair, and alone.
printf '%s\n' "  if (require('./rust-provider-test.cjs').refuseOwned(req, res, {})) return;" "  if (require('./rust-chatgpt-oauth.cjs').refuseOwned(req, res, {})) return;" > "$work/server/index.cjs"
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-provider-test rust-chatgpt-oauth" >/dev/null 2>"$work/err"; rc=$?
check "the provider test and ChatGPT sign-in pairs native start both processes" '[ $rc -eq 3 ] && grep -q "^front " "$work/log" && grep -q "^node " "$work/log"'
reset
run STUB_FEATURES="$BASE_FEATURES rust-provider-test" >/dev/null 2>"$work/err"; rc=$?
check "ChatGPT sign-in Core without the front capability refuses beside a native provider test pair" '[ $rc -eq 2 ] && grep -q "ChatGPT sign-in route ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
rm "$work/server/index.cjs"

# --- Process lifecycle -----------------------------------------------------------------------
reset
(run UI_PORT=8021 UI_HOST=0.0.0.0 COWORK_DAV_PORT=0 >/dev/null 2>"$work/err"; echo $? > "$work/rc") &
wait_log "^front" && wait_log "^node"
check "Node on loopback UI_PORT+1000" 'grep -q "^node UI_HOST=127.0.0.1 UI_PORT=9021 " "$work/log"'
check "the front on UI_HOST:UI_PORT with Node as upstream" 'grep -q "^front UI_HOST=0.0.0.0 UI_PORT=8021 UPSTREAM=http://127.0.0.1:9021 " "$work/log"'
touch "$work/die-front"
for _ in $(seq 1 100); do [ -s "$work/rc" ] && break; sleep 0.05; done
check "the front dying stops Node" 'grep -q "^node TERM" "$work/log"'
check "...and the container exits non-zero" '[ "$(cat "$work/rc")" = 3 ]'

reset
(run UI_PORT=8021 >/dev/null 2>&1; echo $? > "$work/rc") &
wait_log "^front" && wait_log "^node"
touch "$work/die-node"
for _ in $(seq 1 100); do [ -s "$work/rc" ] && break; sleep 0.05; done
check "Node dying stops the front, non-zero exit" 'grep -q "^front TERM" "$work/log" && [ "$(cat "$work/rc")" = 3 ]'

reset
(cd "$work" && exec env NOEVIA_FRONT=rust NOEVIA_SERVER_BIN="$work/noevia-server" UI_PORT=8021 COWORK_DAV_PORT=8040 STUB_FRONT_DRAIN=1 "${sup_cmd[@]}" >/dev/null 2>"$work/err") &
pid=$!
wait_log "^front" && wait_log "^node"
check "with DAV on, Node keeps UI_HOST (default 0.0.0.0) and says so" 'grep -q "^node UI_HOST=0.0.0.0 UI_PORT=9021 " "$work/log" && grep -q "not loopback only" "$work/err"'
kill -TERM "$pid"; wait "$pid"; rc=$?
check "docker stop (SIGTERM) stops both and exits 0" '[ $rc -eq 0 ] && grep -q "^node TERM" "$work/log" && grep -q "^front TERM" "$work/log"'

check "rollback drains and joins the front before stopping Core" 'awk "/^front JOINED$/ { joined=1 } /^node TERM$/ { if (!joined) exit 1; node=1 } END { if (!node || !joined) exit 1 }" "$work/log"'

reset
(cd "$work" && exec env NOEVIA_FRONT=rust NOEVIA_SERVER_BIN="$work/noevia-server" UI_PORT=8021 STUB_FRONT_STUCK=1 STUB_NODE_DRAIN=1 "${sup_cmd[@]}" >/dev/null 2>"$work/err") &
pid=$!
wait_log "^front KCONF=" && wait_log "^node KCONF="
front_child="$(sed -n 's/^front PID=//p' "$work/log")"
stop_started=$SECONDS
kill -TERM "$pid"; wait "$pid"; rc=$?
stop_elapsed=$((SECONDS - stop_started))
check "stuck front is killed and joined before Core shutdown" '[ $rc -eq 0 ] && grep -q "front stop deadline reached" "$work/err" && ! kill -0 "$front_child" 2>/dev/null && awk "/^front TERM$/ { front=1 } /^node TERM$/ { if (!front) exit 1; node=1 } END { if (!node) exit 1 }" "$work/log"'
check "Core shutdown hooks finish inside the default Docker stop budget" '[ "$stop_elapsed" -lt 10 ] && grep -q "^node HOOKS-DONE$" "$work/log"'

# Dedicated sharing moves as an independently paired runtime owner.
printf '%s\n' "const RUST_DAV_LISTENER_OWNED='rust-dav-listener-v1'; module.exports={RUST_DAV_LISTENER_OWNED};" > "$work/server/rust-dav-listener.cjs"
reset
run >/dev/null 2>"$work/err"; rc=$?
check "native DAV Core refuses a front without the listener" '[ $rc -eq 2 ] && grep -q "DAV listener ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'
reset; touch "$work/die-front"
run STUB_FEATURES="$BASE_FEATURES rust-dav-listener-v1" COWORK_DAV_PORT=8031 UI_HOST=192.0.2.7 >/dev/null 2>"$work/err"; rc=$?
check "native DAV pair retains the front bind while keeping Node loopback-only" '[ $rc -eq 3 ] && grep -q "^node UI_HOST=127.0.0.1 UI_PORT=9021 " "$work/log" && grep -q "^front UI_HOST=192.0.2.7 UI_PORT=" "$work/log"'
rm "$work/server/rust-dav-listener.cjs"
reset
run STUB_FEATURES="$BASE_FEATURES rust-dav-listener-v1" >/dev/null 2>"$work/err"; rc=$?
check "native DAV front refuses an old Core listener" '[ $rc -eq 2 ] && grep -q "DAV listener ownership mismatch" "$work/err" && [ ! -s "$work/log" ]'

echo "passed $passes, failed $fails"
[ "$fails" -eq 0 ]
