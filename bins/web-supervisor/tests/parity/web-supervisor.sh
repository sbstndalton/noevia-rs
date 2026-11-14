#!/usr/bin/env bash
# The web image's entrypoint (full-Rust migration M1, docs/adr-0001-rust-and-repo-split.md
# "Amendment 2026-10-10"). noevia-server (noevia-rs bins/noevia-server) faces the network on
# UI_HOST:UI_PORT, Node behind it on UI_PORT+1000 (NOEVIA_LEGACY_UPSTREAM). The front answers the
# routes noevia-rs contracts/http/routes.toml gives to Rust and streams the rest to Node.
#
# sbstndalton/noevia#1071: the front read routes (usage, sampling and reasoning settings, routing
# mode and default, engine statistics, ChatGPT status, connected-storage metadata/list/read and the
# conversation export) and sign-in and the account (rust-auth) are the Rust front's permanently;
# Node no longer has them. So are the project routes (images, project and free chat lists) and the
# project store. The deployment switches NOEVIA_RUST_AUTH and NOEVIA_RUST_PROJECTS are retired: a
# leftover value only warns. So the image runs only with NOEVIA_FRONT=rust and a noevia-server
# whose --features lists rust-auth and every front route feature (FRONT_ROUTE_FEATURES). Anything else stops here, non-zero, before either process starts.
# NOEVIA_FRONT=node is refused for the same reason: Node alone would answer those routes 503.
# If either process exits, the other is stopped and the container exits non-zero, so Docker's
# restart policy brings both back together.
set -uo pipefail

# Only this script may hand out the project store (below);
# never trust a value from the environment. (The retired confirmations, rust-auth's, the projects'
# and the front routes', are scrubbed too, so an inherited value can never reach a Node of either age.)
unset NOEVIA_RUST_AUTH_CONFIRMED NOEVIA_RUST_PROJECTS_CONFIRMED NOEVIA_RUST_PROJECT_CHATS_CONFIRMED NOEVIA_RUST_FREE_CHATS_CONFIRMED NOEVIA_RUST_USAGE_CONFIRMED NOEVIA_RUST_SAMPLING_SETTINGS_CONFIRMED NOEVIA_RUST_REASONING_SETTINGS_CONFIRMED NOEVIA_RUST_ROUTING_MODE_CONFIRMED NOEVIA_RUST_ROUTING_DEFAULT_CONFIRMED NOEVIA_RUST_STATS_CONFIRMED NOEVIA_RUST_EXPORT_CONFIRMED NOEVIA_RUST_STORAGE_READS_CONFIRMED NOEVIA_RUST_STORAGE_BROWSE_CONFIRMED NOEVIA_RUST_CHATGPT_STATUS_CONFIRMED NOEVIA_RUST_PROJECT_WRITES_CONFIRMED NOEVIA_RUST_PROJECT_STORE_CONFIRMED NOEVIA_PROJECT_STORE_PORT NOEVIA_PROJECT_STORE_TOKEN NOEVIA_FRONT_CLIENT_IP NOEVIA_LEGACY_CLAIM_BIN NOEVIA_START_ID
FRONT_ROUTE_FEATURES="rust-chatgpt-status rust-storage-reads rust-storage-browse rust-export rust-free-chats rust-usage rust-stats rust-routing-default rust-sampling-settings rust-reasoning-settings rust-routing-mode rust-projects rust-project-chats rust-project-writes project-store"
# Their old switches only earn a warning now (noevia-server says the same), and neither process
# gets them (each would only warn again).
for retired in NOEVIA_RUST_AUTH NOEVIA_RUST_USAGE NOEVIA_RUST_SAMPLING_SETTINGS NOEVIA_RUST_REASONING_SETTINGS NOEVIA_RUST_ROUTING_MODE \
  NOEVIA_RUST_ROUTING_DEFAULT NOEVIA_RUST_STATS NOEVIA_RUST_EXPORT NOEVIA_RUST_CHATGPT_STATUS NOEVIA_RUST_STORAGE_READS \
  NOEVIA_RUST_STORAGE_BROWSE NOEVIA_RUST_PROJECTS NOEVIA_RUST_PROJECT_WRITES; do
  if [ -n "${!retired+set}" ]; then
    echo "web-supervisor: $retired is retired (sbstndalton/noevia#1071) and ignored; remove it from the environment" >&2
    unset "$retired"
  fi
done
# Rust is the only front (sbstndalton/noevia#1071, #2094): unset or empty means rust, matching the image
# default; node and anything else are refused below.
front="${NOEVIA_FRONT:-rust}"
case "$front" in
  rust) ;;
  node) echo "web-supervisor: NOEVIA_FRONT=node is no longer supported: the Rust front owns sign-in and routes Node no longer has (sbstndalton/noevia#1071); set NOEVIA_FRONT=rust" >&2; exit 2 ;;
  *) echo "web-supervisor: NOEVIA_FRONT must be rust, got '$front'" >&2; exit 2 ;;
esac

bin="${NOEVIA_SERVER_BIN:-/usr/local/bin/noevia-server}"
if [ ! -x "$bin" ]; then
  echo "web-supervisor: NOEVIA_FRONT=rust but this image has no noevia-server (its NOEVIA_RS_REF predates bins/noevia-server); update the image" >&2
  exit 2
fi
# The code-network guard (sbstndalton/noevia#1246): with COWORK_CODE_NET_ADDR set, Node behind
# the front sees every request arrive on loopback, so only the front can refuse code-network
# requests. A front that does not report the guard must not face the network.
# (Captured first: with pipefail, grep -q exiting early could fail the pipeline via SIGPIPE.)
if [ -n "${COWORK_CODE_NET_ADDR:-}" ]; then
  features="$("$bin" --features 2>/dev/null || true)"
else
  features=code-net-guard
fi
if ! grep -qx 'code-net-guard' <<<"$features"; then
  echo "web-supervisor: COWORK_CODE_NET_ADDR is set but this noevia-server has no code-net-guard; update the image" >&2
  exit 2
