# Excerpts of noevia deploy/tools/assemble-release.sh at noevia origin/main 11bf2da7, verbatim (the oracle
# of tests/shell_oracle.rs). Line 255 (lock_value), lines 297-342 (resolve_component, the component and
# noevia-rs steps) and the end of the assembly (for need ... mv -f). tests/shell_oracle.rs checks they still
# appear verbatim in the real script when ASSEMBLE_RELEASE_SH points to it.
#@resolve
lock_value() { printf '%s\n' "$lock" | sed -n "s/^$1=//p" | tail -n 1; }
resolve_component() {
  local key="$1" repo_name="$2" dest="$3"; shift 3
  local ref sum
  ref="$(lock_value "${key}_REF")"
  sum="$(lock_value "${key}_SHA256")"
  # Locks from before #952 have no NOEVIA_SERVICES_REF: services were always exported from here.
  if [ -z "$ref" ] && [ "$key" = NOEVIA_SERVICES ] && ! printf '%s\n' "$lock" | grep -q '^NOEVIA_SERVICES_REF='; then
    ref=self
  fi
  if [ "$ref" = self ]; then
    export_self "$dest" "$@"
    printf '%s' "$full_sha"
  elif [[ "$ref" =~ ^[0-9a-f]{40}$ ]]; then
    [[ "$sum" =~ ^[0-9a-f]{64}$ ]] || die "${key}_REF is pinned but ${key}_SHA256 is missing or not 64 lowercase hex"
    fetch_remote "$repo_name" "$ref" "$sum" "$dest"
    printf '%s' "$ref"
  else
    die "${key}_REF in versions.lock at $sha must be 'self' or a 40-char lowercase sha, got: '${ref}'"
  fi
}

web_sha="$(resolve_component NOEVIA_WEB noevia-web "$tree/web" apps/web \
  src public index.html vite.config.ts tsconfig.json package.json package-lock.json scripts contracts tests/client qa \
  tests/hermetic-network.cjs .gitignore .dockerignore)"
core_sha="$(resolve_component NOEVIA_CORE noevia-core "$tree/core" apps/web \
  server contracts tests/server tests/fixtures tests/hermetic-network.cjs)"
if [ "$(lock_value NOEVIA_CORE_REF)" = self ]; then
  # noevia-core also owns services/code-sandbox, at code-sandbox/ (tools/repo-split's map).
  export_self "$tree/core" services code-sandbox
fi
services_sha="$(resolve_component NOEVIA_SERVICES noevia-services "$tree/services" services \
  diary docling laya model-manager ocr)"

# noevia-rs (full-Rust migration M1): the web image builds its Rust front from this tree's rs/.
# Locks from before M1 pin no tarball checksum for noevia-rs; their trees carry an empty rs/.
mkdir -p "$tree/rs"
rs_ref="$(lock_value NOEVIA_RS_REF)"
rs_sum="$(lock_value NOEVIA_RS_SHA256)"
rs_sha=""
if [ -n "$rs_sum" ]; then
  [[ "$rs_ref" =~ ^[0-9a-f]{40}$ ]] || die "NOEVIA_RS_REF in versions.lock at $sha must be a 40-char lowercase sha, got: '${rs_ref}'"
  [[ "$rs_sum" =~ ^[0-9a-f]{64}$ ]] || die "NOEVIA_RS_SHA256 in versions.lock at $sha is not 64 lowercase hex"
  fetch_remote noevia-rs "$rs_ref" "$rs_sum" "$tree/rs"
  [ -f "$tree/rs/Cargo.toml" ] || die "noevia-rs at $rs_ref has no Cargo.toml"
  rs_sha="$rs_ref"
fi
#@finish
for need in web/package.json web/package-lock.json web/src web/index.html core/server/package.json core/server/index.cjs core/contracts \
            core/code-sandbox/Dockerfile services/diary/Dockerfile services/docling/Dockerfile services/laya/Dockerfile \
            services/model-manager/Dockerfile services/ocr/Dockerfile; do
  [ -e "$tree/$need" ] || die "assembled tree lacks $need"
done

printf 'NOEVIA_SHA=%s\nNOEVIA_WEB_SHA=%s\nNOEVIA_CORE_SHA=%s\nNOEVIA_SERVICES_SHA=%s\nNOEVIA_RS_SHA=%s\n' \
  "$sha" "$web_sha" "$core_sha" "$services_sha" "$rs_sha" > "$tree/release-refs"
# The web image's entrypoint is the one file of noevia/ its build needs.
# Runtime state must never enter an assembled release, even from an old source archive.
find "$tree" -type d -path '*/server/ui-data' -prune -exec rm -rf {} +
printf 'noevia\n!noevia/build/web-supervisor.sh\nservices\n**/server/ui-data\n' > "$tree/.dockerignore"

out="$out_dir/noevia-release-$sha.tar.gz"
out_partial="$out.partial"
COPYFILE_DISABLE=1 tar -czf "$out_partial" -C "$tree" .dockerignore release-refs noevia web core services rs
mv -f "$out_partial" "$out"
