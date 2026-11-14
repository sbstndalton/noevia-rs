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
