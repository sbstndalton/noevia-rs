# Verbatim lines of noevia deploy/tools/assemble-release.sh at noevia origin/main f23eb731 (the pairing guards' shared primitives).
# Sections start with a '#@<name>' marker line; every other line is copied unchanged (tests/guard_primitives.rs checks that).
#@code_lines
code_lines() { grep -vE '^[[:space:]]*(//|/\*|\*|#)' "$1" 2>/dev/null || true; }
#@routes_toml
routes_toml() {
  LC_ALL=C awk '
    function die(msg) { printf "line %d: %s\n", NR, msg > "/dev/stderr"; bad = 1; exit 2 }
    function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t]+$/, "", s); return s }
    function orDash(s) { return s == "" ? "-" : s }
    function flush() {
      if (seen) print orDash(path) "\t" orDash(owner) "\t" (hasSw ? (sw == "" ? "?" : sw) : "-") "\t" orDash(methods)
      path = ""; owner = ""; sw = ""; methods = ""; hasSw = 0; seen = 0
    }
    function setkey(key, val) {
      if (key == "path" || key == "owner" || key == "switch" || key == "methods") {
        if (index(val, "\\")) die("an escape sequence in " key " is not read")
      }
      if (key == "path") path = val
      else if (key == "owner") owner = val
      else if (key == "switch") { hasSw = 1; sw = val }
      else if (key == "methods") methods = val
    }
    function tailok(t) { t = trim(t); if (t != "" && substr(t, 1, 1) != "#") die("unexpected text after the value: " t) }
    function scalar(rest,   q, e) {
      q = substr(rest, 1, 1)
      if (q == "\"" || q == "\047") {
        rest = substr(rest, 2); e = index(rest, q)
        if (!e) die("unterminated string")
        tailok(substr(rest, e + 1))
        return substr(rest, 1, e - 1)
      }
      sub(/[ \t]*#.*$/, "", rest)
      return trim(rest)
    }
    # One line of an array value: quotes are honoured (a # or ] inside them is text), the rest of
    # the line after a # outside them is a comment. Sets arr = 0 when the array closes.
    function scan(s,   i, c, q) {
      if (index(s, "\"\"\"") || index(s, "\047\047\047")) die("multi-line strings are not read")
      q = ""
      for (i = 1; i <= length(s); i++) {
        c = substr(s, i, 1)
        if (q != "") {
          if (c == "\\") die("an escape sequence in an array is not read")
          if (c == q) q = ""; else abuf = abuf c
          continue
        }
        if (c == "\"" || c == "\047") { q = c; continue }
        if (c == "#") break
        if (c == "[") { depth++; continue }
        if (c == "]") {
          if (--depth == 0) { tailok(substr(s, i + 1)); arr = 0; return }
          continue
        }
        abuf = abuf c
      }
      if (q != "") die("unterminated string in an array")
      abuf = abuf " "
    }
    NR == 1 && substr($0, 1, 3) == sprintf("%c%c%c", 239, 187, 191) { $0 = substr($0, 4) }
    {
      line = $0; sub(/\r$/, "", line); line = trim(line)
      if (arr) { scan(line); if (!arr && inroute) setkey(arrkey, trim(abuf)); next }
      if (line == "" || line ~ /^#/) next
      if (line ~ /^\[/) {
        flush(); hdr = 1
        if (!match(line, /^\[\[?[ \t]*[A-Za-z0-9_-]+([ \t]*\.[ \t]*[A-Za-z0-9_-]+)*[ \t]*\]\]?/)) die("table header is not a plain bare name, so it cannot be classified: " line)
        head = substr(line, 1, RLENGTH); tailok(substr(line, RLENGTH + 1))
        opens = 0; while (substr(head, opens + 1, 1) == "[") opens++
        closes = 0; while (substr(head, length(head) - closes, 1) == "]") closes++
        name = head; gsub(/[\[\] \t]/, "", name)
        if (opens != closes) die("unbalanced table header: " line)
        if (name == "route") { if (opens != 2) die("route must be an array of tables, [[route]]: " line); inroute = 1; seen = 1 }
        else if (name ~ /^route\./) die("a sub-table of route is not read: " line)
        else inroute = 0
        next
      }
      if (inroute) {
        if (!match(line, /^("[A-Za-z0-9_-]+"|\047[A-Za-z0-9_-]+\047|[A-Za-z0-9_-]+)[ \t]*=/)) die("cannot classify this line inside a [[route]] table (dotted keys and inline tables are not read): " line)
        key = substr(line, 1, RLENGTH - 1); sub(/[ \t]+$/, "", key); gsub(/["\047]/, "", key)
        rest = trim(substr(line, RLENGTH + 1))
      } else {
        eq = index(line, "=")
        if (!eq) next
        key = substr(line, 1, eq - 1); gsub(/[ \t"\047]/, "", key)
        if (!hdr && (key == "route" || key ~ /^route\./)) die("a top-level route key (an inline route array) is not read; use [[route]] tables: " line)
        rest = trim(substr(line, eq + 1))
      }
      if (substr(rest, 1, 3) == "\"\"\"" || substr(rest, 1, 3) == "\047\047\047") die("multi-line strings are not read")
      if (substr(rest, 1, 1) == "[") {
        arrkey = key; arr = 1; depth = 1; abuf = ""
        scan(substr(rest, 2))
        if (!arr && inroute) setkey(key, trim(abuf))
        next
      }
      if (inroute) setkey(key, scalar(rest))
    }
    END {
      if (bad) exit 2
      if (arr) { printf "line %d: unterminated array\n", NR > "/dev/stderr"; exit 2 }
      flush()
    }
  ' "$1"
}
project_routes_rs() {
  routes_toml "$1" | awk -F'\t' '
    $2 == "rust" && $3 == "-" && ($1 ~ /^\/api\/(projects\/|freechats$|freechats\/[^\/]+$)/ || $1 ~ /^\/api\/chats\/[^\/]+\/(context|move)$/) {
      if ($4 == "-") { print "ALL " $1; next }
      n = split($4, m, /[^A-Z]+/)
      for (i = 1; i <= n; i++) if (m[i] != "") print m[i] " " $1
    }
  ' | sed 's/{[^}]*}/{}/g' | sort -u
}
#@code_has
code_has() {
  [ -f "$1" ] || return 1
  local code
  code="$(awk '
    block { if (index($0, "*/")) block = 0; next }
    { line = $0; sub(/^[ \t]+/, "", line) }
    line ~ /^\/\// { next }
    line ~ /^\/\*/ { if (!index(line, "*/")) block = 1; next }
    { print }
  ' "$1")" || return 2
  grep -qE -e "$2" <<<"$code"
}
#@move_capability
move_capability() {
  [ -f "$1" ] || return 1
  awk -v name="$2" -v value="$3" -v kind="$4" '
    function clean(s) { gsub(/^[ \t]+|[ \t]+$/, "", s); return s }
    {
      line=clean($0)
      if (block) { if (index(line,"*/")) block=0; next }
      if (line ~ /^\/\//) next
      if (index(line,"/*")) { if (!index(line,"*/")) block=1; next }
      if ((kind=="features" || kind=="feature_ref") && index(line,"pub const FEATURES: &[&str] = &[")==1) { decl++; features=1; line=substr(line,length("pub const FEATURES: &[&str] = &[")+1) }
      if (features) {
        fend=index(line,"];")
        if(fend) line=substr(line,1,fend-1)
        n=split(line,fields,",")
        for(i=1;i<=n;i++) if(clean(fields[i])==(kind=="feature_ref" ? value : sprintf("%c",34) value sprintf("%c",34))) observed++
        if(fend) features=0
      }
      if (kind=="rust" && line == "pub const " name ": &str = \"" value "\";") decl++
      if (kind=="js" && line == "const " name " = " sprintf("%c",39) value sprintf("%c",39) ";") decl++
      if (kind=="function" && line ~ ("^async function " name "\\(")) decl++
      if (index(line,"module.exports = {")==1) { exporting=1; line=substr(line,length("module.exports = {")+1) }
      if (exporting) {
        ending=index(line,"}")
        if (ending) line=substr(line,1,ending-1)
        n=split(line,fields,",")
        for(i=1;i<=n;i++) if(clean(fields[i])==name) exported++
        if(ending) exporting=0
      }
    }
    END { exit !(decl==1 && (kind=="rust" || ((kind=="features" || kind=="feature_ref") && observed==1) || (kind!="features" && exported==1))) }
  ' "$1"
}
#@switched_routes
switched_routes() {
  routes_toml "$1" | awk -F'\t' '$3 != "-" { print $3 " " $1 }'
}
project_routes_core() {
  [ -f "$1" ] || return 0
  sed -n "/^const ${2:-OWNED_ROUTES} = /,/]))/p" "$1" | grep -o "'[A-Z]* /[^']*'" | tr -d "'" | sed 's/{[^}]*}/{}/g' | sort -u
}
