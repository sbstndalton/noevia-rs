# Verbatim lines of noevia deploy/tools/assemble-release.sh at noevia origin/main f23eb731: the node-backed primitive
# core_requires, code_body, the ex_* names and the three track R11 model pairings (council p12). Each '#@<name>' section
# is a contiguous run of the script (tests/model_pairings.rs checks it when ASSEMBLE_RELEASE_SH is set).
#@core_requires
core_requires() {
  [ -f "$1" ] || { echo no; return; }
  command -v node >/dev/null 2>&1 || { echo invalid; return; }
  timeout -k 2 10 node -e '
    const raw = require("node:fs").readFileSync(process.argv[1], "utf8"), want = process.argv[2];
    const vm = require("node:vm");
    const s = raw.charCodeAt(0) === 0xfeff ? raw.slice(1) : raw, n = s.length, toks = [], stack = [];
    const bad = () => { process.stdout.write("invalid"); process.exit(0); };
    let i = 0;
    if (s.startsWith("#!")) while (i < n && s[i] !== "\n") i++;
    const kw = new Set(["return", "typeof", "instanceof", "in", "of", "new", "delete", "void", "throw", "case", "do", "else", "yield", "await"]);
    // A slash starts a regex after an operator, an opening bracket or a keyword; after a value it divides.
    const regexOk = () => {
      const t = toks[toks.length - 1];
      if (!t) return true;
      if (t.k === "p") return t.v !== ")" && t.v !== "]";
      return t.k === "id" && kw.has(t.v);
    };
    // The value of one complete string or template literal (only a literal: the scanner delimited it).
    const cooked = (lit) => {
      if (!lit.includes("\\")) return lit.slice(1, -1);
      try { const v = vm.runInNewContext(lit, {}, { timeout: 500 }); return typeof v === "string" ? v : bad(); } catch { bad(); }
    };
    // Template text from i (just past the backtick, or the closing brace of ${...} when first is false)
    // to its end or the next ${. A whole template without substitutions is a string value.
    const tmpl = (first) => {
      const start = i - 1;
      for (;;) {
        if (i >= n) bad();
        const c = s[i];
        if (c === "\\") { i += 2; continue; }
        if (c === "`") { i++; toks.push(first ? { k: "s", v: cooked(s.slice(start, i)) } : { k: "t" }); return; }
        if (c === "$" && s[i + 1] === "{") { i += 2; stack.push("t"); toks.push({ k: "p", v: "(" }); return; }
        i++;
      }
    };
    while (i < n) {
      const c = s[i];
      if (/\s/.test(c)) { i++; continue; }
      if (c === "/" && s[i + 1] === "/") { while (i < n && s[i] !== "\n") i++; continue; }
      if (c === "/" && s[i + 1] === "*") { const e = s.indexOf("*/", i + 2); if (e < 0) bad(); i = e + 2; continue; }
      if (c === "\x27" || c === "\"") {
        let j = i + 1;
        for (;; j++) {
          if (j >= n || s[j] === "\n") bad();
          if (s[j] === "\\") { j++; continue; }
          if (s[j] === c) break;
        }
        toks.push({ k: "s", v: cooked(s.slice(i, j + 1)) }); i = j + 1; continue;
      }
      if (c === "`") { i++; tmpl(true); continue; }
      if (c === "{") { stack.push("{"); toks.push({ k: "p", v: c }); i++; continue; }
      if (c === "}") {
        if (stack[stack.length - 1] === "t") { stack.pop(); toks.push({ k: "p", v: ")" }); i++; tmpl(false); continue; }
        stack.pop(); toks.push({ k: "p", v: c }); i++; continue;
      }
      if (c === "/" && regexOk()) {
        let j = i + 1, cls = false;
        for (;; j++) {
          if (j >= n || s[j] === "\n") bad();
          const d = s[j];
          if (d === "\\") { j++; continue; }
          if (cls) { if (d === "]") cls = false; continue; }
          if (d === "[") cls = true; else if (d === "/") break;
        }
        j++; while (j < n && /[A-Za-z]/.test(s[j])) j++;
        toks.push({ k: "r" }); i = j; continue;
      }
      if (/[A-Za-z_$\u0080-\uffff]/.test(c)) {
        let j = i + 1; while (j < n && /[\w$\u0080-\uffff]/.test(s[j])) j++;
        toks.push({ k: "id", v: s.slice(i, j) }); i = j; continue;
      }
      if (/[0-9]/.test(c) || (c === "." && /[0-9]/.test(s[i + 1] || ""))) {
        let j = i + 1; while (j < n && /[\w.]/.test(s[j])) j++;
        toks.push({ k: "n" }); i = j; continue;
      }
      toks.push({ k: "p", v: c }); i++;
    }
    let found = false;
    for (let k = 0; k + 3 < toks.length; k++) {
      const a = toks[k], b = toks[k + 1], m = toks[k + 2], e = toks[k + 3], before = toks[k - 1];
      if (a.k === "id" && a.v === "require" && !(before && before.k === "p" && before.v === ".")
        && b.k === "p" && b.v === "(" && m.k === "s" && m.v === want && e.k === "p" && e.v === ")") found = true;
    }
    process.stdout.write(found ? "yes" : "no");
  ' "$1" "$2" 2>/dev/null || echo invalid
}
#@code_body
code_body() {
  [ -f "$1" ] || return 0
  awk '
    block { if (index($0, "*/")) block = 0; next }
    /^#\[cfg\(test\)\]/ { pend = 1; next }
    pend { pend = 0; if ($0 ~ /^(pub(\([a-z]+\))? )?mod[ \t]/) exit }
    { line = $0; sub(/^[ \t]+/, "", line) }
    line ~ /^\/\// { next }
    line ~ /^\/\*/ { if (!index(line, "*/")) block = 1; next }
    { print }
  ' "$1"
}
#@ex_names
ex_rs_src="$tree/rs/bins/noevia-server/src"
ex_core="$tree/core/server"
# A front that names a switch but whose executor module is not where the guard looks is refused: the markers moved.
ex_front_named() { grep -rqsF --include='*.rs' -e "$1" "$ex_rs_src"; }
#@model_writes
# Rust port R11: the inference budget and idle unload PUTs (NOEVIA_RUST_MODEL_WRITES). The front's side is
# model_writes_routes.rs (a dispatch override: the route rows stay GET-only); Core's is rust-model-writes.cjs (its
# refusal of both PUTs under the same switch) with inference-budget.cjs reading the saved row on every use, so a
# budget the front saves is enforced at once. old/old and new/new assemble; a front with the writer beside a Core
# without that pair is refused (Core would go on answering its own PUTs, and would keep enforcing the budget it read
# at its start); a Core ahead of a front WITHOUT the writer assembles (its refusal waits for a switch only that front
# answers); beside a front with the writer, the later requests (preset, MTP check, pull) must match on both sides.
if [ -f "$ex_rs_src/model_writes_routes.rs" ]; then
  mw_front="$(code_body "$ex_rs_src/model_writes_routes.rs")"
  mw_loaded="$(core_requires "$ex_core/index.cjs" ./rust-model-writes.cjs)"
  [ "$mw_loaded" != invalid ] || die "model writes pairing: Core's index.cjs could not be read"
  { [ "$mw_loaded" = yes ] && [ -f "$ex_core/rust-model-writes.cjs" ]; } \
    || die "model writes pairing: the Rust front answers the inference budget and idle unload PUTs (model_writes_routes.rs, NOEVIA_RUST_MODEL_WRITES=rust) but Core does not load server/rust-model-writes.cjs (Core would go on answering them)"
  { grep -qF 'pub const SWITCH: &str = "NOEVIA_RUST_MODEL_WRITES";' <<<"$mw_front" && grep -qF 'get(SWITCH).as_deref() == Some("rust")' <<<"$mw_front"; } \
    && code_has "$ex_core/rust-model-writes.cjs" "^const SWITCH = 'NOEVIA_RUST_MODEL_WRITES';" \
    && code_has "$ex_core/rust-model-writes.cjs" "env\\[SWITCH\\][[:space:]]*===[[:space:]]*'rust'" \
    || die "model writes pairing: the Rust front's NOEVIA_RUST_MODEL_WRITES=rust and Core's rust-model-writes.cjs do not read the same switch value (the PUTs would be answered by both, or by neither)"
  code_has "$ex_core/inference-budget.cjs" '^[[:space:]]*function savedGib[(]' \
    || die "model writes pairing: the Rust front saves the inference budget row but Core's inference-budget.cjs does not read it on every use (it would enforce the figure it read at its start until it restarts)"
  # The preset editor's save (track R11 step 8): a front that answers it needs Core to refuse it under the switch too.
  # Track R11 steps 8, 11 and 12: the preset save, the MTP artifact check and the pull. Each is refused by Core under the
  # switch exactly when the front answers it: a front answering one Core does not refuse would leave two answering it;
  # a Core refusing one the front does not answer would refuse what the front relays (a dead end once the switch is set).
  # A list is read from its `const <name> = ` line to the line that closes it (`]);` or `];`), on one line or several,
  # with every `//` comment cut first, so neither a comment nor the next list can name a path for it. A failed read (no
  # body, or a list not closed before the next declaration or the end) is status 2, which both directions below refuse
  # with their own message.
  mw_core_has() { # <core list> <path>
    local body list
    body="$(code_body "$ex_core/rust-model-writes.cjs")" || return 2
    list="$(awk -v start="const $1 = " '
      on && /^(const|let|var|function|module[.]exports) / { exit 2 }
      !on && index($0, start) == 1 { on = 1 }
      on { sub(/\/\/.*/, ""); print; if ($0 ~ /[]][)]?;/) { done = 1; exit } }
      END { if (on && !done) exit 2 }
    ' <<<"$body")" || return 2
    grep -qF "'$2'" <<<"$list"
  }
  mw_pair() { # <what> <front answers: yes|no> <core list> <path>
    local rc=0
    mw_core_has "$3" "$4" || rc=$?
    [ "$rc" -le 1 ] || die "model writes pairing: Core's rust-model-writes.cjs could not be read for $1 (a list not closed before the next declaration)"
    if [ "$2" = yes ]; then [ "$rc" -eq 0 ] || die "model writes pairing: the Rust front answers $1 but Core's rust-model-writes.cjs does not refuse it (both would answer it)"
    else [ "$rc" -ne 0 ] || die "model writes pairing: Core's rust-model-writes.cjs refuses $1 but the Rust front does not answer it (every such request would be refused once the switch is set)"; fi
  }
  mw_preset=no; grep -qE '^pub const PRESET: &str = ' <<<"$mw_front" && mw_preset=yes
  mw_mtp=no; [ -f "$ex_rs_src/model_pull_routes.rs" ] && mw_mtp=yes
  mw_pull=no; grep -qsE '^pub const PULL: &str = ' <<<"$(code_body "$ex_rs_src/model_pull_routes.rs")" && mw_pull=yes
  mw_pair "PUT /api/models/preset" "$mw_preset" OWNED_PATHS /api/models/preset
  mw_pair "GET /api/models/mtp-artifact" "$mw_mtp" OWNED_GETS /api/models/mtp-artifact
  mw_pair "POST /api/models/pull" "$mw_pull" OWNED_POSTS /api/models/pull