fi
# The client address (noevia-core server/auth.cjs, #1578): Node believes X-Noevia-Client-Ip from its
# loopback peer only when this front drops client copies of it, which a front reports with
# client-address-header. Node is told so with NOEVIA_FRONT_CLIENT_IP=1; a front that predates the
# strip (a core/rs pin mismatch or an rs rollback) leaves Node on the socket address, never a
# header a client chose. Not an error: the address is the proxy's until the image catches up.
client_features="$("$bin" --features 2>/dev/null || true)"
client_ip_confirmed=""
if grep -qx 'client-address-header' <<<"$client_features"; then
  client_ip_confirmed=1
else
  echo "web-supervisor: this noevia-server has no client-address-header; Node ignores X-Noevia-Client-Ip and sees every client as the front" >&2
fi
# Rust-owned sign-in (noevia-core server/rust-auth.cjs), permanent since #1071: Node always refuses
# to write the account tables and no longer answers sign-in, so the front facing the network must.
# A front whose --features does not list rust-auth never starts.
rust_features="$("$bin" --features 2>/dev/null || true)"
if ! grep -qx 'rust-auth' <<<"$rust_features"; then
  echo "web-supervisor: this noevia-server has no rust-auth; Node no longer answers sign-in (sbstndalton/noevia#1071), so update the image" >&2
  exit 2
fi
# The permanent front read routes (#1071): every feature, or nothing starts.
missing=""
for feature in $FRONT_ROUTE_FEATURES; do
  grep -qx -- "$feature" <<<"$rust_features" || missing="$missing $feature"
done
if [ -n "$missing" ]; then
  echo "web-supervisor: this noevia-server lacks the front route feature(s):$missing; Node no longer answers those routes (sbstndalton/noevia#1071), so update the image" >&2
  exit 2
fi
# Compare the baked Core entrypoint with the actual selected binary, independently of
# environment switches. Old/old images remain usable for rollback; mixed owners do not.
feature_core_owner=""
feature_front_owner=""
grep -qF '.createReadOnlyFeatures(' "$PWD/server/index.cjs" 2>/dev/null && feature_core_owner=rust
grep -qx 'rust-feature-owner' <<<"$rust_features" && feature_front_owner=rust
if [ "$feature_core_owner" != "$feature_front_owner" ]; then
  echo "web-supervisor: feature ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi
# The five proxy-only Diary routes (sbstndalton/noevia#2185): a Core that refuses them
# (server/rust-diary.cjs) needs a front that answers them (rust-diary), and a rust-diary front
# needs that Core. Old/old images remain usable for rollback; mixed owners do not.
diary_core_owner=""
diary_front_owner=""
grep -qF "require('./rust-diary.cjs')" "$PWD/server/index.cjs" 2>/dev/null && diary_core_owner=rust
grep -qx 'rust-diary' <<<"$rust_features" && diary_front_owner=rust
if [ "$diary_core_owner" != "$diary_front_owner" ]; then
  echo "web-supervisor: Diary route ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi
# The two configuration-only model reads (sbstndalton/noevia#2187, GET /api/models/capabilities and
# GET /api/models/hardware): a Core that refuses them (server/rust-models.cjs) needs a front that
# answers them (rust-model-reads), and a rust-model-reads front needs that Core. Old/old images
# remain usable for rollback; mixed owners do not.
models_core_owner=""
models_front_owner=""
grep -qF "require('./rust-models.cjs')" "$PWD/server/index.cjs" 2>/dev/null && models_core_owner=rust
grep -qx 'rust-model-reads' <<<"$rust_features" && models_front_owner=rust
if [ "$models_core_owner" != "$models_front_owner" ]; then
  echo "web-supervisor: model read route ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi
# A project source's stored reads (sbstndalton/noevia#2191: GET .../sources/text, .../documents/original
# and .../uploads/original): a Core that refuses them (server/rust-project-reads.cjs) needs a front that
# answers them (rust-project-reads), and a rust-project-reads front needs that Core. Old/old images
# remain usable for rollback; mixed owners do not.
project_reads_core_owner=""
project_reads_front_owner=""
grep -qF "require('./rust-project-reads.cjs')" "$PWD/server/index.cjs" 2>/dev/null && project_reads_core_owner=rust
grep -qx 'rust-project-reads' <<<"$rust_features" && project_reads_front_owner=rust
if [ "$project_reads_core_owner" != "$project_reads_front_owner" ]; then
  echo "web-supervisor: project source read ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi
# A project's create and delete (sbstndalton/noevia#2191 round 2: POST /api/projects, DELETE
# /api/projects/{projectId}): a Core that refuses them (server/rust-project-lifecycle.cjs) needs a front
# that answers them (rust-project-lifecycle), and a rust-project-lifecycle front needs that Core (it also
# publishes the toolbox catalogue the create reads and drains the index outbox). Old/old images remain
# usable for rollback; mixed owners do not.
project_lifecycle_core_owner=""
project_lifecycle_front_owner=""
grep -qF "require('./rust-project-lifecycle.cjs')" "$PWD/server/index.cjs" 2>/dev/null && project_lifecycle_core_owner=rust
grep -qx 'rust-project-lifecycle' <<<"$rust_features" && project_lifecycle_front_owner=rust
if [ "$project_lifecycle_core_owner" != "$project_lifecycle_front_owner" ]; then
  echo "web-supervisor: project create/delete ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi
# A project's storage writes (sbstndalton/noevia#2191 round 3): a Core whose outbox executor claims only
# its own job kinds (server/rust-project-storage.cjs, required by index.cjs) needs a front that runs the
# storage sweep jobs and #687's create-time storage step itself (rust-project-storage), and such a front
# needs that Core (an older one would run the same sweeps). Old/old images remain usable for rollback.
# Since noevia-core b6e2238 index.cjs loads the split through the outbox view it requires
# (server/rust-index-outbox.cjs requires rust-project-storage.cjs), so either chain counts. Each require
# must sit on a code line of the file that loads it; release assembly proves the links strictly.
project_storage_core_owner=""
project_storage_front_owner=""
storage_code_requires() {
  local code
  code="$(grep -vE '^[[:space:]]*(//|/\*|\*)' "$1" 2>/dev/null)" || return 1
  grep -qF "require('./$2')" <<<"$code"
}
if storage_code_requires "$PWD/server/index.cjs" rust-project-storage.cjs \
  || { storage_code_requires "$PWD/server/index.cjs" rust-index-outbox.cjs \
    && storage_code_requires "$PWD/server/rust-index-outbox.cjs" rust-project-storage.cjs; }; then
  project_storage_core_owner=rust
