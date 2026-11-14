# Verbatim lines of noevia deploy/tools/assemble-release.sh at noevia origin/main f23eb731: the project storage write
# pairing and the run of route pairings from the close-out routes to the evidence recheck (council p13). Each '#@<name>'
# section is a contiguous run of the script (tests/route_pairings.rs checks it when ASSEMBLE_RELEASE_SH is set).
#@project_storage
# A project's storage writes (sbstndalton/noevia#2191 round 3) move as one owner: Core's
# rust-project-storage.cjs job-kind split and the front's rust-project-storage capability. No route moves
# with it; it builds on the create/delete pair, which must be Rust's too. Core loads the split either
# straight from index.cjs or, since noevia-core b6e2238, through the read-only outbox view that index.cjs
# loads (index.cjs requires rust-index-outbox.cjs, which requires rust-project-storage.cjs). Every link
# is a real require call in code (core_requires): a comment or a string that spells it is not one, and
# neither is a require in an outbox file index.cjs does not load. An unscannable link refuses.
project_storage_core_owner=""
project_storage_rs_owner=""
ps_direct="$(core_requires "$tree/core/server/index.cjs" ./rust-project-storage.cjs)"
ps_outbox="$(core_requires "$tree/core/server/index.cjs" ./rust-index-outbox.cjs)"
ps_chain=no
[ "$ps_outbox" = yes ] && ps_chain="$(core_requires "$tree/core/server/rust-index-outbox.cjs" ./rust-project-storage.cjs)"
case "$ps_direct $ps_outbox $ps_chain" in
  *invalid*) die "project storage write ownership pairing: Core wiring could not be read" ;;
esac
{ [ "$ps_direct" = yes ] || [ "$ps_chain" = yes ]; } && project_storage_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-project-storage features; then
  project_storage_rs_owner=rust
fi
[ "$project_storage_core_owner" = "$project_storage_rs_owner" ] \
  || die "project storage write ownership pairing: Core and Rust must move together"
if [ "$project_storage_rs_owner" = rust ] && [ "$project_lifecycle_rs_owner" != rust ]; then
  die "project storage write ownership pairing: needs the project create/delete pair"
fi
#@close_out
# The close-out routes: GET /api/health and GET /api/auto-roles (sbstndalton/noevia#2190) and GET
# /api/workspace (#2191). Each is Core's refusal (server/rust-health.cjs, rust-auto-roles.cjs,
# rust-workspace.cjs, which also publish rag-status.json and auto-roles-snapshot.json for the front) beside
# the front's capability of the same name, with exactly the GET of the path Rust's in routes.toml (the
# other methods stay Node's: the auto-roles PUT warms the roles in the engine). Old/old stays valid; a
# mixed pair is refused, so a front never answers from a snapshot nobody publishes and a Core never
# refuses a route nobody answers.
get_only_pair() {
  local marker=$1 capability=$2 label=$3 route=$4 core_owner="" rs_owner="" owned
  grep -qF "require('./$marker')" "$tree/core/server/index.cjs" 2>/dev/null && core_owner=rust
  if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES "$capability" features; then
    rs_owner=rust
  fi
  [ "$core_owner" = "$rs_owner" ] \
    || die "$label ownership pairing: Core and Rust must move together"
  if [ "$rs_owner" = rust ]; then
    [ -f "$tree/rs/contracts/http/routes.toml" ] || die "$label ownership pairing: missing route contract"
    owned="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' -v want="$route" -v put="${auto_roles_write_owner:-}" '
      $1 == want {
        if ($2 != "rust" || $3 != "-") exit 1
        n = split($4, m, /[^A-Z]+/); k = 0; g = 0
        for (i = 1; i <= n; i++) if (m[i] != "") { if (m[i] == "GET") g++; k++ }
        if (want == "/api/auto-roles" && put == "rust") {
          p = 0; for (i = 1; i <= n; i++) if (m[i] == "PUT") p++
          if (k != 2 || g != 1 || p != 1) exit 1
        } else if (k != 1 || g != 1) exit 1
        print $1
      }')" || die "$label ownership pairing: exactly GET of $route must belong to Rust"
    [ "$owned" = "$route" ] || die "$label ownership pairing: incomplete route ownership"
  fi
}
get_only_pair rust-health.cjs rust-health "health check" /api/health
get_only_pair rust-auto-roles.cjs rust-auto-roles "auto-roles read" /api/auto-roles
get_only_pair rust-workspace.cjs rust-workspace "workspace list" /api/workspace
# The chat search (#1450) and one chat's Markdown download (#1471), each its own pair of Core refusal
# (server/rust-chat-search.cjs, rust-chat-export.cjs) and front capability, exactly the GET Rust's. A paired
# Core no longer carries the handler in routes/chat-lists.cjs and refuses exactly that GET.
get_only_pair rust-chat-search.cjs rust-chat-search "chat search" /api/chats/search
get_only_pair rust-chat-export.cjs rust-chat-export "chat download" "/api/chats/{chatId}/export"
if grep -qF "require('./rust-chat-search.cjs')" "$tree/core/server/index.cjs" 2>/dev/null; then
  code_has "$tree/core/server/rust-chat-search.cjs" "^const OWNED_ROUTES = Object[.]freeze[(]new Set[(][[]'GET /api/chats/search'[]][)][)];$" \
    || die "chat search ownership pairing: Core must refuse exactly this GET"
  ! code_has "$tree/core/server/routes/chat-lists.cjs" "p === '/api/chats/search'" \
    || die "chat search ownership pairing: Core still carries the public search handler"
