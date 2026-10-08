# noevia-rs

Rust workspace for noevia strangler slices: self-contained Rust replacements for
pieces of [noevia](https://github.com/sbstndalton/noevia), built and tested here
one slice at a time.

- Integration, releases and the issue tracker live in
  [sbstndalton/noevia](https://github.com/sbstndalton/noevia). File issues there.
- Every slice ships dark. Nothing here replaces a live code path until the owner
  switches it on.

## Layout

- `crates/` — libraries
- `bins/` — command-line binaries

## Slices

- `crates/gguf` + `bins/gguf-meta` (sbstndalton/noevia#896): bounded GGUF metadata
  reader. `gguf-meta <path>` prints the same summary JSON as model-manager's
  `gguf_meta.py`. Its differential corpus is regenerated from the Python reference with
  `NOEVIA_GGUF_META=<noevia>/services/model-manager/app/gguf_meta.py python3 tools/gen-fixtures.py`.
  `gguf::node` is noevia-core's gguf-meta.cjs instead (`summarize(readGguf(file))`, its own
  caps and thrown errors): the host passes the file's size and the byte ranges it has read and
  is told the next range to read; bytes the JS skips (long strings, the rest of big arrays) are
  never read or passed. Same summary
  as the JS byte for byte (numbers as `Number#toString`, NaN/±Infinity/-0 tagged, mode() ties,
  `kv[null]`). Stricter than the JS, as refusals: arrays nested past 64, more than 262,144 kept
  values, more than 24 MiB of read (not skipped) header bytes in 512 ranges. In `dav-parse.wasm` as `gguf_summary`
  (GGUF_META_IMPL). Table from noevia-core (`node tools/gen-gguf-meta-fixtures.cjs`, synthetic
  headers incl. truncated at every byte, huge counts and lengths, odd types), copied
  byte-for-byte to `crates/gguf/tests/fixtures/gguf-meta.v1.json`.
- `crates/policy-leaves`: noevia-core's auth-tokens.cjs (`resolveAuthTokens`, #294: both tokens
  trimmed as `String#trim` over UTF-16 units, no fallback between them, the two warnings) and
  tool-policy.cjs's per-tool decision and `set()` checks (the table stays in the JS). Never
  weaker than the JS: a stored mode outside the table's CHECK is `block`, writes are never
  `allow`; no refusal carries input, so no token is ever echoed. In `dav-parse.wasm` as
  `auth_tokens` (a secret call) and `tool_policy` (POLICY_LEAVES_IMPL). Table from noevia-core
  (`node tools/gen-policy-leaves-fixtures.cjs`, synthetic tokens only), copied byte-for-byte to
  `crates/policy-leaves/tests/fixtures/policy-leaves.v1.json`.
- `crates/review-verdict`: noevia-core's code-review-verdict.cjs (#519): `readVerdict` (the
  strict reading of a Planner review verdict: only the schema's fields, controls and bidi
  overrides dropped, trimmed and cut by code points, inconsistent verdicts refused, with the JS's
  nine reasons) and `boundReviewEvent` (what a `review.*` job event keeps, -0 included). The host
  sends JS values in a tagged JSON form that keeps undefined/-0/NaN and hides objects that are not
  plain data; where the JS would look inside one, the port refuses (`opaque`), which the host
  turns into the JS's own failure. In `dav-parse.wasm` as `review_verdict`
  (CODE_REVIEW_VERDICT_IMPL). Table from noevia-core (`node tools/gen-code-review-verdict-fixtures.cjs`,
  synthetic text only), copied byte-for-byte to
  `crates/review-verdict/tests/fixtures/code-review-verdict.v1.json`.