elif ex_front_named NOEVIA_RUST_MODEL_WRITES; then
  die "model writes pairing: the Rust front names NOEVIA_RUST_MODEL_WRITES but has no bins/noevia-server/src/model_writes_routes.rs (the writer moved: update this guard)"
fi
#@model_manager
# Rust port R11: the model manager proxy (NOEVIA_RUST_MODEL_MANAGER). The front's side is model_manager_routes.rs (a
# dispatch override for /api/model-manager/*, POST benchmark/sweep relayed to Core); Core's is rust-model-manager-proxy.cjs
# (its refusal of the rest under the same switch), and the front's host clears Core's folder scan cache through the
# trusted bridge's `clear` op (noevia#2550). old/old, a Core ahead and new/new assemble; a front with the proxy beside a
# Core without the refusal, with another switch value, or without the scan cache clear is refused.
if [ -f "$ex_rs_src/model_manager_routes.rs" ]; then
  mm_front="$(code_body "$ex_rs_src/model_manager_routes.rs")"
  mm_loaded="$(core_requires "$ex_core/index.cjs" ./rust-model-manager-proxy.cjs)"
  [ "$mm_loaded" != invalid ] || die "model manager proxy pairing: Core's index.cjs could not be read"
  { [ "$mm_loaded" = yes ] && [ -f "$ex_core/rust-model-manager-proxy.cjs" ]; } \
    || die "model manager proxy pairing: the Rust front answers /api/model-manager/* (model_manager_routes.rs, NOEVIA_RUST_MODEL_MANAGER=rust) but Core does not load server/rust-model-manager-proxy.cjs (Core would go on answering it)"
  { grep -qF 'pub const SWITCH: &str = "NOEVIA_RUST_MODEL_MANAGER";' <<<"$mm_front" && grep -qF 'get(SWITCH).as_deref() == Some("rust")' <<<"$mm_front"; } \
    && code_has "$ex_core/rust-model-manager-proxy.cjs" "^const SWITCH = 'NOEVIA_RUST_MODEL_MANAGER';" \
    && code_has "$ex_core/rust-model-manager-proxy.cjs" "env\\[SWITCH\\][[:space:]]*===[[:space:]]*'rust'" \
    || die "model manager proxy pairing: the Rust front's NOEVIA_RUST_MODEL_MANAGER=rust and Core's rust-model-manager-proxy.cjs do not read the same switch value (the proxy would be answered by both, or by neither)"
  code_has "$ex_core/rust-model-manager-scan-cache.cjs" "'clear'" \
    || die "model manager proxy pairing: the Rust front clears Core's folder scan cache but Core's rust-model-manager-scan-cache.cjs has no clear op (a Core from before noevia#2550)"