fi
if grep -qF "require('./rust-chat-export.cjs')" "$tree/core/server/index.cjs" 2>/dev/null; then
  code_has "$tree/core/server/rust-chat-export.cjs" "^const OWNED_ROUTES = Object[.]freeze[(]new Set[(][[]'GET /api/chats/[{]chatId[}]/export'[]][)][)];$" \
    || die "chat download ownership pairing: Core must refuse exactly this GET"
  ! code_has "$tree/core/server/routes/chat-lists.cjs" 'chatMarkdown[(]' \
    || die "chat download ownership pairing: Core still carries the public download handler"
fi
#@decision_settings
# The decision settings (sbstndalton/noevia#2190): Core's rust-decision-settings.cjs refusal and the
# front's rust-decision-settings capability (Rust the single writer of decision:configuration), every
# method of both routes Rust's in routes.toml.
decision_core_owner=""
decision_rs_owner=""
grep -qF "require('./rust-decision-settings.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && decision_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-decision-settings features; then
  decision_rs_owner=rust
fi
[ "$decision_core_owner" = "$decision_rs_owner" ] \
  || die "decision settings ownership pairing: Core and Rust must move together"
if [ "$decision_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "decision settings ownership pairing: missing route contract"
  decision_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    $1 == "/api/admin/decision-settings" || $1 == "/api/admin/decision-settings/test" {
      if ($2 != "rust" || $3 != "-" || $4 != "-") exit 1
      print $1
    }' | sort)" || die "decision settings ownership pairing: all methods must belong to Rust"
  decision_expected="$(printf '%s\n' /api/admin/decision-settings /api/admin/decision-settings/test | sort)"
  [ "$decision_routes" = "$decision_expected" ] || die "decision settings ownership pairing: incomplete route ownership"
fi
#@storage_integrations
# The storage integrations (sbstndalton/noevia#2190, round 3): Core's rust-storage-integrations.cjs
# refusal and the front's rust-storage-integrations capability (the folder write, the connection test
# and the Nextcloud sign-in, whose flows file Rust owns), exactly POST of all five routes Rust's in
# routes.toml (Node answered only POST there; the other methods stay Node's).
storage_core_owner=""
storage_rs_owner=""
grep -qF "require('./rust-storage-integrations.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && storage_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-storage-integrations features; then
  storage_rs_owner=rust
fi
[ "$storage_core_owner" = "$storage_rs_owner" ] \
  || die "storage integration ownership pairing: Core and Rust must move together"
if [ "$storage_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "storage integration ownership pairing: missing route contract"
  storage_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    $1 == "/api/integrations/storage/folder" || $1 == "/api/integrations/storage/test" ||
    $1 == "/api/integrations/storage/nextcloud/start" || $1 == "/api/integrations/storage/nextcloud/poll" ||
    $1 == "/api/integrations/storage/nextcloud/cancel" {
      if ($2 != "rust" || $3 != "-") exit 1
      n = split($4, m, /[^A-Z]+/); k = 0
      for (i = 1; i <= n; i++) if (m[i] != "") { if (m[i] != "POST") exit 1; k++ }
      if (k != 1) exit 1
      print $1
    }' | sort)" || die "storage integration ownership pairing: exactly POST of all five routes must belong to Rust"
  storage_expected="$(printf '%s\n' /api/integrations/storage/folder /api/integrations/storage/test /api/integrations/storage/nextcloud/start /api/integrations/storage/nextcloud/poll /api/integrations/storage/nextcloud/cancel | sort)"
  [ "$storage_routes" = "$storage_expected" ] || die "storage integration ownership pairing: incomplete route ownership"
fi
#@vault_mirror
# The vault-mirror preferences (#2190, round 3): Core's rust-vault-mirror-preferences.cjs refusal (every
# method; Node's route answered them all, 405 for the others) and the front's
# rust-vault-mirror-preferences capability, with the path Rust's for every method in routes.toml.
mirror_core_owner=""
mirror_rs_owner=""
grep -qF "require('./rust-vault-mirror-preferences.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && mirror_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-vault-mirror-preferences features; then
  mirror_rs_owner=rust
fi
[ "$mirror_core_owner" = "$mirror_rs_owner" ] \
  || die "vault mirror preference ownership pairing: Core and Rust must move together"
if [ "$mirror_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "vault mirror preference ownership pairing: missing route contract"
  mirror_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    $1 == "/api/chat-vault-mirror/preferences" {
      if ($2 != "rust" || $3 != "-" || $4 != "-") exit 1
      print $1
    }')" || die "vault mirror preference ownership pairing: every method of the path must belong to Rust"
  [ "$mirror_routes" = /api/chat-vault-mirror/preferences ] || die "vault mirror preference ownership pairing: incomplete route ownership"
fi

#@storage_connection
# The storage connection save (#2190, round 4): Core's rust-storage-connection.cjs refusal (PUT on the
# exact path; Core's guard then refuses every Node write of storage_connections, and its legacy WebDAV
# setup asks the front's internal store) and the front's rust-storage-connection capability, with
# exactly GET and PUT of /api/integrations/storage Rust's in routes.toml (GET is the metadata worker's).
conn_core_owner=""
conn_rs_owner=""
grep -qF "require('./rust-storage-connection.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && conn_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-storage-connection features; then
  conn_rs_owner=rust
fi
[ "$conn_core_owner" = "$conn_rs_owner" ] \
  || die "storage connection ownership pairing: Core and Rust must move together"
if [ "$conn_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "storage connection ownership pairing: missing route contract"
  conn_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    $1 == "/api/integrations/storage" {
      if ($2 != "rust" || $3 != "-") exit 1
      n = split($4, m, /[^A-Z]+/); k = 0; get = 0; put = 0
      for (i = 1; i <= n; i++) if (m[i] != "") { k++; if (m[i] == "GET") get = 1; else if (m[i] == "PUT") put = 1; else exit 1 }
      if (k != 2 || !get || !put) exit 1
      print $1
    }')" || die "storage connection ownership pairing: exactly GET and PUT of the path must belong to Rust"
  [ "$conn_routes" = /api/integrations/storage ] || die "storage connection ownership pairing: incomplete route ownership"
fi

#@providers
# The provider registry (#2190, round 6): Core's rust-providers.cjs refusal and the front's
# rust-providers capability. Detected first because it widens the probe's route below: with it,
# PUT and DELETE of /api/providers/test (a provider id like any other) are Rust's too.
prov_core_owner=""
prov_rs_owner=""
grep -qF "require('./rust-providers.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && prov_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-providers features; then
  prov_rs_owner=rust
fi
[ "$prov_core_owner" = "$prov_rs_owner" ] \
  || die "provider registry ownership pairing: Core and Rust must move together"
if [ "$prov_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "provider registry ownership pairing: missing route contract"
  prov_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    function want(path) {
      if (path == "/api/providers") return "GET,POST"
      if (path == "/api/providers/{id}") return "PUT,DELETE"
      if (path == "/api/providers/chatgpt") return "DELETE,GET"
      if (path == "/api/providers/test") return "DELETE,POST,PUT"
      return ""
    }
    want($1) != "" {
      if ($2 != "rust" || $3 != "-") exit 1
      n = split($4, m, /[^A-Z]+/); k = 0
      delete seen
      for (i = 1; i <= n; i++) if (m[i] != "") { if (m[i] in seen) exit 1; seen[m[i]] = 1; k++ }
      nw = split(want($1), w, ",")
      if (k != nw) exit 1
      for (i = 1; i <= nw; i++) if (!(w[i] in seen)) exit 1
      print $1
    }' | sort)" || die "provider registry ownership pairing: exactly the registry's methods of the routes must belong to Rust"
  prov_expected="$(printf '%s\n' /api/providers /api/providers/chatgpt /api/providers/test '/api/providers/{id}' | sort)"
  [ "$prov_routes" = "$prov_expected" ] || die "provider registry ownership pairing: incomplete route ownership"
fi

# The provider keys' re-seal (#2190, round 6 review): Core's secrets rotation no longer rewrites the
# provider files, it asks the front's internal store (rust-providers-reseal.cjs in Core,
# rust-providers-reseal in the front's FEATURES, POST /v1/providers/reseal in project_store.rs and
# providersReseal in Core's store client). It moves with the registry: a registry without it would
# leave a second writer of the files (the rotation), and the op alone has no files of its own.
reseal_core_owner=""
reseal_rs_owner=""
grep -qF "require('./rust-providers-reseal.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && reseal_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-providers-reseal features; then
  reseal_rs_owner=rust
fi
[ "$reseal_core_owner" = "$reseal_rs_owner" ] \
  || die "provider key reseal ownership pairing: Core and Rust must move together"
[ "$reseal_rs_owner" = "$prov_rs_owner" ] \
  || die "provider key reseal ownership pairing: it moves with the provider registry (a registry without it leaves a second writer of the provider files)"
if [ "$reseal_rs_owner" = rust ]; then
  grep -qF '"/v1/providers/reseal"' "$tree/rs/bins/noevia-server/src/project_store.rs" 2>/dev/null \
    || die "provider key reseal ownership pairing: the front does not serve /v1/providers/reseal"
  grep -qF 'providersReseal' "$tree/core/server/project-store-client.cjs" 2>/dev/null \
    || die "provider key reseal ownership pairing: Core's store client does not call /v1/providers/reseal"
fi

# The provider probe (#2190, round 4): Core's rust-provider-test.cjs refusal (POST on the exact path;
# Node keeps the other methods, its /api/providers/{id} handler reads `test` as an id) and the front's
# rust-provider-test capability, with exactly POST of /api/providers/test Rust's in routes.toml.
# With the provider registry (round 6) the route is also Rust's for PUT and DELETE, and then those
# three methods exactly.
ptest_core_owner=""
ptest_rs_owner=""
grep -qF "require('./rust-provider-test.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && ptest_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-provider-test features; then
  ptest_rs_owner=rust
fi
[ "$ptest_core_owner" = "$ptest_rs_owner" ] \
  || die "provider test ownership pairing: Core and Rust must move together"
if [ "$ptest_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "provider test ownership pairing: missing route contract"
  ptest_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' -v registry="$prov_rs_owner" '
    $1 == "/api/providers/test" {
      if ($2 != "rust" || $3 != "-") exit 1
      n = split($4, m, /[^A-Z]+/); k = 0
      for (i = 1; i <= n; i++) if (m[i] != "") {
        if (m[i] != "POST" && !(registry == "rust" && (m[i] == "PUT" || m[i] == "DELETE"))) exit 1
        k++
      }
      if (k != (registry == "rust" ? 3 : 1)) exit 1
      print $1
    }')" || die "provider test ownership pairing: exactly POST of the path must belong to Rust"
  [ "$ptest_routes" = /api/providers/test ] || die "provider test ownership pairing: incomplete route ownership"
fi

#@chatgpt_signin
# Sign in with ChatGPT's token broker (#2190, round 5): Core's rust-chatgpt-oauth.cjs refusal and
# the front's rust-chatgpt-oauth capability, with exactly POST of the three device routes and GET of
# the model list Rust's in routes.toml (Node keeps DELETE /api/providers/chatgpt, the disconnect).
cgpt_core_owner=""
cgpt_rs_owner=""
grep -qF "require('./rust-chatgpt-oauth.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && cgpt_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-chatgpt-oauth features; then
  cgpt_rs_owner=rust
fi
[ "$cgpt_core_owner" = "$cgpt_rs_owner" ] \
  || die "ChatGPT sign-in ownership pairing: Core and Rust must move together"
if [ "$cgpt_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "ChatGPT sign-in ownership pairing: missing route contract"
  cgpt_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    function want(path) {
      if (path == "/api/providers/chatgpt/device" || path == "/api/providers/chatgpt/device/poll" || path == "/api/providers/chatgpt/device/cancel") return "POST"
      if (path == "/api/providers/chatgpt/models") return "GET"
      return ""
    }
    want($1) != "" {
      if ($2 != "rust" || $3 != "-") exit 1
      n = split($4, m, /[^A-Z]+/); k = 0
      for (i = 1; i <= n; i++) if (m[i] != "") { if (m[i] != want($1)) exit 1; k++ }
      if (k != 1) exit 1
      print $1
    }')" || die "ChatGPT sign-in ownership pairing: exactly the broker's methods of the routes must belong to Rust"
  cgpt_routes="$(printf '%s\n' "$cgpt_routes" | sort | tr '\n' ' ')"
  [ "$cgpt_routes" = "/api/providers/chatgpt/device /api/providers/chatgpt/device/cancel /api/providers/chatgpt/device/poll /api/providers/chatgpt/models " ] \
    || die "ChatGPT sign-in ownership pairing: incomplete route ownership"
fi

#@mcp_credentials
# The MCP server credentials (#2202): Core's rust-mcp-credentials.cjs refusal and the front's
# rust-mcp-credentials capability, with exactly the methods the broker answers Rust's in routes.toml
# (the keys, the sign-in servers, the connect, the disconnect and the sign-in callback). The front is
# the only writer of the three credential tables, and Core's chat asks its internal store
# (/v1/mcp/*; mcpSession, mcpKey and the rotation's mcpReseal in Core's store client) for one call's
# access token or key: a pair without either side would leave a second writer or a chat with no key.
mcp_core_owner=""
mcp_rs_owner=""
code_has "$tree/core/server/index.cjs" "require\\('\\./rust-mcp-credentials\\.cjs'\\)" && mcp_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-mcp-credentials features; then
  mcp_rs_owner=rust
fi
[ "$mcp_core_owner" = "$mcp_rs_owner" ] \
  || die "MCP credential ownership pairing: Core and Rust must move together"
if [ "$mcp_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "MCP credential ownership pairing: missing route contract"
  mcp_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    function want(path) {
      if (path == "/api/mcp-keys/servers" || path == "/api/mcp-oauth/servers" || path == "/api/mcp-oauth/callback") return "GET"
      if (path == "/api/mcp-keys/{*}") return "DELETE,PUT"
      if (path == "/api/mcp-oauth/{id}") return "DELETE"
      if (path == "/api/mcp-oauth/{id}/connect") return "POST"
      return ""
    }
    want($1) != "" {
      if ($2 != "rust" || $3 != "-") exit 1
      n = split($4, m, /[^A-Z]+/); k = 0
      delete seen
      for (i = 1; i <= n; i++) if (m[i] != "") { if (m[i] in seen) exit 1; seen[m[i]] = 1; k++ }
      nw = split(want($1), w, ",")
      if (k != nw) exit 1
      for (i = 1; i <= nw; i++) if (!(w[i] in seen)) exit 1
      print $1
    }' | LC_ALL=C sort | tr '\n' ' ')" || die "MCP credential ownership pairing: exactly the broker's methods of the routes must belong to Rust"
  [ "$mcp_routes" = "/api/mcp-keys/servers /api/mcp-keys/{*} /api/mcp-oauth/callback /api/mcp-oauth/servers /api/mcp-oauth/{id} /api/mcp-oauth/{id}/connect " ] \
    || die "MCP credential ownership pairing: incomplete route ownership"
  # Code, not comments: a mention of the path or of an op in a comment serves and calls nothing.
  code_has "$tree/rs/bins/noevia-server/src/project_store.rs" '/v1/mcp/' \
    || die "MCP credential ownership pairing: the front does not serve /v1/mcp/*"
  for op in mcpSession mcpKey mcpReseal; do
    code_has "$tree/core/server/project-store-client.cjs" "(^|[^A-Za-z0-9_])$op[[:space:]]*\\(" \
      || die "MCP credential ownership pairing: Core's store client does not call $op"
  done
fi

#@mcp_views
# The admin MCP directory, the permitted-tools view and the connectors list (#2202 round 2): each is a Core refusal
# (rust-mcp-directory.cjs, rust-toolboxes-permitted.cjs, rust-connectors.cjs) paired with a front capability of the
# same name, with exactly the methods the front answers Rust's in routes.toml. Together they make the front the
# only writer of directory_mcp_servers (Node re-reads it: its directory-mcp.cjs must hold no write of it) and move
# the last two views onto the discovery snapshot Node publishes (mcp-discovery.json), or, since row T, onto the front's
# own discovery and Node's toolbox-chat-facts.json: a pair without the Core half that publishes it, or the front half
# that reads it, would answer from nothing.
discovery_pair_ok() {
  code_has "$tree/rs/bins/noevia-server/src/lib.rs" "pub[[:space:]]+mod[[:space:]]+mcp_discovery" || return 1
  if [ "$md_rs_owner" = rust ]; then
    # Row T: the front discovers itself and reads the chat process's toolbox facts.
    code_has "$tree/core/server/index.cjs" "createChatFactsPublisher[[:space:]]*\\(" \
      && [ -f "$tree/core/server/toolbox-chat-facts.cjs" ]
  else
    code_has "$tree/core/server/index.cjs" "createMcpDiscoveryPublisher[[:space:]]*\\(" \
      && [ -f "$tree/core/server/mcp-discovery-snapshot.cjs" ]
  fi
}
mcpd_core_owner=""
mcpd_rs_owner=""
code_has "$tree/core/server/index.cjs" "require\\('\\./rust-mcp-directory\\.cjs'\\)" && mcpd_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-mcp-directory features; then
  mcpd_rs_owner=rust
fi
[ "$mcpd_core_owner" = "$mcpd_rs_owner" ] \
  || die "MCP directory ownership pairing: Core and Rust must move together"
if [ "$mcpd_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "MCP directory ownership pairing: missing route contract"
  mcpd_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    $1 ~ /^\/api\/admin\/mcp-directory(\/|$)/ {
      if ($2 != "rust" || $3 != "-" || $4 != "-") exit 1
      print $1
    }')" || die "MCP directory ownership pairing: every method of the paths must belong to Rust"
  mcpd_routes="$(printf '%s\n' "$mcpd_routes" | LC_ALL=C sort | tr '\n' ' ')"
  [ "$mcpd_routes" = "/api/admin/mcp-directory /api/admin/mcp-directory/custom /api/admin/mcp-directory/custom/preview /api/admin/mcp-directory/{*} /api/admin/mcp-directory/{*}/keys /api/admin/mcp-directory/{id}/oauth-client " ] \
    || die "MCP directory ownership pairing: incomplete route ownership"
  discovery_pair_ok || die "MCP directory ownership pairing: Core does not publish the discovery snapshot the front reads"
  # Node re-reads the table and never writes it: a Core that still writes it beside the front would be a second writer.
  ! code_has "$tree/core/server/directory-mcp.cjs" "(INSERT[[:space:]]+(OR[[:space:]]+[A-Za-z]+[[:space:]]+)?INTO|REPLACE[[:space:]]+INTO|UPDATE|DELETE[[:space:]]+FROM)[[:space:]]+.?directory_mcp_servers" \
    || die "MCP directory ownership pairing: Core still writes directory_mcp_servers"
  # The table is on Core's list of the front's, and the front re-seals the shared key for a rotation.
  code_has "$tree/core/server/rust-mcp-credentials.cjs" "directory_mcp_servers" \
    || die "MCP directory ownership pairing: Core does not treat directory_mcp_servers as the front's"
  code_has "$tree/rs/bins/noevia-server/src/mcp_broker.rs" "directory_mcp_servers" \
    || die "MCP directory ownership pairing: the front does not re-seal the shared keys"
fi

ptb_core_owner=""
ptb_rs_owner=""
code_has "$tree/core/server/index.cjs" "require\\('\\./rust-toolboxes-permitted\\.cjs'\\)" && ptb_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-toolboxes-permitted features; then
  ptb_rs_owner=rust
fi
[ "$ptb_core_owner" = "$ptb_rs_owner" ] \
  || die "permitted tools ownership pairing: Core and Rust must move together"
if [ "$ptb_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "permitted tools ownership pairing: missing route contract"
  ptb_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    $1 == "/api/toolboxes/permitted" {
      if ($2 != "rust" || $3 != "-" || $4 != "-") exit 1
      print $1
    }')" || die "permitted tools ownership pairing: every method of the path must belong to Rust"
  [ "$(printf '%s' "$ptb_routes" | tr '\n' ' ')" = "/api/toolboxes/permitted" ] \
    || die "permitted tools ownership pairing: incomplete route ownership"
  discovery_pair_ok || die "permitted tools ownership pairing: Core does not publish the discovery snapshot the front reads"
fi

cnx_core_owner=""
cnx_rs_owner=""
code_has "$tree/core/server/index.cjs" "require\\('\\./rust-connectors\\.cjs'\\)" && cnx_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-connectors features; then
  cnx_rs_owner=rust
fi
[ "$cnx_core_owner" = "$cnx_rs_owner" ] \
  || die "connectors ownership pairing: Core and Rust must move together"
if [ "$cnx_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "connectors ownership pairing: missing route contract"
  cnx_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    function want(path) {
      if (path == "/api/connectors") return "GET"
      if (path == "/api/connectors/nextcloud/policy") return "PUT"
      return ""
    }
    want($1) != "" {
      if ($2 != "rust" || $3 != "-") exit 1
      n = split($4, m, /[^A-Z]+/); k = 0
      for (i = 1; i <= n; i++) if (m[i] != "") { if (m[i] != want($1)) exit 1; k++ }
      if (k != 1) exit 1
      print $1
    }')" || die "connectors ownership pairing: exactly GET /api/connectors and PUT /api/connectors/nextcloud/policy must belong to Rust"
  cnx_routes="$(printf '%s\n' "$cnx_routes" | LC_ALL=C sort | tr '\n' ' ')"
  [ "$cnx_routes" = "/api/connectors /api/connectors/nextcloud/policy " ] \
    || die "connectors ownership pairing: incomplete route ownership"
  discovery_pair_ok || die "connectors ownership pairing: Core does not publish the discovery snapshot the front reads"
fi

#@provider_models
# The provider model lists (#2190, round 5): Core's rust-provider-models.cjs refusal (every method on
# the exact path) and the front's rust-provider-models capability, with the route Rust's for every
# method in routes.toml.
pmod_core_owner=""
pmod_rs_owner=""
grep -qF "require('./rust-provider-models.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && pmod_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-provider-models features; then
  pmod_rs_owner=rust
fi
[ "$pmod_core_owner" = "$pmod_rs_owner" ] \
  || die "provider model list ownership pairing: Core and Rust must move together"
if [ "$pmod_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "provider model list ownership pairing: missing route contract"
  pmod_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    $1 == "/api/providers/{providerId}/models{*?}" {
      if ($2 != "rust" || $3 != "-" || $4 != "-") exit 1
      print $1
    }')" || die "provider model list ownership pairing: the route must belong to Rust for every method"
  [ "$pmod_routes" = "/api/providers/{providerId}/models{*?}" ] || die "provider model list ownership pairing: incomplete route ownership"
fi

#@google_drive
# Google Drive's token authority (#2190, round 7): Core's rust-google-drive.cjs refusal and the front's
# rust-google-drive capability, with exactly POST of the connect and disconnect routes and PUT of the policy
# and backup-copy routes Rust's in routes.toml (GET /api/connectors and the Nextcloud policy stay Node's).
gdrv_core_owner=""
gdrv_rs_owner=""
grep -qF "require('./rust-google-drive.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && gdrv_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-google-drive features; then
  gdrv_rs_owner=rust
fi
[ "$gdrv_core_owner" = "$gdrv_rs_owner" ] \
  || die "Google Drive ownership pairing: Core and Rust must move together"
if [ "$gdrv_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "Google Drive ownership pairing: missing route contract"
  gdrv_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    function want(path) {
      if (path == "/api/connectors/gdrive/connect" || path == "/api/connectors/gdrive/disconnect") return "POST"
      if (path == "/api/connectors/gdrive/policy" || path == "/api/connectors/gdrive/backup-copy") return "PUT"
      return ""
    }
    want($1) != "" {
      if ($2 != "rust" || $3 != "-") exit 1
      n = split($4, m, /[^A-Z]+/); k = 0
      for (i = 1; i <= n; i++) if (m[i] != "") { if (m[i] != want($1)) exit 1; k++ }
      if (k != 1) exit 1
      print $1
    }')" || die "Google Drive ownership pairing: exactly the connector writes' methods of the routes must belong to Rust"
  gdrv_routes="$(printf '%s\n' "$gdrv_routes" | sort | tr '\n' ' ')"
  [ "$gdrv_routes" = "/api/connectors/gdrive/backup-copy /api/connectors/gdrive/connect /api/connectors/gdrive/disconnect /api/connectors/gdrive/policy " ] \
    || die "Google Drive ownership pairing: incomplete route ownership"
fi

#@offsite_backup
# The off-site backup (#2190, round 7): Core's rust-offsite-backup.cjs refusal and the front's
# rust-offsite-backup capability, with all eight paths (the seven of the first cut and the review's
# new-store action) Rust's for every method in routes.toml.
offs_core_owner=""
offs_rs_owner=""
grep -qF "require('./rust-offsite-backup.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && offs_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-offsite-backup features; then
  offs_rs_owner=rust
fi
[ "$offs_core_owner" = "$offs_rs_owner" ] \
  || die "off-site backup ownership pairing: Core and Rust must move together"
if [ "$offs_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "off-site backup ownership pairing: missing route contract"
  offs_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    $1 ~ /^\/api\/admin\/offsite-backup(\/|$)/ {
      if ($2 != "rust" || $3 != "-" || $4 != "-") exit 1
      print $1
    }')" || die "off-site backup ownership pairing: every method of the paths must belong to Rust"
  offs_routes="$(printf '%s\n' "$offs_routes" | sort | tr '\n' ' ')"
  [ "$offs_routes" = "/api/admin/offsite-backup /api/admin/offsite-backup/copy /api/admin/offsite-backup/google/connect /api/admin/offsite-backup/google/disconnect /api/admin/offsite-backup/new-store /api/admin/offsite-backup/recovery-key /api/admin/offsite-backup/run /api/admin/offsite-backup/verify " ] \
    || die "off-site backup ownership pairing: incomplete route ownership"
  # Core refuses exactly the paths the front answers: the new-store action included, or a stale Core
  # would route an administrator's "this is a new store" to a Node that has no such path.
  grep -qF "'/api/admin/offsite-backup/new-store'" "$tree/core/server/rust-offsite-backup.cjs" 2>/dev/null \
    || die "off-site backup ownership pairing: Core does not refuse the new-store path"
fi

#@model_sizing
# The two read-only model sizing reads (sbstndalton/noevia#2189) move as one owner: Core's
# rust-model-sizing.cjs refusal of their GET and the front's rust-model-sizing capability, with
# exactly GET of both routes Rust's in routes.toml (Node keeps every other method). Historic old/old
# pins remain valid rollback pairs.
sizing_core_owner=""
sizing_rs_owner=""
grep -qF "require('./rust-model-sizing.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && sizing_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-model-sizing features; then
  sizing_rs_owner=rust
fi
[ "$sizing_core_owner" = "$sizing_rs_owner" ] \
  || die "model sizing ownership pairing: Core and Rust must move together"
if [ "$sizing_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "model sizing ownership pairing: missing route contract"
  sizing_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    $1 == "/api/models/estimate" || $1 == "/api/models/preset/suggest" {
      if ($2 != "rust" || $3 != "-") exit 1
      n = split($4, m, /[^A-Z]+/); k = 0
      for (i = 1; i <= n; i++) if (m[i] != "") { if (m[i] != "GET") exit 1; k++ }
      if (k != 1) exit 1
      print $1
    }' | sort)" || die "model sizing ownership pairing: exactly GET of both routes must belong to Rust"
  sizing_expected="$(printf '%s\n' /api/models/estimate /api/models/preset/suggest | sort)"
  [ "$sizing_routes" = "$sizing_expected" ] || die "model sizing ownership pairing: incomplete route ownership"
fi

#@model_state_reads
# GET inference-budget and GET installed (sbstndalton/noevia#2262) move as one owner: Core's rust-model-state-reads.cjs
# refusal and the front's rust-model-state-reads capability, with exactly GET of both Rust's in routes.toml (the budget's
# PUT stays Node's). Core must have dropped the handlers and hand the front its facts on the loopback check, with the
# secret captured as for research. Old/old remains a valid rollback pair; either mixed generation is refused.
ms_core_owner=""
ms_rs_owner=""
grep -qF "require('./rust-model-state-reads.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && ms_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-model-state-reads features; then
  ms_rs_owner=rust
fi
[ "$ms_core_owner" = "$ms_rs_owner" ] \
  || die "model state read ownership pairing: Core and Rust must move together"
if [ -f "$tree/rs/contracts/http/routes.toml" ]; then
  # An unreadable table is the route-table check's to report, below.
  ms_table="$(routes_toml "$tree/rs/contracts/http/routes.toml" 2>/dev/null)" || ms_table=""
  ms_routes="$(printf '%s\n' "$ms_table" | awk -F'\t' -v want="$ms_rs_owner" '
    $1 == "/api/models/inference-budget" || $1 == "/api/models/installed" {
      if (want == "rust") {
        if ($2 != "rust" || $3 != "-") exit 1
        n = split($4, m, /[^A-Z]+/); k = 0
        for (i = 1; i <= n; i++) if (m[i] != "") { if (m[i] != "GET") exit 1; k++ }
        if (k != 1) exit 1
      }
      if (want != "rust" && $2 != "node") exit 1
      print $1
    }')" || die "model state read ownership pairing: exactly GET of both routes must be Rust's with the new pair, and Node's with the old"
  if [ "$ms_rs_owner" = rust ]; then
    [ "$(printf '%s\n' "$ms_routes" | sort -u | wc -l | tr -d ' ')" = 2 ] || die "model state read ownership pairing: incomplete route ownership"
    grep -qF "OWNED_PATHS = Object.freeze(['/api/models/inference-budget', '/api/models/installed'])" "$tree/core/server/rust-model-state-reads.cjs" 2>/dev/null \
      || die "model state read ownership pairing: Core must refuse exactly the two reads the front answers"
    ! grep -qE 'inferenceEstimates|await modelsInstalled\(\)\)' "$tree/core/server/routes/models.cjs" 2>/dev/null \
      || die "model state read ownership pairing: Core still carries the read handlers"
    [ -f "$tree/core/server/rust-model-facts.cjs" ] \
      || die "model state read ownership pairing: Core does not offer the front its model facts"
    ms_index="$(code_lines "$tree/core/server/index.cjs")"
    grep -qE 'modelFactsBridge\(req, res\)' <<<"$ms_index" \
      || die "model state read ownership pairing: Core does not mount the model facts bridge"
    ms_secret="$(sed -nE "s/^[[:space:]]*const ([A-Za-z_][A-Za-z0-9_]*) = String\(process\.env\.NOEVIA_PROJECT_STORE_TOKEN \|\| ''\);.*/\1/p" <<<"$ms_index" | head -1)"
    [ -n "$ms_secret" ] && awk -v c="$ms_secret" '
      /createModelFactsBridge\(\{/ { bridge = NR }
      index($0, "secret: " c) && bridge && NR - bridge <= 3 { ok = 1 }
      END { exit !ok }
    ' <<<"$ms_index" \
      || die "model state read ownership pairing: Core does not hand its model facts bridge the front's captured secret"
  fi
fi

#@model_evidence
# The model maintenance routes (sbstndalton/noevia#2210) move as one owner: Core's rust-model-maintenance.cjs
# refusal and the front's rust-model-maintenance capability, with exactly POST /api/models/presets/reload,
# GET /api/models/evidence and POST /api/models/evidence/import Rust's in routes.toml (Node keeps every other
# method; recheck moves separately in #2286). The front is the single writer of the reload guard state file and the evidence
# store; a mixed pair would have two writers or none. Historic old/old pins remain valid rollback pairs.
maint_core_owner=""
maint_rs_owner=""
grep -qF "require('./rust-model-maintenance.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && maint_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-model-maintenance features; then
  maint_rs_owner=rust
fi
[ "$maint_core_owner" = "$maint_rs_owner" ] \
  || die "model maintenance ownership pairing: Core and Rust must move together"
if [ "$maint_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "model maintenance ownership pairing: missing route contract"
  maint_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    $1 == "/api/models/presets/reload" || $1 == "/api/models/evidence" || $1 == "/api/models/evidence/import" {
      want = ($1 == "/api/models/evidence") ? "GET" : "POST"
      if ($2 != "rust" || $3 != "-") exit 1
      n = split($4, m, /[^A-Z]+/); k = 0
      for (i = 1; i <= n; i++) if (m[i] != "") { if (m[i] != want) exit 1; k++ }
      if (k != 1) exit 1
      print $1
    }' | sort)" || die "model maintenance ownership pairing: exactly the one method of each route must belong to Rust"
  maint_expected="$(printf '%s\n' /api/models/presets/reload /api/models/evidence /api/models/evidence/import | sort)"
  [ "$maint_routes" = "$maint_expected" ] || die "model maintenance ownership pairing: incomplete route ownership"
  # Recheck needs its own native probe/admission pairing, beyond this earlier maintenance group.
  if ! move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-model-evidence-recheck features && routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '$1 == "/api/models/evidence/recheck" && $2 == "rust" { f = 1 } END { exit !f }'; then
    die "model maintenance ownership pairing: the recheck stays Node's"
  fi
fi

# #2286: the 1x1 recheck probe and evidence filing are native; Node keeps live admission only.
er_core_owner=""
er_rs_owner=""
grep -qF "require('./rust-model-evidence-recheck.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && er_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-model-evidence-recheck features; then er_rs_owner=rust; fi
[ "$er_core_owner" = "$er_rs_owner" ] || die "evidence recheck ownership pairing: Core and Rust must move together"
if [ -f "$tree/rs/contracts/http/routes.toml" ]; then
  # The common contract parser below reports malformed TOML; preserve its old-pair diagnostic.
  er_table="$(routes_toml "$tree/rs/contracts/http/routes.toml" 2>/dev/null)" || er_table=""
  er_rows="$(printf '%s\n' "$er_table" | awk -F'\t' -v want="$er_rs_owner" '
    $1 == "/api/models/evidence/recheck" {
      if(want=="rust") {
        if($2!="rust" || $3!="-") exit 1
        n=split($4,m,/[^A-Z]+/); k=0
        for(i=1;i<=n;i++) if(m[i]!="") {if(m[i]!="POST") exit 1;k++}
        if(k!=1) exit 1
      } else if($2!="node") exit 1
      print $1
    }')" || die "evidence recheck ownership pairing: POST alone must be native with the new pair and Node's with the old"
  if [ "$er_rs_owner" = rust ]; then
    [ "$maint_rs_owner" = rust ] || die "evidence recheck ownership pairing: native evidence store required"
    [ "$(printf '%s\n' "$er_rows" | wc -w | tr -d ' ')" = 1 ] || die "evidence recheck ownership pairing: missing route"
    er_refused="$( { project_routes_core "$tree/core/server/rust-model-evidence-recheck.cjs" || true; } | sort | tr '\n' ' ')"
    [ "$er_refused" = 'POST /api/models/evidence/recheck ' ] || die "evidence recheck ownership pairing: exact POST refusal required"
    ! grep -qE 'createVisionProbe|source:[[:space:]]*.recheck.' "$tree/core/server/routes/models.cjs" 2>/dev/null || die "evidence recheck ownership pairing: public Node probe still present"
    [ -f "$tree/core/server/inference-admission-lease.cjs" ] || die "evidence recheck ownership pairing: live inference admission required"
    er_index="$(code_lines "$tree/core/server/index.cjs")"
    grep -qE 'inferenceAdmission\.handle\(req, res\)' <<<"$er_index" || die "evidence recheck ownership pairing: inference admission must be mounted"
    er_secret="$(sed -nE "s/^[[:space:]]*const ([A-Za-z_][A-Za-z0-9_]*) = String\(process\.env\.NOEVIA_PROJECT_STORE_TOKEN \|\| ''\);.*/\1/p" <<<"$er_index" | head -1)"
    [ -n "$er_secret" ] && awk -v c="$er_secret" '
      /createAdmissionRoutes\(\{/ {bridge=NR}
      index($0,"secret: " c) && bridge && NR-bridge<=3 {ok=1}
      END {exit !ok}
    ' <<<"$er_index" || die "evidence recheck ownership pairing: captured inference admission secret required"
  fi
fi

