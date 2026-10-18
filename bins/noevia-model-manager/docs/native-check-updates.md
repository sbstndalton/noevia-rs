# Native model update check (noevia#1318)

`native-check-updates` with `MODEL_MANAGER_CHECK_UPDATES_IMPL=rust` (and `MODEL_MANAGER_IMPL=rust`) serves `POST /api/v1/models/check-updates`. Default `python`. The confirmed child (`NOEVIA_NATIVE_MODEL_MANAGER_CHECK_UPDATES_CONFIRMED=1`) refuses that POST with 503. It gets the same `MODELS_DIR`, `MODELS_INI_PATH` and `DATA_DIR`, with canonical spelling required, as for the native update status.

## What it does (`api.check_updates`, `hf.check_updates_for`, `hf.repo_file_mtimes`)
- **Records:** `db.download_records()` reads the latest `done` record per `(filename, repo_id)`, grouped by repo in first-appearance order (Python dict-key equality).
- **No `/` in the repo id** (a URL import): each file gets an empty check, with no request.
- **Any other repo id:** one `GET {HF_API}/models/{repo}/tree/main?recursive=true&expand=true` is sent. It carries `Authorization: Bearer {token}` when the stored `hf_token` is truthy.
  - The header value is built as Python builds it, including `repr()` of a BLOB token.
  - A non-ASCII value raises (500), as httpx does when it encodes headers.
  - A value h11 refuses (leading or trailing whitespace, a control line break, NUL) is a failed request with no dates.
- **Tree answers:**
  - A 200 answer is decoded with `json.loads(bytes)`'s encoding detection and CPython's JSON grammar (NaN, Infinity, big ints).
  - Shapes the loop raises on answer 500: a non-empty object or string, a number, a non-object entry, a truthy non-object `lastCommit`, or a list/dict `path`.
  - A failed request, a non-200 status or a body `json.loads` refuses gives no dates.
- **Writes:** each file's `lastCommit.date` is written as `set_update_check` writes it, with one `time.time()` for the whole check.
  - The value is bound when its row is written: a list, a dict or an int past 64 bits raises then, after earlier rows were written. NaN is stored as NULL.
  - A later duplicate path wins.
- **Answer:** `{"checked": len(records), "status": …}`. The status is the native `GET /api/v1/models/updates` answer, read after the writes. On that read's own 503 refusal the writes are already made.

## Decisions (technical)
- **Malformed repo ids:** a repo id that isn't Hub-shaped (segments of letters, digits and `._-`, no empty, `.` or `..` segment) is not sent to the Hub and gets no dates. Python would send it, and the Hub does not list such repos.
- **Header bytes:** h11 accepts the bytes 0x01–0x08, 0x0e–0x1f and 0x7f in a header value, but the HTTP library here does not. A token containing them is treated as a failed request.
- **Bounds:**
  - **Tree listing (noevia#1996):** 16 MiB, at most 262,144 JSON values and keys, and strings of at most 65,536 characters. A larger page means no dates, as for a failed request; Python reads any size. Real Hub pages are well under 1 MiB. The read buffer never grows past 16 MiB. A UTF-16 or UTF-32 page is decoded and its raw bytes are dropped before the parse; a UTF-8 page is parsed where it lies. These limits are one module, `hub_page`, shared with the draft-head lookup (noevia#2000). So while a page is parsed, one check holds at most the page and its parsed tree, at most 84 MiB by buffer capacities (a hostile 64 MiB page of bare numbers was about 2 GiB). Once parsed, the raw page is gone, but `entries()` builds its output (a path and a date copied for each file entry, in a vector that doubles) while the whole tree is still held. Tree and output together reach about 88 MiB for the worst page built, by buffer capacities: a date of nested arrays (nearly all of the node cap) beside long paths and dates (one character past a power of two, which the parse doubles in the tree and the output copies exactly). For every page the limits admit, tree and output together stay under 110.7 MiB (noevia#2013). That bound is derived from the limits, not measured: the tree is at most 68 MiB (per node, a slot in its parent with the parent's doubling slack and the node's own smallest allocation; every string byte twice, at most the page's bytes), and the output copies disjoint subtrees of that tree to their exact lengths (per node a slot and an allocation, every string byte once) plus one pair per file entry (at least 9 nodes each) with the vector's previous buffer while it grows. The test builds seven page shapes and checks each against it and against 90 MiB; the worst of them measured 88.3 MiB, so the derived bound is not tight, and it shows the 84 MiB of the parse is not the whole peak of a check. Real pages are far smaller (noevia#1999): the Hub pages an `expand=true` listing by 50 entries, an entry is 17 to 71 nodes for a GGUF file (50 on average) and up to about 300 for a pickle file whose `securityFileStatus` lists 33 imports (7 nodes each), the busiest real page measured 4,440 nodes (under 2 % of the cap) and the longest string was 168 characters. The pages are fixtures in `tests/fixtures/hub-tree`. Within a check, repositories are looked up one after another; concurrent checks are bounded only by the front's 4 request permits. Every blocking step (store reads and writes, tree parses) is registered with the request's admission, so a dropped check (client gone, route timeout) keeps its permit until that step has returned and the bound also holds for checks whose client went away (noevia#1998). A page of more than 262,144 values whose items are not objects (for example `[1, 1, …]`) also means no dates; Python would answer 500 for it.
  - **JSON nesting:** depth 512 (deeper means no dates; CPython parses until RecursionError).
  - **Records:** 4096 (more answers 500).
  - **Timeouts:** connect and each read 15 s (httpx's `timeout=15.0`), and 120 s for a whole tree request.
  - **Route budget:** the forwarded-route budget (600 s), since Python's handler has none.
- **Filename matching:** only a str filename can match a tree path. Python would also match an int file name to an int path.

## Differential evidence
`tools/gen-check-updates-oracle.py` runs the actual services app (pinned `d475f42`) with a fake Hub on 127.0.0.1 and `time.time()` in `hf` pinned. Each case records the answer, every Hub request (path, query, Authorization) and the `update_check` table afterwards, as SQL literals.

The 26 cases cover tokens (none, empty, trailing space, newline, non-ASCII, BLOB), a BLOB repo id, Hub statuses, body encodings and every tree shape above. `tests/check_updates.rs` replays all three against the same fake Hub.

The status body is compared on Linux, where the kernel-contained models walk runs. Elsewhere a 200 case is that walk's 503, after identical requests and writes.