fi
grep -qx 'rust-project-storage' <<<"$rust_features" && project_storage_front_owner=rust
if [ "$project_storage_core_owner" != "$project_storage_front_owner" ]; then
  echo "web-supervisor: project storage write ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi
# A project source file's delete (sbstndalton/noevia#2191 round 3: DELETE /api/projects/{id}/files): a
# Core that refuses it (server/rust-project-files.cjs) and runs the `unindex` jobs needs a front that
# answers it (rust-project-files), and such a front needs that Core. Old/old remains usable for rollback.
project_files_core_owner=""
project_files_front_owner=""
grep -qF "require('./rust-project-files.cjs')" "$PWD/server/index.cjs" 2>/dev/null && project_files_core_owner=rust
grep -qx 'rust-project-files' <<<"$rust_features" && project_files_front_owner=rust
if [ "$project_files_core_owner" != "$project_files_front_owner" ]; then
  echo "web-supervisor: project file delete ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi
# A stored document's page reader (sbstndalton/noevia#2191 round 4: GET /api/projects/{id}/documents/pages):
# a Core that refuses it (server/rust-project-document-pages.cjs) needs a front that answers it
# (rust-project-document-pages), and such a front needs that Core. Old/old remains usable for rollback.
document_pages_core_owner=""
document_pages_front_owner=""
grep -qF "require('./rust-project-document-pages.cjs')" "$PWD/server/index.cjs" 2>/dev/null && document_pages_core_owner=rust
grep -qx 'rust-project-document-pages' <<<"$rust_features" && document_pages_front_owner=rust
if [ "$document_pages_core_owner" != "$document_pages_front_owner" ]; then
  echo "web-supervisor: project document page reader ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi
# The conversation import (sbstndalton/noevia#2191 round 4: POST /api/import/conversations): a Core that
# refuses it (server/rust-conversation-import.cjs) needs a front that answers it (rust-conversation-import),
# and such a front needs that Core. Old/old remains usable for rollback.
conversation_import_core_owner=""
conversation_import_front_owner=""
grep -qF "require('./rust-conversation-import.cjs')" "$PWD/server/index.cjs" 2>/dev/null && conversation_import_core_owner=rust
grep -qx 'rust-conversation-import' <<<"$rust_features" && conversation_import_front_owner=rust
if [ "$conversation_import_core_owner" != "$conversation_import_front_owner" ]; then
  echo "web-supervisor: conversation import ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi
# The project document upload (sbstndalton/noevia#2191 round 5: POST /api/projects/{id}/documents, on the
# front's Docling extraction pipeline): a Core that refuses it (server/rust-project-documents.cjs) needs a
# front that answers it (rust-project-documents), and such a front needs that Core. Old/old stays usable.
project_documents_core_owner=""
project_documents_front_owner=""
grep -qF "require('./rust-project-documents.cjs')" "$PWD/server/index.cjs" 2>/dev/null && project_documents_core_owner=rust
grep -qx 'rust-project-documents' <<<"$rust_features" && project_documents_front_owner=rust
if [ "$project_documents_core_owner" != "$project_documents_front_owner" ]; then
  echo "web-supervisor: project document upload ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi
# The public pages and the device approval page (sbstndalton/noevia#2190, server/rust-public-pages.cjs
# beside the front's rust-public-pages), and the plugin directory (server/rust-plugin-directory.cjs
# beside rust-plugin-directory), and the decision settings (server/rust-decision-settings.cjs beside
# rust-decision-settings; Rust is the decision:configuration row's single writer), and the storage
# integrations (server/rust-storage-integrations.cjs beside rust-storage-integrations; Rust owns the
# Nextcloud sign-in's flows file), and the vault-mirror preferences (server/rust-vault-mirror-preferences.cjs
# beside rust-vault-mirror-preferences; Rust writes the setting and the nudges Node polls), and the
# storage connection save (server/rust-storage-connection.cjs beside rust-storage-connection, round 4;
# Rust is the storage_connections row's only writer and saves Node's legacy WebDAV setup through the
# internal store), the provider probe (server/rust-provider-test.cjs beside rust-provider-test,
# round 4), and Sign in with ChatGPT's token broker (server/rust-chatgpt-oauth.cjs beside
# rust-chatgpt-oauth, round 5; Rust owns the device sign-in, the tokens and their refresh, and Node's
# chat asks it for access tokens through the internal store), and the provider model lists
# (server/rust-provider-models.cjs beside rust-provider-models, round 5), and the provider registry
# (server/rust-providers.cjs beside rust-providers, round 6; Rust is the only writer of providers.json
# and shared-providers.json and Node re-reads them), and its key re-seal (server/rust-providers-reseal.cjs
# beside rust-providers-reseal, round 6 review; a Core that rotates secrets through the front's internal
# store needs a front that answers it, and a front that has it needs that Core, so Node never rewrites a
# provider file the front owns), Google Drive's token authority
# (server/rust-google-drive.cjs beside rust-google-drive, round 7; Rust owns the sign-ins, the sealed refresh tokens
# and tool_policies, and Node's chat tools ask it for access tokens through the internal store), the off-site
# backup (server/rust-offsite-backup.cjs beside rust-offsite-backup, round 7), the MCP server credentials
# (server/rust-mcp-credentials.cjs beside rust-mcp-credentials, sbstndalton/noevia#2202; Rust is the broker of every
# account's OAuth sign-ins and own server keys, the only writer of their three tables, and Node's chat asks it for
# one call's access token or key headers), the close-out GETs (sbstndalton/noevia#2190, #2191): the health
# check (server/rust-health.cjs beside rust-health, which also publishes rag-status.json), the auto-roles read
# (server/rust-auto-roles.cjs beside rust-auto-roles, which publishes auto-roles-snapshot.json) and the workspace
# list (server/rust-workspace.cjs beside rust-workspace), the rest of the Diary routes
# (server/rust-diary-routes.cjs beside rust-diary-routes, sbstndalton/noevia#2206; Rust owns the connector
# credentials table and reads the running-exchange set Node publishes), the Diary local exchange
# (server/rust-diary-local-exchange.cjs beside rust-diary-local-exchange: the front relays it and holds Node's model
# throttle and inference gate through the admission lease's diaryExchange ask, which only that Core grants), since #2202 round 2 the admin MCP directory
# (server/rust-mcp-directory.cjs beside rust-mcp-directory; Rust lists a candidate server's tools itself and is the
# only writer of directory_mcp_servers, Node re-reads it), the permitted-tools view (server/rust-toolboxes-permitted.cjs
# beside rust-toolboxes-permitted) and the connectors list (server/rust-connectors.cjs beside rust-connectors), the last
# two computed by the front from the discovery snapshot Node publishes, a project's configuration and instruction skills
# (server/rust-project-config.cjs beside rust-project-config, sbstndalton/noevia#2191 round 9) and an account's
# deletion (server/rust-admin-users.cjs beside rust-admin-users, #2190: Rust deletes the rows, the folder and every
# credential, and Core can no longer delete an account), deep research (server/rust-research.cjs beside rust-research,
# #2218: the jobs run on the front, Node keeps only their admission lease and tool bridge), the Code and Browser task reads (server/rust-task-reads.cjs beside rust-task-reads, #2257: Node keeps the writes and the facts bridge), the inference budget and installed reads (server/rust-model-state-reads.cjs beside rust-model-state-reads, #2262: Node keeps the PUT and the facts bridge): each group moves as one owner. Old/old images
# remain usable for rollback; mixed owners do not.
while read -r pair_marker pair_capability pair_label; do
  pair_core_owner=""
  pair_front_owner=""
  grep -qF "require('./$pair_marker')" "$PWD/server/index.cjs" 2>/dev/null && pair_core_owner=rust
  grep -qx "$pair_capability" <<<"$rust_features" && pair_front_owner=rust
  if [ "$pair_core_owner" != "$pair_front_owner" ]; then
    echo "web-supervisor: $pair_label route ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
    exit 2
  fi
