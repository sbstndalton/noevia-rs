# Verbatim top-level pairing guard blocks of noevia deploy/tools/assemble-release.sh at noevia origin/main f23eb731.
# Run by tests/guard_pairing.rs (never sourced whole): each '#@<name>' section runs alone with $tree set.
#@feature
feature_core_owner=""
feature_rs_owner=""
grep -qF '.createReadOnlyFeatures(' "$tree/core/server/index.cjs" 2>/dev/null && feature_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-feature-owner features; then
  feature_rs_owner=rust
fi
[ "$feature_core_owner" = "$feature_rs_owner" ] \
  || die "feature ownership pairing: Core and Rust must move together"
if [ "$feature_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "feature ownership pairing: missing route contract"
  feature_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    $1 == "/api/features" || $1 == "/api/admin/features" || $1 == "/api/admin/features/{name}" {
      if ($2 != "rust" || $3 != "-" || $4 != "-") exit 1
      print $1
    }' | sort)" || die "feature ownership pairing: all methods must belong to Rust"
  feature_expected="$(printf '%s\n' /api/features /api/admin/features '/api/admin/features/{name}' | sort)"
  [ "$feature_routes" = "$feature_expected" ] || die "feature ownership pairing: incomplete namespace ownership"
fi
#@model_reads
models_core_owner=""
models_rs_owner=""
grep -qF "require('./rust-models.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && models_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-model-reads features; then
  models_rs_owner=rust
fi
[ "$models_core_owner" = "$models_rs_owner" ] \
  || die "model read ownership pairing: Core and Rust must move together"
if [ "$models_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "model read ownership pairing: missing route contract"
  model_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    $1 == "/api/models/capabilities" || $1 == "/api/models/hardware" {
      if ($2 != "rust" || $3 != "-") exit 1
      n = split($4, m, /[^A-Z]+/); k = 0
      for (i = 1; i <= n; i++) if (m[i] != "") { if (m[i] != "GET") exit 1; k++ }
      if (k != 1) exit 1
      print $1
    }' | sort)" || die "model read ownership pairing: exactly GET of both routes must belong to Rust"
  model_expected="$(printf '%s\n' /api/models/capabilities /api/models/hardware | sort)"
  [ "$model_routes" = "$model_expected" ] || die "model read ownership pairing: incomplete route ownership"
fi
#@project_reads
project_reads_core_owner=""
project_reads_rs_owner=""
grep -qF "require('./rust-project-reads.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && project_reads_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-project-reads features; then
  project_reads_rs_owner=rust
fi
[ "$project_reads_core_owner" = "$project_reads_rs_owner" ] \
  || die "project source read ownership pairing: Core and Rust must move together"
if [ "$project_reads_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "project source read ownership pairing: missing route contract"
  project_read_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    $1 == "/api/projects/{id}/sources/text" || $1 == "/api/projects/{id}/documents/original" || $1 == "/api/projects/{id}/{*}/original" {
      if ($2 != "rust" || $3 != "-") exit 1
      n = split($4, m, /[^A-Z]+/); k = 0
      for (i = 1; i <= n; i++) if (m[i] != "") { if (m[i] != "GET") exit 1; k++ }
      if (k != 1) exit 1
      print $1
    }' | sort)" || die "project source read ownership pairing: exactly GET of the three routes must belong to Rust"
  project_read_expected="$(printf '%s\n' '/api/projects/{id}/sources/text' '/api/projects/{id}/documents/original' '/api/projects/{id}/{*}/original' | sort)"
  [ "$project_read_routes" = "$project_read_expected" ] || die "project source read ownership pairing: incomplete route ownership"
fi
#@project_lifecycle
project_lifecycle_core_owner=""
project_lifecycle_rs_owner=""
grep -qF "require('./rust-project-lifecycle.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && project_lifecycle_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-project-lifecycle features; then
  project_lifecycle_rs_owner=rust
fi
[ "$project_lifecycle_core_owner" = "$project_lifecycle_rs_owner" ] \
  || die "project create/delete ownership pairing: Core and Rust must move together"
if [ "$project_lifecycle_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "project create/delete ownership pairing: missing route contract"
  project_lifecycle_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    $1 == "/api/projects" || $1 == "/api/projects/{projectId}" {
      if ($2 != "rust" || $3 != "-") exit 1
      want = ($1 == "/api/projects") ? "POST" : "DELETE"
      n = split($4, m, /[^A-Z]+/); k = 0
      for (i = 1; i <= n; i++) if (m[i] != "") { if (m[i] != want) exit 1; k++ }
      if (k != 1) exit 1
      print $1
    }' | sort)" || die "project create/delete ownership pairing: exactly POST /api/projects and DELETE /api/projects/{projectId} must belong to Rust"
  project_lifecycle_expected="$(printf '%s\n' '/api/projects' '/api/projects/{projectId}' | sort)"
  [ "$project_lifecycle_routes" = "$project_lifecycle_expected" ] || die "project create/delete ownership pairing: incomplete route ownership"
fi
#@project_files
project_files_core_owner=""
project_files_rs_owner=""
grep -qF "require('./rust-project-files.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && project_files_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-project-files features; then
  project_files_rs_owner=rust
fi
[ "$project_files_core_owner" = "$project_files_rs_owner" ] \
  || die "project file delete ownership pairing: Core and Rust must move together"
if [ "$project_files_rs_owner" = rust ]; then
  [ "$project_storage_rs_owner" = rust ] || die "project file delete ownership pairing: needs the project storage pair"
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "project file delete ownership pairing: missing route contract"
  project_files_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    $1 == "/api/projects/{id}/files" {
      if ($2 != "rust" || $3 != "-") exit 1
      n = split($4, m, /[^A-Z]+/); k = 0
      for (i = 1; i <= n; i++) if (m[i] != "") { if (m[i] != "DELETE") exit 1; k++ }
      if (k != 1) exit 1
      print $1
    }')" || die "project file delete ownership pairing: exactly DELETE /api/projects/{id}/files must belong to Rust"
  [ "$project_files_routes" = "/api/projects/{id}/files" ] || die "project file delete ownership pairing: incomplete route ownership"
fi
#@project_config
pc_core_owner=""
pc_rs_owner=""
grep -qF "require('./rust-project-config.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && pc_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-project-config features; then
  pc_rs_owner=rust
fi
[ "$pc_core_owner" = "$pc_rs_owner" ] \
  || die "project configuration ownership pairing: Core and Rust must move together"
if [ "$pc_rs_owner" = rust ]; then
  [ -f "$tree/rs/contracts/http/routes.toml" ] || die "project configuration ownership pairing: missing route contract"
  pc_routes="$(routes_toml "$tree/rs/contracts/http/routes.toml" | awk -F'\t' '
    function want(path) {
      if (path == "/api/projects/{projectId}/config") return "POST"
      if (path == "/api/projects/{projectId}/instruction-skills") return "GET,PUT"
      if (path == "/api/projects/{projectId}/instruction-skills/manifests") return "GET"
      if (path == "/api/projects/{projectId}/instruction-skills/manifests/{skillId}/content") return "GET"
      if (path == "/api/projects/{project}/skills/install") return "POST"
      return ""
    }
    want($1) != "" {
      if ($2 != "rust" || $3 != "-" || $4 == "-") exit 1
      w = want($1)
      ne = split(w, e, ","); delete ok; for (j = 1; j <= ne; j++) ok[e[j]] = 1
      n = split($4, m, /[^A-Z]+/); k = 0; delete seen
      for (i = 1; i <= n; i++) if (m[i] != "") { if (!(m[i] in ok) || (m[i] in seen)) exit 1; seen[m[i]] = 1; k++ }
      if (k != ne) exit 1
      print $1
    }')" || die "project configuration ownership pairing: exactly the methods Node answered must belong to Rust"
  pc_routes="$(printf '%s\n' "$pc_routes" | sort | tr '\n' ' ')"
  pc_expected="$(printf '%s\n' /api/projects/{project}/skills/install /api/projects/{projectId}/config /api/projects/{projectId}/instruction-skills /api/projects/{projectId}/instruction-skills/manifests '/api/projects/{projectId}/instruction-skills/manifests/{skillId}/content' | sort | tr '\n' ' ')"
  [ "$pc_routes" = "$pc_expected" ] || die "project configuration ownership pairing: incomplete route ownership"
  pc_refused="$( { project_routes_core "$tree/core/server/rust-project-config.cjs" || true; } | sort | tr '\n' ' ')"
  pc_refused_expected="$(printf '%s\n' 'GET /api/projects/{}/instruction-skills' 'GET /api/projects/{}/instruction-skills/manifests' 'GET /api/projects/{}/instruction-skills/manifests/{}/content' 'POST /api/projects/{}/config' 'POST /api/projects/{}/skills/install' 'PUT /api/projects/{}/instruction-skills' | sort | tr '\n' ' ')"
  [ "$pc_refused" = "$pc_refused_expected" ] \
    || die "project configuration ownership pairing: Core must refuse exactly the six routes the front answers"
  { [ ! -e "$tree/core/server/routes/projects.cjs" ] && ! grep -qF "routes/projects.cjs" "$tree/core/server/index.cjs" 2>/dev/null; } \
    || die "project configuration ownership pairing: Core still mounts the project routes"
  grep -qF "known: allToolboxes()" "$tree/core/server/index.cjs" 2>/dev/null \
    || die "project configuration ownership pairing: Core does not publish the toolbox ids the front validates skills against"
fi
#@code_egress
ce_core_owner=""; ce_rs_owner=""
code_has "$tree/core/server/code-egress-client.cjs" "^const CODE_EGRESS_CLIENT_MARKER = 'noevia-code-egress-client/1';" && ce_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-code-egress-issuer features; then ce_rs_owner=rust; fi
[ "$ce_core_owner" = "$ce_rs_owner" ] || die "code egress issuer pairing: Core ('noevia-code-egress-client/1') and Rust ('rust-code-egress-issuer') must move together"
if [ "$ce_core_owner" = rust ]; then
  [ ! -e "$tree/core/server/code-egress-control.cjs" ] || die "code egress issuer pairing: Core still carries the Node issuer (server/code-egress-control.cjs)"
  ! code_has "$tree/core/server/code-egress.cjs" 'publishWebKey|createKeyKeeper|createControlClient|mintGrant|secrets[.]derive' \
    || die "code egress issuer pairing: Core still publishes keys or signs grants"
  code_has "$tree/core/server/code-egress.cjs" "require\\('[.]/code-egress-client[.]cjs'\\)" \
    || die "code egress issuer pairing: Core's egress setup does not use the native client"
fi
#@calibration_cancel
cc_core_owner=""; cc_rs_owner=""
grep -qF "require('./rust-calibration-cancel.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && cc_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-calibration-cancel features; then cc_rs_owner=rust; fi
[ "$cc_core_owner" = "$cc_rs_owner" ] || die "calibration cancellation ownership pairing: Core and Rust must move together"
if [ -f "$tree/rs/contracts/http/routes.toml" ]; then
  cc_table="$(routes_toml "$tree/rs/contracts/http/routes.toml" 2>/dev/null)" || cc_table=""
  cc_rows="$(printf '%s\n' "$cc_table" | awk -F'\t' -v want="$cc_rs_owner" '
    $1 == "/api/models/calibration/cancel" {
      if (want == "rust" && ($2 != "rust" || $3 != "-" || $4 != "-")) exit 1
      if (want != "rust" && $2 != "node") exit 1
      print $1
    }')" || die "calibration cancellation ownership pairing: all methods must belong to the paired owner"
fi
if [ "$cc_rs_owner" = rust ]; then
  [ "${cc_rows:-}" = /api/models/calibration/cancel ] || die "calibration cancellation ownership pairing: incomplete route ownership"
  cc_refused="$( { project_routes_core "$tree/core/server/rust-calibration-cancel.cjs" || true; } | tr '\n' ' ')"
  [ "$cc_refused" = 'ALL /api/models/calibration/cancel ' ] || die "calibration cancellation ownership pairing: Core must refuse exactly the cancellation path"
  ! code_has "$tree/core/server/routes/models.cjs" '/api/models/calibration/cancel' \
    || die "calibration cancellation ownership pairing: Core still carries the public handler"
  ! code_has "$tree/core/server/llamacpp-calibration.cjs" '(^|[^A-Za-z0-9_$])function[[:space:]]+cancel[[:space:]]*\(' \
    || die "calibration cancellation ownership pairing: Core still carries the public cancellation policy"
  code_has "$tree/core/server/llamacpp-manager.cjs" 'cancellationFacts: calibrator.cancellationFacts' \
    && code_has "$tree/core/server/llamacpp-manager.cjs" 'compareAndAbort: calibrator.compareAndAbort' \
    || die "calibration cancellation ownership pairing: Core does not expose the live actor primitives"
  [ -f "$tree/core/server/rust-calibration-cancel-bridge.cjs" ] || die "calibration cancellation ownership pairing: missing state bridge"
  cc_index="$(code_lines "$tree/core/server/index.cjs")"
  grep -qF 'calibrationCancelBridge(req, res)' <<<"$cc_index" || die "calibration cancellation ownership pairing: missing state bridge mount"
  cc_secret="$(sed -nE "s/^[[:space:]]*const ([A-Za-z_][A-Za-z0-9_]*) = String\(process\.env\.NOEVIA_PROJECT_STORE_TOKEN \|\| ''\);.*/\1/p" <<<"$cc_index" | head -1)"
  [ -n "$cc_secret" ] && awk -v c="$cc_secret" '
    /createCalibrationCancelBridge\(\{/ { bridge = NR }
    index($0, "secret: " c) && bridge && NR - bridge <= 3 { ok = 1 }
    END { exit !ok }
  ' <<<"$cc_index" || die "calibration cancellation ownership pairing: state bridge needs the front captured secret"
fi
#@calibration_routes
cr_core_owner=""; cr_rs_owner=""
grep -qF "require('./rust-calibration-routes.cjs')" "$tree/core/server/index.cjs" 2>/dev/null && cr_core_owner=rust
if move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES rust-calibration-routes features; then cr_rs_owner=rust; fi
[ "$cr_core_owner" = "$cr_rs_owner" ] || die "calibration start/read ownership pairing: Core and Rust must move together"
if [ -f "$tree/rs/contracts/http/routes.toml" ]; then
  cr_table="$(routes_toml "$tree/rs/contracts/http/routes.toml" 2>/dev/null)" || cr_table=""
  cr_rows="$(printf '%s\n' "$cr_table" | awk -F'\t' -v want="$cr_rs_owner" '
    $1 == "/api/models/calibration" {
      if (want == "rust" && ($2 != "rust" || $3 != "-" || $4 != "-")) exit 1
      if (want != "rust" && $2 != "node") exit 1
      print $1
    }')" || die "calibration start/read ownership pairing: all methods must belong to the paired owner"
fi
if [ "$cr_rs_owner" = rust ]; then
  [ "${cr_rows:-}" = /api/models/calibration ] || die "calibration start/read ownership pairing: incomplete route ownership"
  cr_refused="$( { project_routes_core "$tree/core/server/rust-calibration-routes.cjs" || true; } | tr '\n' ' ')"
  [ "$cr_refused" = 'ALL /api/models/calibration ' ] || die "calibration start/read ownership pairing: Core must refuse exactly the calibration path"
  ! code_has "$tree/core/server/routes/models.cjs" '/api/models/calibration' \
    || die "calibration start/read ownership pairing: Core still carries the public handler"
  ! code_has "$tree/core/server/llamacpp-calibration.cjs" '(async[[:space:]]+)?function[[:space:]]+(start|status)[[:space:]]*\(' \
    || die "calibration start/read ownership pairing: Core still carries the public start/status policy"
  code_has "$tree/core/server/llamacpp-manager.cjs" 'stateFacts:[[:space:]]*calibrator[.]stateFacts[[:space:]]*[,}]' \
    && code_has "$tree/core/server/llamacpp-manager.cjs" 'listingFacts:[[:space:]]*calibrator[.]listingFacts[[:space:]]*[,}]' \
    && code_has "$tree/core/server/llamacpp-manager.cjs" 'preparationFacts:[[:space:]]*calibrator[.]preparationFacts[[:space:]]*[,}]' \
    && code_has "$tree/core/server/llamacpp-manager.cjs" 'compareAndLaunch:[[:space:]]*calibrator[.]compareAndLaunch[[:space:]]*[,}]' \
    || die "calibration start/read ownership pairing: Core does not expose the live actor primitives"
  [ -f "$tree/core/server/rust-calibration-routes-bridge.cjs" ] || die "calibration start/read ownership pairing: missing state bridge"
  cr_index="$(code_lines "$tree/core/server/index.cjs")"
  grep -qF 'calibrationRoutesBridge(req, res)' <<<"$cr_index" || die "calibration start/read ownership pairing: missing state bridge mount"
  cr_secret="$(sed -nE "s/^[[:space:]]*const ([A-Za-z_][A-Za-z0-9_]*) = String\(process\.env\.NOEVIA_PROJECT_STORE_TOKEN \|\| ''\);.*/\1/p" <<<"$cr_index" | head -1)"
  [ -n "$cr_secret" ] && awk -v c="$cr_secret" '
    /createCalibrationRoutesBridge\(\{/ { bridge = NR }
    index($0, "secret: " c) && bridge && NR - bridge <= 3 { ok = 1 }
    END { exit !ok }
  ' <<<"$cr_index" || die "calibration start/read ownership pairing: state bridge needs the front captured secret"
fi