- `crates/tool-exchange`: noevia-core's tool-exchange.cjs checks before a chat tool runs
  (cancelled, enabled, arguments a JSON object, exactly as `JSON.parse`), its canonical-JSON
  dedupe key (keys in UTF-16 order, numbers as `JSON.stringify` writes them, lone surrogates
  escaped) and a failed call's text; the per-turn exchange stays in the JS. Stricter, as
  refusals (the host answers with an error, the tool never runs): arguments over 4 Mi units or
  nested 1,024 containers deep. In `dav-parse.wasm` as `tool_exchange` (TOOL_EXCHANGE_IMPL).
  Table from noevia-core (`node tools/gen-tool-exchange-fixtures.cjs`), copied byte-for-byte to
  `crates/tool-exchange/tests/fixtures/tool-exchange.v1.json`.
- `crates/mcp-servers`: noevia-core's mcp-servers.cjs: `parseMcpServers` (MCP_SERVERS
  `id|url|auth` entries or MCP_SERVER_URL: http(s) only, no credentials in the URL, `internal`
  only on a loopback IP literal and once, `bearer:ENV_NAME` by name, the JS's warnings word for
  word), `parseEnabledToolboxes` and `toolboxOffered`. Token values never cross. Stricter, as a
  refusal of the whole list (the host configures no MCP server): a URL it must judge that is not
  printable ASCII or has `%` / `xn--` in its authority, or an id re-accepted after a URL drop. In
  `dav-parse.wasm` as `mcp_servers` (MCP_SERVERS_IMPL). Table from noevia-core
  (`node tools/gen-mcp-servers-fixtures.cjs`, synthetic URLs only), copied byte-for-byte to
  `crates/mcp-servers/tests/fixtures/mcp-servers.v1.json`.