done <<'PAIRS'
rust-public-pages.cjs rust-public-pages public page
rust-plugin-directory.cjs rust-plugin-directory plugin directory
rust-decision-settings.cjs rust-decision-settings decision settings
rust-storage-integrations.cjs rust-storage-integrations storage integration
rust-vault-mirror-preferences.cjs rust-vault-mirror-preferences vault mirror preference
rust-storage-connection.cjs rust-storage-connection storage connection
rust-provider-test.cjs rust-provider-test provider test
rust-chatgpt-oauth.cjs rust-chatgpt-oauth ChatGPT sign-in
rust-provider-models.cjs rust-provider-models provider model list
rust-providers.cjs rust-providers provider registry
rust-providers-reseal.cjs rust-providers-reseal provider key reseal
rust-google-drive.cjs rust-google-drive Google Drive
rust-offsite-backup.cjs rust-offsite-backup off-site backup
rust-mcp-credentials.cjs rust-mcp-credentials MCP credential
rust-health.cjs rust-health health check
rust-auto-roles.cjs rust-auto-roles auto-roles read
rust-workspace.cjs rust-workspace workspace list
rust-chat-search.cjs rust-chat-search chat search
rust-chat-export.cjs rust-chat-export chat download
rust-diary-routes.cjs rust-diary-routes Diary connector and recovery
rust-diary-local-exchange.cjs rust-diary-local-exchange Diary local exchange
rust-mcp-directory.cjs rust-mcp-directory MCP directory
rust-toolbox-picker.cjs rust-toolbox-picker toolbox picker
rust-toolboxes-permitted.cjs rust-toolboxes-permitted permitted tools
rust-connectors.cjs rust-connectors connectors
rust-project-config.cjs rust-project-config project configuration
rust-admin-users.cjs rust-admin-users account deletion
rust-research.cjs rust-research deep research
rust-task-reads.cjs rust-task-reads task read
rust-autotune-read.cjs rust-autotune-read auto-tune status GET
rust-idle-unload-read.cjs rust-idle-unload-read idle unload read
rust-model-assignments.cjs rust-model-assignments model assignments read
rust-model-downloads-read.cjs rust-model-downloads-read model download GET
rust-preset-read.cjs rust-preset-read preset GET
rust-autotune-resume.cjs rust-autotune-resume auto-tune resume
rust-autotune-cancel.cjs rust-autotune-cancel auto-tune cancellation
rust-autotune-clear.cjs rust-autotune-clear autotune dismissal
rust-model-manager-scan.cjs rust-model-manager-scan model folder scan read
rust-calibration-cancel.cjs rust-calibration-cancel calibration cancellation
rust-autotune-untuned.cjs rust-autotune-untuned untuned discovery
rust-calibration-routes.cjs rust-calibration-routes calibration start/read
rust-autotune-settings.cjs rust-autotune-settings auto-tune settings
rust-tool-approvals.cjs rust-tool-approvals tool approval decision
rust-model-evidence-recheck.cjs rust-model-evidence-recheck model evidence recheck
rust-model-state-reads.cjs rust-model-state-reads model state read
rust-chat-framing-suggest.cjs rust-chat-framing-suggest framing suggestion
PAIRS
# Inference machines (#1638): Core's native-owner clients are marked by MACHINES_CLIENT_MARKER, exported by
# server/rust-inference-machines.cjs (noevia-core 16e260f loads it from routes/inference-machines.cjs, never
# from index.cjs), the marker release assembly pairs with the front's rust-inference-machines. Old/old
# images remain usable for rollback; mixed owners do not.
machines_core_owner="$(node -e "try { const m = require('./server/rust-inference-machines.cjs'); if (m.MACHINES_CLIENT_MARKER === 'noevia-inference-machines-client/1') process.stdout.write('rust'); } catch {}" 2>/dev/null || true)"
machines_front_owner=""
grep -qx 'rust-inference-machines' <<<"$rust_features" && machines_front_owner=rust
if [ "$machines_core_owner" != "$machines_front_owner" ]; then
  echo "web-supervisor: inference machines route ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi
