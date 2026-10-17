# Native draft-head read (noevia#1318 slice 12)

`native-draft-heads` with `MODEL_MANAGER_DRAFT_HEADS_IMPL=rust` serves `GET /api/v1/sections/{name}/draft-heads`. The default is `python`. The confirmed child (`NOEVIA_NATIVE_MODEL_MANAGER_DRAFT_HEADS_CONFIRMED=1`) refuses that GET with 503, so the read is answered by one process. The child gets the same `MODELS_DIR`, `MODELS_INI_PATH` and `DATA_DIR`. A case-folded spelling of any of them keeps Python serving the route, with one log line.

Only GET with a name segment that does not decode to `/` is native. Other methods get the front's 405, as FastAPI answers. A name with `%2F` is forwarded and gets Python's 404. The draft-head download (`POST .../draft-heads/download`) follows `MODEL_MANAGER_DOWNLOADS_IMPL`. The route keeps a forwarded route's time budget, because it waits on the Hub.

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
- **Bounds:** a page over 64 MiB, or JSON nested deeper than 512 levels, is a failed lookup. Python reads any size and recurses until `RecursionError`, which also fails the lookup.
- **Memory (noevia#1973):** each page is projected as it arrives (only `type`, `path`, `size` and `lfs.size` are kept, as `model_files._project` does), and the lookup fails as soon as the projected listing passes the 16 MiB input cap of `model-files`, without asking for the next page. Python keeps every page and checks the cap once at the end; the answer is the same because the size only grows. At most 2 Hub lookups run at once; later requests wait. One lookup holds at most one page (64 MiB) plus the 16 MiB projected listing.
- **Refusals:** some conditions answer the native 503 before any Hub request, because the kernel-contained reader cannot follow them exactly:
  - a link or symlinked folder leaving the models folder;
  - a `..` in a `model =` path;
  - a GGUF header past the reader's bound;
  - a summary error whose Python class (caught `ValueError` or unhandled `TypeError`) is unknown;
  - a `repo_id` stored as a blob.

## Differential evidence
`tools/gen-draft-heads-oracle.py` runs the actual services app (pinned `1c966cd`) on synthetic cases: flat and folder models, `MTP` folders, heads beside the model, oversized and odd entries, links, sharded models, explicit and `[DEFAULT]` `model =` values, duplicate sections, built-in layers of several GGUF value types, unreadable headers, odd names, the download history's `LIKE` and status rules, two-page listings with a token, the `-MTP-GGUF` sibling, and Hub failures (500, not JSON, not a list, a refused listing, a missing next page).

The Hub is a local fake. For every request it records the answer and the Hub requests (path, query and token). The lib test replays every case on every platform. On Linux the integration test replays them through the router with the kernel-contained opener. CI regenerates the fixture and compares it byte for byte.