- `crates/decision`: noevia-core's decision/index.cjs pure checks: `invalidRequest`,
  `invalidResult` (a backend's answer stays inside the offered ids, `SameValueZero`, hashed) and
  `causeOf`; the chain, deadlines and backends stay in the JS. The host projects exactly the
  values they read; where the JS would throw, compare object identities, coerce an object or walk
  a sparse array, the port refuses and the host falls back. In `dav-parse.wasm` as `decision`
  (DECISION_IMPL). Table from noevia-core (`node tools/gen-decision-fixtures.cjs`), copied
  byte-for-byte to `crates/decision/tests/fixtures/decision.v1.json`.
- `crates/egress` + `bins/egress-proxy` (sbstndalton/noevia#926): deny-by-default egress
  proxy for coding tasks (port of `code-egress.cjs` + `ssrf.cjs` `isPrivateIp`).
  `egress-proxy --grants <file.json> --listen 127.0.0.1:<port>`. Its differential corpus is
  regenerated with
  `NOEVIA_CHECKOUT=<noevia> node tools/egress-diff.cjs > crates/egress/tests/fixtures/egress-diff.json`.
- `crates/model-files` + `bins/model-files` (sbstndalton/noevia#964): the model manager's
  model-files front. `model-files tree < listing.json` turns a Hugging Face repository tree
  listing (untrusted, from the network) into the file entries the service uses (path, size,
  quant label, shard group), exactly as noevia-services' `model-manager/app/model_files.py`
  `files_from_tree_py` does, Python's Unicode rules included. Bounded (16 MiB input, 100k
  entries, 4096-char strings, depth 64), no network or filesystem. The Unicode tables and
  the differential corpus are regenerated from the Python reference (on the service's Python,
  3.12) with
  `uv run --python 3.12 tools/gen-model-files.py --model-manager <noevia-services>/model-manager`;
  copy `crates/model-files/tests/fixtures/model-files.v1.json` into noevia-services'
  `model-manager/tests/fixtures/` when it changes.
- `crates/dav-parse` + `bins/dav-parse-wasm` (sbstndalton/noevia#967): the storage browser's
  WebDAV PROPFIND (`Depth: 1`) listing parser, a port of noevia-core's `server/dav-listing.cjs`
  `listingRecordsJs` (entity decoding, WHATWG href resolution, `decodeURIComponent`, direct
  children of the requested folder only). Not an XML parser: no DTD, entities or external
  resources. Bounded (16 MiB body, 8 KiB URL, 100k responses), typed errors. Shipped as
  WebAssembly with no imports: `tools/build-dav-parse-wasm.sh [out]` builds `dav-parse.wasm`
  reproducibly (paths remapped) and prints its sha256, which noevia-core pins in
  `server/dav-parse.lock`. The differential table is generated by noevia-core
  (`node tools/gen-dav-listing-fixtures.cjs`); copy it to
  `crates/dav-parse/tests/fixtures/dav-listing.v1.json` byte-for-byte when it changes.
- `crates/s3-list-parse` (sbstndalton/noevia#976): one S3 ListObjectsV2 page (CommonPrefixes,
  Contents, IsTruncated, NextContinuationToken), a port of noevia-core's `server/s3-listing.cjs`
  `s3PageRecordsJs` on dav-parse's forward-only scanner and entity decoder. Bounded (12 MiB
  decoded body = Node's 4 MiB read with U+FFFD expansion, 64 KiB prefix, 200k blocks).
- `crates/storage-path` (sbstndalton/noevia#978): `safeRelativePath`, `cleanRoot`, `joinRoot` and
  the upload filename rule, a port of noevia-core's `server/storage-path.cjs`. 64 KiB per argument.
- Both ship inside the same `dav-parse.wasm` (exports `s3_list` and `storage_path(op)` next to
  `dav_list`), so one lock pins all three switches. Their differential tables come from
  noevia-core (`node tools/gen-storage-fixtures.cjs s3|path`); copy them to
  `crates/s3-list-parse/tests/fixtures/s3-list.v1.json` and
  `crates/storage-path/tests/fixtures/storage-path.v1.json` byte-for-byte when they change.
- `crates/upload-sniff` (sbstndalton/noevia#977): the upload checks, a port of noevia-core's
  `server/upload-sniff.cjs`: `validate` (the storage-path filename rule, the 25 MiB cap, archive
  names and magic numbers; only the first 262 bytes are read), `classify` and `decodeText` (BOM,
  then NUL means binary, then strict UTF-8, then WHATWG windows-1252, byte-identical to Node's
  `TextDecoder`). In the same `dav-parse.wasm` as `upload_validate`, `upload_classify` and
  `upload_decode` (raw reply: tag byte + UTF-8 text). The module's input cap rises to 25 MiB for
  `upload_decode`; every other call keeps its own smaller cap. Table from noevia-core
  (`node tools/gen-upload-fixtures.cjs`), copied byte-for-byte to
  `crates/upload-sniff/tests/fixtures/upload-sniff.v1.json`.
- `crates/secret-envelope` (sbstndalton/noevia#979): stored-credential envelopes, a port of
  noevia-core's `server/secret-envelope.cjs` (`enc:v1` without AAD, `enc:v2` with AAD
  `noevia:user:<id>`, AES-256-GCM from RustCrypto `aes-gcm` =0.10.3, no `getrandom`). Node keeps
  the key files and passes key bytes (and, to seal, a 12-byte nonce from `crypto.randomBytes`) per
  call, so the module still imports nothing. In `dav-parse.wasm` as `secret_open` and
  `secret_seal`; both wipe their input buffer and previous reply (`zeroize`), and the host wipes
  the whole linear memory and drops the instance after each call. Table from noevia-core
  (`node tools/gen-secret-fixtures.cjs`; synthetic keys), copied byte-for-byte to
  `crates/secret-envelope/tests/fixtures/secret-envelope.v1.json`.
- `crates/sandbox-bridge` + `bins/sandbox-bridge-wasm` (sbstndalton/noevia#999): the code
  sandbox bridge's untrusted-input handling, a port of noevia-core's `code-sandbox/`
  `pi-acp-bridge.cjs` (`lines()` JSONL framing, `toolCallFor()`) and `supervisor.cjs` (start-line
  parsing, `insideRoot`'s decision on two real paths; realpath stays in Node). Its own
  WebAssembly module, `sandbox-bridge.wasm` (`tools/build-sandbox-bridge-wasm.sh [out]`,
  reproducible, no imports), because it ships in the code-sandbox image rather than the web one;
  noevia-core pins its sha256 in `code-sandbox/sandbox-bridge.lock` and loads it under
  `SANDBOX_BRIDGE_IMPL=rust`. Framing state lives in the module (one handle per stream). Table
  from noevia-core (`node tools/gen-sandbox-bridge-fixtures.cjs`), copied byte-for-byte to
  `crates/sandbox-bridge/tests/fixtures/sandbox-bridge.v1.json`.
- `crates/mcp-frame` (sbstndalton/noevia#980): MCP response framing, a port of noevia-core's
  `server/mcp.cjs` `parseRpcBody` (plain JSON or `text/event-stream` bodies, id matching, server
  requests and notifications refused as replies) and `resolveSchemaRefs` (local `$ref` inlining
  under the same depth, node and character budget, JS corner cases included). Text crosses as
  UTF-16 units; the JSON scanner accepts exactly what `JSON.parse` accepts, iteratively. In
  `dav-parse.wasm` as `mcp_rpc_body` and `mcp_schema_refs` (raw replies: tag byte + UTF-8 JSON).
  Table from noevia-core (`node tools/gen-mcp-fixtures.cjs`), copied byte-for-byte to
  `crates/mcp-frame/tests/fixtures/mcp-frame.v1.json`.
- `crates/chat-template-caps` + `crates/provider-error` (sbstndalton/noevia#1002, #1003): a
  lexical capability scan of a model's Jinja chat template (tools, tool calls, tool role, system
  role, strict role alternation, `raise_exception`, thinking switch) and the decision noevia-core
  makes from it (`sendTools`: native tools, or a template that never raises); a classifier for
  upstream provider errors (context_full / template_or_tools_unsupported / bad_request /
  backend_down / other) with a capped, redacted reason; and autotune's serving verdict. In
  `dav-parse.wasm` as `template_caps`, `provider_error` and `serving_verdict`. Table from
  noevia-core (`node tools/gen-chat-template-caps-fixtures.cjs`, public chat templates with
  their sources, adversarial ones, and the JS context test's own answers), copied byte-for-byte
  to `crates/chat-template-caps/tests/fixtures/chat-template-caps.v1.json`.
- `crates/autotune-plan` (sbstndalton/noevia#1003): the deterministic step planner behind
  native model auto-tune. From a model's GGUF facts, the inference memory budget (less the
  services reserve and the floor, bounded by `MemAvailable`) and every result so far, it returns
  the one next step: the largest context that fits first, then the most precise KV cache type
  for it, fill-and-recall probes (memory failure: a more compact type at the same context;
  recall, time or quality failure: bisect down), the sampling/drafting/batch phases once each,
  one final fill check and the serving check, never planning a measured step twice. Integer
  byte arithmetic mirroring noevia-core's `kvCacheBytes` estimate. In `dav-parse.wasm` as
  `autotune_plan`. Table from noevia-core (`node tools/gen-autotune-plan-fixtures.cjs`, seeded
  simulated runs over synthetic model layouts, expectations from an independent JS reference),
  copied byte-for-byte to `crates/autotune-plan/tests/fixtures/autotune-plan.v1.json`.
- `crates/load-verdict` (sbstndalton/noevia#1004): why a failed auto-tune step failed, in the
  planner's words (`oom`, `load_failed`, `timeout`, `over_time`, `recall_failed`, `template`).
  A measured cause (memory floor, time limit, recall) stands. For a guessed one (refused load,
  failed router row, stream error, engine gone) a fixed, ordered rule list reads the engine's
  evidence (exit code 137, out-of-memory, chat-template, context, time-out and model-file
  patterns); only when no rule matches may an advisory label from the decision service (Laya)
  decide, at a confidence of at least 0.6 and never as `timeout`; a crash (`crash: true`) never
  reads as a time out (noevia#1046); otherwise the calibrator's own cause stands. Fixed
  reasons, never echoing the engine's text. In `dav-parse.wasm` as `load_verdict`. Table from
  noevia-core (`node tools/gen-load-verdict-fixtures.cjs`, synthetic engine messages, seeded
  combinations, expectations from an independent JS reference), copied byte-for-byte to
  `crates/load-verdict/tests/fixtures/load-verdict.v1.json`.
- `crates/tune-contention` (sbstndalton/noevia#1062): while auto-tune runs, may it go on when
  another client of the llama.cpp router has a model live? Foreign rows (not the tuned model,
  not `unloaded`/`failed`) that are loading, in an unrecognised state or resident with requests
  in flight (`llamacpp:requests_processing`) make it wait; when every foreign model is resident
  and idle and the foreign set has looked the same for the quiet window (twice that when a
  request count could not be read), it may send the router's own unload; past the wait limit it
  gives up (the tune stops as interrupted, resumable). Stateless: the caller hands back the
  previous reply's fingerprint and since. Never asks to stop a busy or loading model. In
  `dav-parse.wasm` as `tune_contention`. Table from noevia-core
  (`node tools/gen-tune-contention-fixtures.cjs`, synthetic model ids, seeded combinations,
  expectations from an independent JS reference), copied byte-for-byte to
  `crates/tune-contention/tests/fixtures/tune-contention.v1.json`.
- `crates/long-profile` (sbstndalton/noevia#1079): low- and high-context profiles per model. A
  Long auto-tune writes `[<model>-long]`, which loads the same weights with its own context, KV
  cache and batch values. Three decisions: which router rows pair as a model and its long
  profile (the `-long` id and the same model file, never a name alone); the models.ini text with
  that section appended (the base section's lines without `load-on-startup` and `alias`, the
  router's model and projector files when the base has none, and every existing section left
  as preset-reload needs it to reload without unloading anything); and which entry serves a
  chat with Context Low or High. In `dav-parse.wasm` as `long_profile`. Table from noevia-core
  (`node tools/gen-long-profile-fixtures.cjs`, synthetic ids and paths, expectations from an
  independent JS reference), copied byte-for-byte to
  `crates/long-profile/tests/fixtures/long-profile.v1.json`.
- `crates/ssrf-policy` (sbstndalton/noevia#795, #930): the decision core of noevia-core's
  outbound-URL guard (ssrf.cjs `isPublicUrl`/`isPrivateIp`, public-fetch.cjs). Is this URL
  acceptable (WHATWG parse, so decimal/octal/hex IPv4, userinfo and backslash tricks resolve to
  the host Node connects to), and is every resolved address public; DNS and the socket stay in
  JS, and the loader refuses unless the host matches `new URL().hostname`. Addresses via
  `egress::is_private_ip`. Stricter than the JS: trailing-dot names, internationalized (`xn--`)
  hosts, and the metadata name under public fetch. In `dav-parse.wasm` as `ssrf_policy`. Table
  from noevia-core (`node tools/gen-ssrf-fixtures.cjs`, expectations from the JS itself with the
  network stubbed), copied byte-for-byte to `crates/ssrf-policy/tests/fixtures/ssrf.v1.json`.
- `crates/stream-guard` (sbstndalton/noevia#516, #704): noevia-core's stream-guard.cjs, the
  incremental JSON validator over the restricted schema subset (type, required, properties,
  additionalProperties: false, enum, items, maxItems, maxLength) and its bounded correction
  request. Same first violation (message and path, unit for unit, after the same chunk) as the
  JS over UTF-16 code units, lone surrogates, split pairs and Node's `/\s/` included; maxBytes
  counts each chunk's `Buffer.byteLength` as the JS does. The state between chunks is bytes the
  host holds (bounded, checked on decode). No dependencies. Stricter than the JS, as refusals:
  schema shapes outside plain JSON subset use, maxBytes over 2 MiB, maxDepth over 1024,
  non-integer options. In `dav-parse.wasm` as `stream_guard` (STREAM_GUARD_IMPL). Table from
  noevia-core (`node tools/gen-stream-guard-fixtures.cjs`, synthetic texts and the real caller
  schemas, expectations from the JS itself), copied byte-for-byte to
  `crates/stream-guard/tests/fixtures/stream-guard.v1.json`.
- `crates/prompt-framing` (sbstndalton/noevia#769, #740): noevia-core's prompt-injection
  boundary, byte-identical to the JS over UTF-16 code units (lone surrogates included):
  `frameUntrusted`/`escapeClosing` (prompt-framing.cjs: the `<untrusted kind label>` block and
  its defused closing markers), the tool-layer provenance policy (provenance-policy.cjs: framed
  block parsing, NFKC/lowercase normalisation, 16-unit FNV-1a grams, sensitive key stems, the
  candidate forms of a value with WHATWG URL hosts and Node's `domainToUnicode`, `checkWrite`)
  and task packet schema 1 (task-packet.cjs: strict parse/validate with the JS's `path: rule`
  errors, render). The taint store is plain data the host hands back each call. In
  `dav-parse.wasm` as `frame_untrusted`, `escape_closing`, `provenance` and `task_packet`
  (PROMPT_FRAMING_IMPL). Table from noevia-core (`node tools/gen-prompt-framing-fixtures.cjs`
  on Node 22, the shipped runtime; synthetic text, seeded), copied byte-for-byte to
  `crates/prompt-framing/tests/fixtures/prompt-framing.v1.json`. Node 22's ada maps U+1E9E to
  "ss" (pre-15.1 UTS46); the crate does the same for hosts (see `idna_compat`).
- `model-files backups` (`crates/model-files/src/backups.rs`, sbstndalton/noevia#1021): which
  recovery copies of models.ini a model-manager write makes and which old ones it removes. The
  rotating `<file>.bak-*` copies keep ini.py's rule (newest 10 by name); the
  `<file>.noevia-backup-<revision>` copies, never removed before, keep the newest
  `keepRevisions` (default 10) by mtime, this write's copy always among them; a write with
  noevia-core's `backup: false` hint (#1003, one copy per auto-tune run) makes neither. Only
  listed names of the two patterns are ever returned. Table from `python3
  tools/gen-model-backups.py` (an independent reference of the specification), copied
  byte-for-byte to noevia-services `model-manager/tests/fixtures/model-backups.v1.json`.
- `crates/docx-text` + `bins/docx-text` (sbstndalton/noevia#981): the OCR service's DOCX
  text front. `docx-text extract < upload.docx` checks the ZIP container (EOCD, member
  count/sizes, encryption, compression ratio) and prints `{"text", "truncated", "scope"}`
  exactly as noevia-services' `ocr/docx_text.py` `extract_docx` does; refusals print
  `docx-text: refused: <class>` and exit 1. Same limits as Python (1000 members, 64 MiB,
  8 MiB XML, 200k characters, 25 MiB input), a strict streaming XML reader with no DTD or
  entities, and a ZIP reader stricter than `zipfile` (the crate docs list each difference).
  The differential corpus is regenerated with
  `uv run --python 3.12 tools/gen-docx-text.py --ocr <noevia-services>/ocr --bin target/release/docx-text`;
  copy `crates/docx-text/tests/fixtures/docx-text.v1.json` into noevia-services'
  `ocr/tests/fixtures/` when it changes. Producer compatibility (python-docx, pandoc,
  LibreOffice headless, macOS textutil, Java ZipOutputStream and Info-ZIP/ditto/zipfile re-packs,
  synthetic content) is
  `crates/docx-text/tests/fixtures/docx-producers.v1.json`, regenerated on a Mac with
  `uv run --python 3.12 --with python-docx tools/gen-docx-producers.py --ocr <noevia-services>/ocr --bin target/release/docx-text`
  (copied into noevia-services the same way).

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The toolchain is pinned in `rust-toolchain.toml`.