# F1: compare the actual Core export with exact runtime capability entries.
auto_roles_core_owner() {
  [ -f "$1" ] || { echo none; return; }
  command -v node >/dev/null 2>&1 || { echo invalid; return; }
  local auto_roles_bound="timeout -k 2 10"
  command -v timeout >/dev/null 2>&1 || { echo invalid; return; }
  $auto_roles_bound node -e '
    const fs = require("node:fs"), vm = require("node:vm");
    const src = fs.readFileSync(process.argv[1], "utf8");
    let value;
    try {
      // Invocation and export access both run inside the timed evaluation.
      value = vm.runInNewContext(
        "(function () {" +
        "  const stub = new Proxy(function () {}, { get: () => stub, apply: () => stub, construct: () => stub });" +
        "  const module = { exports: {} };" +
        "  (function (require, module, exports) {" + src + "\n})(() => stub, module, module.exports);" +
        "  const v = module.exports.RUST_AUTO_ROLES_WRITE_OWNED;" +
        "  return v === undefined ? undefined : typeof v === \"string\" ? v : null;" +
        "})()", {}, { timeout: 2000 });
    } catch { process.stdout.write("invalid"); process.exit(0); }
    if (value === undefined) { process.stdout.write("invalid"); process.exit(0); }
    const markers = src.match(/["\x27]rust-auto-roles-write[^"\x27\r\n]*["\x27]/g) || [];
    process.stdout.write(value === "rust-auto-roles-write" && markers.length === 1 ? "rust" : "invalid");
  ' "$1" 2>/dev/null || echo invalid
}
auto_roles_write_owner="$(auto_roles_core_owner "$PWD/server/rust-auto-roles-write.cjs")"
auto_roles_write_front=none
while IFS= read -r marker; do
  case "$marker" in
    *rust-auto-roles-write*)
      if [ "$marker" = rust-auto-roles-write ] && [ "$auto_roles_write_front" = none ]; then auto_roles_write_front=rust
      else auto_roles_write_front=invalid; fi ;;
  esac
done <<<"$rust_features"
if [ "$auto_roles_write_owner" = invalid ] || [ "$auto_roles_write_front" = invalid ] || [ "$auto_roles_write_owner" != "$auto_roles_write_front" ]; then
  echo "web-supervisor: auto-roles write route ownership mismatch or ambiguous capability marker; update the paired image" >&2
  exit 2
fi
# The two read-only model sizing reads (sbstndalton/noevia#2189, GET /api/models/estimate and GET
# /api/models/preset/suggest): a Core that refuses them (server/rust-model-sizing.cjs) needs a front
# that answers them (rust-model-sizing), and a rust-model-sizing front needs that Core. Old/old images
# remain usable for rollback; mixed owners do not.
sizing_core_owner=""
sizing_front_owner=""
grep -qF "require('./rust-model-sizing.cjs')" "$PWD/server/index.cjs" 2>/dev/null && sizing_core_owner=rust
grep -qx 'rust-model-sizing' <<<"$rust_features" && sizing_front_owner=rust
if [ "$sizing_core_owner" != "$sizing_front_owner" ]; then
  echo "web-supervisor: model sizing route ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi

# The model maintenance routes (sbstndalton/noevia#2210: POST /api/models/presets/reload, GET
# /api/models/evidence, POST /api/models/evidence/import): a Core that refuses them
# (server/rust-model-maintenance.cjs) needs a front that answers them (rust-model-maintenance), and a
# rust-model-maintenance front needs that Core. The front is the single writer of the reload guard state
# file and of the evidence store, and Core's producers queue their records for it, so a mixed pair would
# have two writers (or none). Old/old images remain usable for rollback; mixed owners do not.
maintenance_core_owner=""
maintenance_front_owner=""
grep -qF "require('./rust-model-maintenance.cjs')" "$PWD/server/index.cjs" 2>/dev/null && maintenance_core_owner=rust
grep -qx 'rust-model-maintenance' <<<"$rust_features" && maintenance_front_owner=rust
if [ "$maintenance_core_owner" != "$maintenance_front_owner" ]; then
  echo "web-supervisor: model maintenance route ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi

# A new front beside old Core stays idle (proof 404). A new Core must have the
# native owner capability before either process is launched.
embedding_core_owner="$(node -e "try { const m = require('./server/rust-embedding-background.cjs'); if (m.RUST_EMBEDDING_OWNED === 'rust-embedding-background-v1') process.stdout.write('rust'); } catch {}" 2>/dev/null || true)"
if grep -qF "require('./rust-embedding-background.cjs')" "$PWD/server/index.cjs" 2>/dev/null && [ "$embedding_core_owner" != rust ]; then
  echo "web-supervisor: invalid embedding background ownership marker" >&2
  exit 2
fi
if [ "$embedding_core_owner" = rust ] && ! grep -qx 'rust-embedding-background-v1' <<<"$rust_features"; then
  echo "web-supervisor: embedding background requires rust-embedding-background-v1; update the paired image" >&2
  exit 2
fi

# The chat journal tail and the mirror and brain schedules (rust-chat-mirror-v1): the front reads each active
# account's change journal in process and owns the debounce, backoff, idle timers and queue; the core keeps only
# their two work executors (server/rust-chat-mirror.cjs) and nudges the front. A core that leaves the schedule to
# the front beside a front without it would leave the journal unread; a front with it beside a core that still
# tails the journal would run a second schedule. Either mix is refused before any process starts.
chat_mirror_core_owner="$(node -e "try { const m = require('./server/rust-chat-mirror.cjs'); if (m.RUST_CHAT_MIRROR_OWNED === 'rust-chat-mirror-v1') process.stdout.write('rust'); } catch {}" 2>/dev/null || true)"
if grep -qF "require('./rust-chat-mirror.cjs')" "$PWD/server/index.cjs" 2>/dev/null && [ "$chat_mirror_core_owner" != rust ]; then
  echo "web-supervisor: invalid chat mirror ownership marker" >&2
  exit 2
fi
chat_mirror_front_owner=""
grep -qx 'rust-chat-mirror-v1' <<<"$rust_features" && chat_mirror_front_owner=rust
if [ "$chat_mirror_core_owner" != "$chat_mirror_front_owner" ]; then
  echo "web-supervisor: chat mirror schedule ownership mismatch between noevia-server and noevia-core (rust-chat-mirror-v1); update the paired image" >&2
  exit 2
fi

# The source migration runs before the native project store serves. Reject either
# half-pair before launching processes so an older process cannot race the sweep.
source_startup_core=none
if [ -f "$PWD/server/rust-source-startup.cjs" ]; then
  source_startup_core="$(timeout -k 2 10 node -e '
    const fs=require("node:fs"),vm=require("node:vm");
    try {
      const src=fs.readFileSync(process.argv[1],"utf8");
      const value=vm.runInNewContext("const module={exports:{}};"+src+"\nmodule.exports.RUST_SOURCE_STARTUP_OWNED",{}, {timeout:2000});
      process.stdout.write(value === "rust-source-startup-v1" ? "v1" : "invalid");
    } catch { process.stdout.write("invalid"); }
  ' "$PWD/server/rust-source-startup.cjs" 2>/dev/null)" || source_startup_core=invalid
fi
source_startup_front=none
grep -qx 'rust-source-startup-v1' <<<"$rust_features" && source_startup_front=v1
if [ "$source_startup_core" = invalid ] || [ "$source_startup_core" != "$source_startup_front" ]; then
  echo "web-supervisor: source startup ownership mismatch; update the paired image" >&2
  exit 2
fi

dav_listener_core=none
if [ -f "$PWD/server/rust-dav-listener.cjs" ]; then
  dav_listener_core="$(timeout -k 2 10 node -e '
    const fs=require("node:fs"),vm=require("node:vm");
    try {
      const src=fs.readFileSync(process.argv[1],"utf8");
      const value=vm.runInNewContext("const module={exports:{}};"+src+"\nmodule.exports.RUST_DAV_LISTENER_OWNED",{}, {timeout:2000});
      process.stdout.write(value === "rust-dav-listener-v1" ? "v1" : "invalid");
    } catch { process.stdout.write("invalid"); }
  ' "$PWD/server/rust-dav-listener.cjs" 2>/dev/null)" || dav_listener_core=invalid
fi
dav_listener_front=none
grep -qx 'rust-dav-listener-v1' <<<"$rust_features" && dav_listener_front=v1
if [ "$dav_listener_core" = invalid ] || [ "$dav_listener_core" != "$dav_listener_front" ]; then
  echo "web-supervisor: DAV listener ownership mismatch; update the paired image" >&2
  exit 2
fi

# The preset reload baseline (noevia-rs model_maint/reload.rs): a Core that asks the front for it
# (server/preset-reload.cjs createBridgeReloadGuard, marker noevia-preset-baseline-bridge/1) needs a
# front that holds it (rust-preset-baseline). One direction only: such a Core falls back to its own
# guard when the front's internal listener does not answer, but a mismatched pair is refused here so
# a manually mixed install never runs that way; a rust-preset-baseline front beside an older Core is
# shared mode (the older Core writes the same file under the same lock) and starts.
preset_baseline_core=""
grep -qF "'noevia-preset-baseline-bridge/1'" "$PWD/server/preset-reload.cjs" 2>/dev/null && preset_baseline_core=bridge
if [ "$preset_baseline_core" = bridge ] && ! grep -qx 'rust-preset-baseline' <<<"$rust_features"; then
  echo "web-supervisor: this noevia-core asks the front for the preset reload baseline, but this noevia-server has no rust-preset-baseline; update the paired image" >&2
  exit 2
fi

# The three web-address GETs are a paired owner change. The front's compiled capability
# and core's exported marker must agree before either process starts; source pins alone cannot
# protect a manually mixed installed pair. Neither result comes from an inherited environment.
web_address_front_owner=""
grep -qx 'rust-web-address-read' <<<"$rust_features" && web_address_front_owner=rust
web_address_core_owner="$(node -e "try { const m = require('./server/rust-web-address.cjs'); if (m.WEB_ADDRESS_READ_OWNER === 'noevia-web-address-read-rust/1') process.stdout.write('rust'); } catch {}" 2>/dev/null || true)"
if [ "$web_address_front_owner" != "$web_address_core_owner" ]; then
  echo "web-supervisor: web-address GET ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi
# Both POST actions move together. A binary/core mismatch is refused before either starts.
web_write_front_owner=""
grep -qx 'rust-web-address-write' <<<"$rust_features" && web_write_front_owner=rust
web_write_core_owner="$(node -e "try { const m = require('./server/rust-web-address.cjs'); if (m.WEB_ADDRESS_WRITE_OWNER === 'noevia-web-address-write-rust/1') process.stdout.write('rust'); } catch {}" 2>/dev/null || true)"
if [ "$web_write_front_owner" != "$web_write_core_owner" ]; then
  echo "web-supervisor: web-address POST ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi
# Diary backup ownership is compiled into the paired artifacts, never inherited from the environment.
diary_backup_front_owner=""
grep -qx 'rust-diary-backup-worker' <<<"$rust_features" && diary_backup_front_owner=rust
diary_backup_core_owner="$(node -e "try { const m = require('./server/rust-diary.cjs'); if (m.DIARY_BACKUP_WORKER_OWNER === 'noevia-diary-backup-worker-rust/1') process.stdout.write('rust'); } catch {}" 2>/dev/null || true)"
if [ "$diary_backup_front_owner" != "$diary_backup_core_owner" ]; then
  echo "web-supervisor: diary backup worker ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi

# The inference budget watch moves with paired exported/compiled markers. Refuse mixed owners.
# Root accepts legacy rollback ignoring leftover fences: like a legacy Node restart it loses
# the in-memory quarantine. Old releases ignore the directory; its files never gate rollback.
budget_watch_front_owner=""
grep -qx 'rust-budget-watch' <<<"$rust_features" && budget_watch_front_owner=v1
grep -qx 'rust-budget-watch-v2' <<<"$rust_features" && budget_watch_front_owner=v2
grep -qx 'rust-budget-watch-v3' <<<"$rust_features" && budget_watch_front_owner=v3
budget_watch_core_owner="$(node -e "try { const m = require('./server/rust-budget-watch.cjs'); if (m.BUDGET_WATCH_OWNER === 'noevia-budget-watch-rust/1') process.stdout.write('v1'); else if (m.BUDGET_WATCH_OWNER === 'noevia-budget-watch-rust/2') process.stdout.write('v2'); else if (m.BUDGET_WATCH_OWNER === 'noevia-budget-watch-rust/3') process.stdout.write('v3'); } catch {}" 2>/dev/null || true)"
if [ "$budget_watch_front_owner" != "$budget_watch_core_owner" ]; then
  echo "web-supervisor: budget watch ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi

# The calibration job itself (sbstndalton/noevia#2303: its state file, boot recovery and maintenance hold) is
# the front's when it lists the worker and Core exports CALIBRATION_WORKER_OWNER (Core then builds no public
# calibrator and does not recover the job at boot). A mixed pair would recover one state file twice at a joint
# restart, or never. Generation 2 (rust-calibration-worker-v2, noevia-calibration-worker-rust/2) has every load
# the job asks for decided by Core first, so a generation pairs only with itself. Neither result comes from an
# inherited environment.
calibration_worker_front_owner=""
grep -qx 'rust-calibration-worker' <<<"$rust_features" && calibration_worker_front_owner=v1
grep -qx 'rust-calibration-worker-v2' <<<"$rust_features" && calibration_worker_front_owner=v2
calibration_worker_core_owner="$(node -e "try { const m = require('./server/rust-calibration-routes.cjs'); if (m.CALIBRATION_WORKER_OWNER === 'noevia-calibration-worker-rust/1') process.stdout.write('v1'); else if (m.CALIBRATION_WORKER_OWNER === 'noevia-calibration-worker-rust/2') process.stdout.write('v2'); } catch {}" 2>/dev/null || true)"
if [ "$calibration_worker_front_owner" != "$calibration_worker_core_owner" ]; then
  echo "web-supervisor: calibration worker ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi
# Source job journals are a paired internal-store capability: this Node client has no local
# source journal fallback. Probe the installed client export and the actual front binary before
# either process starts. The release assembler also checks their canonical source declarations.
source_client_file="$PWD/server/project-store-client.cjs"
source_client_marker="$(node -e 'const c=require(process.argv[1]); if(c.SOURCE_JOBS_CLIENT_MARKER) process.stdout.write(c.SOURCE_JOBS_CLIENT_MARKER)' "$source_client_file" 2>/dev/null || true)"
if { grep -qE 'SOURCE_JOBS_CLIENT_MARKER|sourceJobsFor\(' "$source_client_file" 2>/dev/null \
     || grep -qF 'workspace.sourceJobs' "$PWD/server/source-jobs.cjs" 2>/dev/null; } \
   && [ "$source_client_marker" != 'noevia-source-jobs-client/1' ]; then
  echo "web-supervisor: core declares a source job store client but does not export its canonical capability" >&2
  exit 2
fi
if [ -n "$source_client_marker" ] && [ "$source_client_marker" != 'noevia-source-jobs-client/1' ]; then
  echo "web-supervisor: unknown source job client capability" >&2
  exit 2
fi
source_store_reported=""
if grep -qx 'noevia-source-jobs-store/1' <<<"$rust_features"; then source_store_reported=1; fi
if [ -n "$source_client_marker" ] && [ -z "$source_store_reported" ]; then
  echo "web-supervisor: core source job client requires noevia-source-jobs-store/1 from this noevia-server" >&2
  exit 2
fi
if [ -z "$source_client_marker" ] && [ -n "$source_store_reported" ]; then
  echo "web-supervisor: noevia-server source job store requires a paired core source job client" >&2
  exit 2
fi
# The source jobs (sbstndalton/noevia#2191 round 6): the front becomes the one owning process of the
# source-job store and Node keeps no source journal client. A Core that refuses the background starts
# and the poll (server/rust-source-jobs.cjs) needs a front that answers them (rust-source-jobs), and
# such a front needs that Core. The older pair (Node's client, the front's internal store) is checked
# above; old/old stays usable.
source_jobs_core_owner=""
source_jobs_front_owner=""
grep -qF "require('./rust-source-jobs.cjs')" "$PWD/server/index.cjs" 2>/dev/null && source_jobs_core_owner=rust
grep -qx 'rust-source-jobs' <<<"$rust_features" && source_jobs_front_owner=rust
if [ "$source_jobs_core_owner" != "$source_jobs_front_owner" ]; then
  echo "web-supervisor: source job ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi
# The project source operations (sbstndalton/noevia#2191 round 8): the upload, the folder sync and the folder
# import's plan run in the front, with no worker call to Node. A Core that refuses them
# (server/rust-project-source-work.cjs) and reads the front's storage latch and file-operation list needs a
# front that runs them (rust-project-source-work), and such a front needs that Core. Old/old stays usable.
source_work_core_owner=""
source_work_front_owner=""
grep -qF "require('./rust-project-source-work.cjs')" "$PWD/server/index.cjs" 2>/dev/null && source_work_core_owner=rust
grep -qx 'rust-project-source-work' <<<"$rust_features" && source_work_front_owner=rust
if [ "$source_work_core_owner" != "$source_work_front_owner" ]; then
  echo "web-supervisor: project source operations ownership mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi
# In that generation Core keeps no worker path: a Core that refuses the source operations but still ships the
# worker call (its registry, folder import, PDF reduction or the worker header) would run them a second time.
if [ "$source_work_core_owner" = rust ]; then
  for worker_file in source-work.cjs folder-import.cjs pdf-reduce.cjs; do
    if [ -e "$PWD/server/$worker_file" ]; then
      echo "web-supervisor: project source operations ownership mismatch between noevia-server and noevia-core (Core still carries server/$worker_file); update the paired image" >&2
      exit 2
    fi
  done
  if grep -qiF 'x-noevia-source-work' "$PWD/server/routes/projects.cjs" "$PWD/server/index.cjs" 2>/dev/null; then
    echo "web-supervisor: project source operations ownership mismatch between noevia-server and noevia-core (Core still answers the worker call); update the paired image" >&2
    exit 2
  fi
fi
# The large-document lock (sbstndalton/noevia#2191 round 7, #697): a core whose large-document-lock.cjs
# also takes the host-wide lock file (large-document-file-lock.cjs) needs a front that takes it too
# (rust-large-document-host-lock), and the reverse; otherwise the two could each hold a document over
# 25 MiB at once. Old/old stays usable.
large_lock_core_owner=""
large_lock_front_owner=""
grep -qF "require('./large-document-file-lock.cjs')" "$PWD/server/large-document-lock.cjs" 2>/dev/null && large_lock_core_owner=host
grep -qx 'rust-large-document-host-lock' <<<"$rust_features" && large_lock_front_owner=host
if [ "$large_lock_core_owner" != "$large_lock_front_owner" ]; then
  echo "web-supervisor: large-document lock mismatch between noevia-server and noevia-core; update the paired image" >&2
  exit 2
fi
# #2027: Node's legacy workspace prologue delegates only to the actual selected front
# when that binary advertises the filesystem-only helper. Never inherit an arbitrary path.
legacy_claim_bin=""
if grep -qx 'legacy-root-claim' <<<"$rust_features"; then
  case "$bin" in
    /*) legacy_claim_bin="$bin" ;;
    *) echo "web-supervisor: legacy-root-claim requires an absolute NOEVIA_SERVER_BIN" >&2; exit 2 ;;
  esac
fi
port="${UI_PORT:-8021}"
if ! [[ "$port" =~ ^[0-9]+$ ]] || [ "$port" -lt 1 ] || [ "$port" -gt 64535 ]; then
  echo "web-supervisor: UI_PORT '$port' leaves no room for the legacy port UI_PORT+1000" >&2
  exit 2
fi
legacy=$((port + 1000))

# The project store (sbstndalton/noevia#1264): permanent. Node writes projects.json and
# free-chats.json only through the front's loopback store and refuses to start without it, so it is
# always set up: at UI_PORT+2000, with a secret made here for this start and handed to both
# processes (never taken from the environment). The front lists project-store (FRONT_ROUTE_FEATURES).
store_port=$((port + 2000))
if [ "$store_port" -gt 65535 ]; then
  echo "web-supervisor: UI_PORT '$port' leaves no room for the project store port UI_PORT+2000" >&2
  exit 2
fi
store_token="$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')"
if [ "${#store_token}" -ne 64 ]; then
  echo "web-supervisor: could not make the project store secret" >&2
  exit 2
fi

# The start id (sbstndalton/noevia#1071): one random id per container start, made here and handed
# to both processes (never taken from the environment). Node writes it into a running reply's
# partial file (core chat-partial.cjs `running: { start }`), so the front can tell a reply still
# running in this start from one a crash or restart cut off; a front that reads it (--features
# start-id) refuses to start beside Node without it.
start_id="$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')"
if ! [[ "$start_id" =~ ^[0-9a-f]{32}$ ]]; then
  echo "web-supervisor: could not make the start id" >&2
  exit 2
fi

# A native sharing pair binds DAV in the front using the original environment.
# Only rollback pairs with the old listener need Node on UI_HOST; the native
# pair keeps the legacy listener loopback-only even with sharing configured.
node_host="127.0.0.1"
if [ "$dav_listener_core" != v1 ] && [ -n "${COWORK_DAV_PORT:-}" ] && [ "${COWORK_DAV_PORT}" != 0 ]; then
  node_host="${UI_HOST:-0.0.0.0}"
  echo "web-supervisor: COWORK_DAV_PORT is set; Node's legacy port $legacy listens on $node_host, not loopback only" >&2
fi

# Serialize coherent pair lifetimes, including rollback to an older Core. Keep
# the fd inherited by both children: a replacement cannot start before both exit.
# Never unlink this inode, including after an unclean shutdown.
pair_data="${UI_DATA_DIR:-$PWD/server/ui-data}"
mkdir -p "$pair_data" || exit 2
exec 9>"$pair_data/embedding-pair.lock" || exit 2
if ! flock -n 9; then
  echo "web-supervisor: previous Core/front pair is not quiescent" >&2
  exit 2
fi

node_pid="" front_pid="" signalled=0 stopping=0
stop() {
  [ "$stopping" = 1 ] && return 0
  stopping=1
  # Required rollback order: stop/join the native owner before stopping Core.
  # Revoking the proof cannot retract a previously admitted native batch.
  if [ -n "$front_pid" ]; then
    kill -TERM "$front_pid" 2>/dev/null || true
    # Reserve Core's 3s shutdown-hook budget inside Docker's default 10s stop.
    # Force a stuck front to exit before signalling Core; never overlap owners.
    local front_deadline=$((SECONDS + 5))
    while kill -0 "$front_pid" 2>/dev/null; do
      if [ "$SECONDS" -ge "$front_deadline" ]; then
        echo "web-supervisor: front stop deadline reached; killing the front before stopping Core" >&2
        kill -KILL "$front_pid" 2>/dev/null || true
        break
      fi
      sleep 0.05
    done
    wait "$front_pid" 2>/dev/null || true
  fi
  [ -n "$node_pid" ] && kill -TERM "$node_pid" 2>/dev/null
  return 0
}
trap 'signalled=1; stop' TERM INT

UI_HOST="$node_host" UI_PORT="$legacy" \
  NOEVIA_FRONT_CLIENT_IP="$client_ip_confirmed" \
  NOEVIA_LEGACY_CLAIM_BIN="$legacy_claim_bin" \
  NOEVIA_PROJECT_STORE_PORT="$store_port" NOEVIA_PROJECT_STORE_TOKEN="$store_token" \
  NOEVIA_START_ID="$start_id" \
  node server/index.cjs &
node_pid=$!
NOEVIA_PROJECT_STORE_PORT="$store_port" NOEVIA_PROJECT_STORE_TOKEN="$store_token" \
  NOEVIA_START_ID="$start_id" NOEVIA_LEGACY_UPSTREAM="http://127.0.0.1:$legacy" "$bin" &
front_pid=$!

# Whichever exits first (or a signal) ends the wait.
wait -n "$node_pid" "$front_pid"
code=$?
if [ "$signalled" = 0 ]; then
  if kill -0 "$node_pid" 2>/dev/null; then which=noevia-server; else which=node; fi
  echo "web-supervisor: $which exited ($code); stopping the other" >&2
fi
stop
wait "$node_pid" 2>/dev/null
wait "$front_pid" 2>/dev/null
# docker stop: a clean exit. Anything else: non-zero, even when the process that left said 0.
[ "$signalled" = 1 ] && exit 0
[ "$code" = 0 ] && code=1
exit "$code"
