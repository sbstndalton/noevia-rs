# Native draft-head read (noevia#1318 slice 12)

`native-draft-heads` serves `GET /api/v1/sections/{name}/draft-heads`, and it is the only path; the former `MODEL_MANAGER_DRAFT_HEADS_IMPL` selector is retired and ignored. The confirmed child (`NOEVIA_NATIVE_MODEL_MANAGER_DRAFT_HEADS_CONFIRMED=1`) refuses that GET with 503, so the read is answered by one process. The child gets the same `MODELS_DIR`, `MODELS_INI_PATH` and `DATA_DIR`. A case-folded spelling of any of them keeps Python serving the route, with one log line.

Only GET with a name segment that does not decode to `/` is native. Other methods get the front's 405, as FastAPI answers. A name with `%2F` is forwarded and gets Python's 404. The draft-head download (`POST .../draft-heads/download`) follows the native download table (`native-downloads`). The route keeps a forwarded route's time budget, because it waits on the Hub.

## Behaviour (`api.section_draft_heads`)
- **The section's file (`helpers._resolve_section_gguf`):**
  - The section's `model =` value is used when it names a file. Defaults from `[DEFAULT]` count, as `cp.items` gives them.
  - Otherwise the stem of a file found by the models walk is used.
  - With `model =` set and that file present, it is the model file.
  - Otherwise, for a file in a folder, the first `.gguf` in that folder in name order is used, preferring files without `mmproj` in the name.
  - No file answers 404 `no model file for this section`.
- **Local heads (`autoconfig._find_mtp`):**
  - For a model in a folder: the smallest head in `<folder>/MTP`, `<folder>/mtp`, then the folder itself.
  - For a flat model: the smallest top-level head whose name carries the model's name.
  - The name rules are `model_autoconfig::files` (the same crate as the autoconfig check). The listing is `_listing`: names in order, with each entry's kind and size taken after following links.
  - A flat head that cannot be sized is Python's unhandled `OSError` (500).
- **Built-in layers:** `nextn_predict_layers` of the model file's summary (the `gguf` crate). A file that cannot be opened or read, or a header the parser rejects, gives 0, as Python catches them.
- **Repository (`db.repo_for_file`):** the same SQL on `DATA_DIR/model_loader.db`. A store that cannot be read is a 500.
- **Hub (`hf.repo_detail`):**
  - The tree listing is read page by page, up to 40 pages, following `Link: <...>; rel="next"`.
  - The stored token is sent as `Authorization: Bearer ...`. A token httpx or h11 would refuse makes the lookup fail.
  - Any non-2xx answer, a body `json.loads` refuses, or a `model-files` refusal makes the lookup fail, which gives `remoteError`. A page that is not a list ends the paging.
  - The entries are projected and serialised exactly as `model_files._project` and `json.dumps` do, then handed to the `model-files` crate. So the size cap, JSON caps and errors are the binary's own.
  - The heads are `.gguf` files without `mmproj` in the name that pass `_looks_like_draft` and are 1 byte to 2 GiB. They are sorted by size, stably after the listing's own order.
  - With no remote heads, no built-in layers and a `-GGUF` (not `-MTP-GGUF`) repository, the `-MTP-GGUF` sibling is asked for. If that lookup succeeds, it becomes `mtpBuild`.
- **Answer:** the keys in Python's order, with `remoteError` last when the first lookup failed.

## Decisions (technical)
- **Next links stay on the Hub's origin:** Python follows any `next` link and sends the token with it. Natively, a `next` link to another scheme, host or port is a failed lookup (`remoteError`), so the token never leaves the configured Hub origin.
- **Hub-shaped ids only:** a repository id with a segment that is empty, `.`, `..`, or not letters, digits and `._-` is a failed lookup. Python would send it (the update check does the same).
- **Bounds:** a page over 16 MiB, JSON nested deeper than 512 levels, a page of more than 262,144 values and keys, or a string longer than 65,536 characters is a failed lookup (noevia#1985). Python reads any size and recurses until `RecursionError`, which also fails the lookup. Real Hub pages are well under 1 MiB and a few thousand entries.
- **Memory (noevia#1973):** each page is projected as it arrives (only `type`, `path`, `size` and `lfs.size` are kept, as `model_files._project` does), and the lookup fails as soon as the projected listing passes the 16 MiB input cap of `model-files`, without asking for the next page. Python keeps every page and checks the cap once at the end; the answer is the same because the size only grows. At most 2 Hub lookups run at once; later requests wait. A lookup is held until its final parse has returned, and that parse (like every blocking step of the route) is registered with the request's admission, so a dropped request (client gone, route timeout) gives back neither the lookup nor its admission permit while the parse is still running (noevia#1998). Each entry's projected size is measured before it is appended, so one entry cannot take the listing past the cap. The projected listing is also held to 100,000 entries (the binary's own cap, so the answer is the same) and 262,144 values and keys (about 23,800 file entries; Python would read up to 100,000), counted as the binary's parser counts them, and its final parse has the same node limit (noevia#1995). Memory per lookup, by buffer capacities (noevia#1985, #1995):
  - while a page is read and parsed: the page (16 MiB), its tree (at most 68 MiB, asserted for the worst shapes: bare numbers, object keys, long strings, nested single-element arrays) and the listing projected from the earlier pages (16 MiB): at most 100 MiB;
  - then that tree and the listing (at most 84 MiB);
  - at the end, the listing text (16 MiB) while it is parsed into a tree of at most 68 MiB (the same caps): at most 84 MiB;
  - then that tree and the file list, once the text is dropped. The file list holds at most one file per 5 nodes and copies of their paths. It is largest for sharded paths just past a power of two, where the node cap and the input cap both bind: the parser's path string doubles, and `shard_key` reserves the shard base by characters and then doubles it for the extension. A sweep of path lengths peaked at 98.3 MiB (288-character sharded paths, 52,428 files); the test holds this stage under 100 MiB. There is no tight analytic bound for this stage, and the shared `model-files` crate is left as it is.

  So one lookup holds about 100 MiB at most, and the two concurrent lookups about 200 MiB. Before, a hostile 64 MiB page could take 1-2 GiB, and 21 pages of kept numbers about 370 MiB at the final parse. A UTF-16 or UTF-32 page is decoded and its raw bytes are dropped before the parse; the decoded text is shrunk to its length first. A UTF-8 page is parsed where it lies. The projection is written straight from the page without building a copy.
- **Refusals:** some conditions answer the native 503 before any Hub request, because the kernel-contained reader cannot follow them exactly:
  - a link or symlinked folder leaving the models folder;
  - a `..` in a `model =` path;
  - a GGUF header past the reader's bound;
  - a summary error whose Python class (caught `ValueError` or unhandled `TypeError`) is unknown;
  - a `repo_id` stored as a blob.

## Differential evidence
`tools/gen-draft-heads-oracle.py` runs the actual services app (pinned `1c966cd`) on synthetic cases: flat and folder models, `MTP` folders, heads beside the model, oversized and odd entries, links, sharded models, explicit and `[DEFAULT]` `model =` values, duplicate sections, built-in layers of several GGUF value types, unreadable headers, odd names, the download history's `LIKE` and status rules, two-page listings with a token, the `-MTP-GGUF` sibling, and Hub failures (500, not JSON, not a list, a refused listing, a missing next page).

The Hub is a local fake. For every request it records the answer and the Hub requests (path, query and token). The lib test replays every case on every platform. On Linux the integration test replays them through the router with the kernel-contained opener. CI regenerates the fixture and compares it byte for byte.