elif ex_front_named NOEVIA_RUST_MODEL_MANAGER; then
  die "model manager proxy pairing: the Rust front names NOEVIA_RUST_MODEL_MANAGER but has no bins/noevia-server/src/model_manager_routes.rs (the proxy moved: update this guard)"
fi
#@embedding_admin
# Rust port R11 step 10: /api/embedding (NOEVIA_RUST_EMBEDDING_ADMIN). The front's side is embedding_admin_routes.rs (it
# answers the path, every method, and writes embedding-config.json); Core's is rust-embedding-admin.cjs (its refusal of the
# path under the same switch) and embedding-config.cjs's `reread` (it reads the file again when the front changes it, so
# retrieval and the delete guard follow), switched on from index.cjs by the same module, and embedding-service.cjs's
# reading of the front's announced service changes (the delete guard's protection). old/old, a Core ahead and new/new
# assemble; a front answering it beside a Core without the refusal, the re-read, the announcement or the same switch
# value is refused, and so is a front that does not announce its changes.
if [ -f "$ex_rs_src/embedding_admin_routes.rs" ]; then
  ea_front="$(code_body "$ex_rs_src/embedding_admin_routes.rs")"
  ea_loaded="$(core_requires "$ex_core/index.cjs" ./rust-embedding-admin.cjs)"
  [ "$ea_loaded" != invalid ] || die "embedding admin pairing: Core's index.cjs could not be read"
  { [ "$ea_loaded" = yes ] && [ -f "$ex_core/rust-embedding-admin.cjs" ] \
    && grep -qF "require('./rust-embedding-admin.cjs').refuseOwned(req, res" <<<"$(code_body "$ex_core/index.cjs")"; } \
    || die "embedding admin pairing: the Rust front answers /api/embedding (embedding_admin_routes.rs, NOEVIA_RUST_EMBEDDING_ADMIN=rust) but Core does not refuse it through server/rust-embedding-admin.cjs (Core would go on answering it)"
  { grep -qF 'pub const SWITCH: &str = "NOEVIA_RUST_EMBEDDING_ADMIN";' <<<"$ea_front" && grep -qF 'get(SWITCH).as_deref() == Some("rust")' <<<"$ea_front"; } \
    && code_has "$ex_core/rust-embedding-admin.cjs" "^const SWITCH = 'NOEVIA_RUST_EMBEDDING_ADMIN';" \
    && code_has "$ex_core/rust-embedding-admin.cjs" "env\\[SWITCH\\][[:space:]]*===[[:space:]]*'rust'" \
    || die "embedding admin pairing: the Rust front's NOEVIA_RUST_EMBEDDING_ADMIN=rust and Core's rust-embedding-admin.cjs do not read the same switch value (the path would be answered by both, or by neither)"
  { code_has "$ex_core/embedding-config.cjs" '^[[:space:]]*function refresh[(][)]' \
    && grep -qF "reread: require('./rust-embedding-admin.cjs').frontOwns()" <<<"$(code_body "$ex_core/index.cjs")"; } \
    || die "embedding admin pairing: the Rust front writes embedding-config.json but Core's embedding configuration does not read it again under the switch (embedding-config.cjs reread from index.cjs): Core would search and guard deletes with the model it read at its start"
  # The front changes the embedding service itself; Core's delete guard protects what it asks for only through the
  # announced change (embedding-service-intent.json, rust-embedding-service-intent-v1): both sides must speak it.
  grep -qF '"rust-embedding-service-intent-v1"' <<<"$ea_front" \
    || die "embedding admin pairing: the Rust front changes the embedding service without announcing it (rust-embedding-service-intent-v1): Core's delete guard would not protect the model being loaded"
  { code_has "$ex_core/embedding-service.cjs" "^const INTENT_PROTOCOL = 'rust-embedding-service-intent-v1';" \
    && grep -qF "intentFile: require('./rust-embedding-admin.cjs').frontOwns() ?" <<<"$(code_body "$ex_core/index.cjs")"; } \
    || die "embedding admin pairing: the Rust front announces its embedding service changes but Core does not read them (embedding-service.cjs intent, from index.cjs): Core's delete guard would not protect the model being loaded"
elif ex_front_named NOEVIA_RUST_EMBEDDING_ADMIN; then
  die "embedding admin pairing: the Rust front names NOEVIA_RUST_EMBEDDING_ADMIN but has no bins/noevia-server/src/embedding_admin_routes.rs (the route moved: update this guard)"
fi
