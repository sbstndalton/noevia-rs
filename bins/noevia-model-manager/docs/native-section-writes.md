# Native models.ini writers (noevia#1318)

`native-section-writes` with `MODEL_MANAGER_SECTION_WRITES_IMPL=rust` (and `MODEL_MANAGER_IMPL=rust`) makes the Rust front the only models.ini writer. Default `python`. It covers five routes:

- `PUT /api/v1/sections/{name}`
- `PUT /api/v1/models-ini`
- `POST /api/v1/sections/{name}/rename`
- `DELETE /api/v1/sections/{name}`
- `POST /api/v1/sections/{name}/safe-defaults`

## Why all five at once
Python's `ini.WRITE_LOCK` only serialises threads inside the Python process. Splitting the writers across two processes would let a compare-and-swap check in one race a write in the other. So the switch moves every models.ini writer together. One in-process lock serialises them here, and the store work runs on a blocking thread that owns the lock guard (the #1909 rule), so a dropped request still finishes before the next write starts. That store work is also registered with the request's admission, so the request's permit comes back only when it has returned (noevia#2006). The first write waits for the child's `/api/v1/health`, because its startup may still be migrating cache-ram values in models.ini.

## A dropped request and jobs with no deadline (noevia#2006, #2015)
The write itself (the store work above) and the idle-unload reconcile after it run on blocking tasks registered with the request's admission. The `PUT /api/v1/idle-unload` setting's store write and its reconcile do too. The reconcile has its own lock, and its guard also moves into each job, so a dropped request keeps that lock until its job has returned and a second write cannot rewrite the same preset file at the same time. None of these jobs has a deadline, and a filesystem call that has started cannot be interrupted. A request that is dropped or that runs past its budget keeps its permit (one of the front's 4) until its job has returned, and a request that timed out sends its 504 only then. On a hung mount neither the 504 nor the permit comes back until the call returns, and there is no bound on that. After four such requests, every admitted route answers 503 `service is busy`.

## Behaviour (services `api.py` + `ini.py`)
- **Validation:** the same order and messages as Python. FastAPI's 422s come first, then the handlers' 400s (name, `baseRevision`, single-line values, extra lines that open a section), then the revision compare-and-swap (409), duplicate sections (409 with Python's message), 404s, and a 413 above the 1 MiB editor limit.
- **Writes:**
  - `_upsert_section` sets the known form fields in schema order, then the extras.
  - `_bound_cache_ram` folds the `cram`/`LLAMA_ARG_CACHE_RAM` aliases into `cache-ram`, clamps to `LLAMACPP_CACHE_RAM_HARD_MAX_MIB`, gives embedding/rerank sections 0 and other sections the `LLAMACPP_AUTOCONFIG_CACHE_RAM_MAX_MIB` cap unless a bounded `[*]` value is inherited.
  - Edits are made in place with `_render_preserving`, falling back to configparser's full render. A rename edits only the header line where `_rename_header` can.
  - `models.roles.json` drops a deleted section's role and moves a renamed one's.
- **Files:**
  - Python reads models.ini with universal newlines, and edits write `\n`. The raw editor writes its text verbatim.
  - Backups always follow the `model-files backups` plan (rotating `.bak-<UTC stamp>[-n]` copy plus the revision copy, keeping 10 of each). `MODEL_FILES_IMPL` is retired (noevia#1957): a leftover value is ignored with one warning.
  - The replace is a fsynced temp file, renamed over models.ini, then the directory is fsynced.
- **Answer:** `{"ok": true, "revision", ["section"], ["mtp"]}`, where `section` is `get_section` (DEFAULT keys merged). After a successful write the idle-unload reconcile (#1747) runs and the revision reported is that of the final file, as for the forwarded writers.

## Safe defaults
These need the GGUF hints, the draft-head search beside the weights and the built-in MTP layer count. The child computes all three read-only through the private `GET /api/v1/internal/safe-defaults-plan?name=` (`api._safe_defaults_plan`, split out of `_register_safe_defaults` without changing it). This side asks for the plan while holding the write lock, then writes the section. A refusal comes back as `{status, detail}` and is answered as is. A child that cannot answer gives 503 `Native models.ini writes are unavailable`.

## Fixed on the way
- noevia#1921: a rename that cannot edit the header in place copied the `[DEFAULT]` keys into the renamed section. It now copies the section's own keys only, on both sides.

## Decisions (technical)
- **TZ:** the native writer needs `TZ` unset or `UTC`, because rotating backups are named by local time. Any other zone, case-folded setting names, a non-normal `MODELS_INI_PATH` or non-integer cache-ram limits keep Python writing (all five routes), with one log line. The child then gets `TZ=UTC`.
- **Routes:** a name segment is unquoted as uvicorn does. A segment that decodes to `/`, a trailing slash and other methods stay with Python (404/405).
- **Body size:** the raw editor reads bodies up to 8 MiB. A body over 1 MiB answers 413 `models.ini exceeds the editor limit` before it is parsed; Python would first answer a malformed body that large with 422. Other writers keep the 1 MiB body cap.
- **Bounds:** models.ini reads are capped at 8 MiB (500 above).
- **Known gaps (approximate, not byte-tested):** cache-ram values with non-ASCII digits are treated as text. A 422 body's key order follows the shared validator (see noevia#1908).

## Differential evidence
`tools/gen-section-writes-oracle.py` runs the actual services app (pinned `ff8f314`, services#67) step by step on synthetic models folders. `time.strftime` is pinned to one UTC instant, and cases with `impl` `rust` use the real `model-files` binary; the native tests ignore `impl` because the plan is the only backup path.

It records every answer, the plan the child gives for each safe-defaults step, and the final models.ini, roles file and directory listing. The cases cover new files, preserving edits, every error path, cache-ram rules, a `[DEFAULT]` section, duplicates, unparseable files, odd headers (full render), CRLF and missing final newlines, the raw editor (backups, `backup: false`, size, parse errors), Rust and Python backup policies with pruning, rename (roles, fallback, header comments), delete (roles, trailing blanks, a middle section) and safe defaults (flat file, folder with a draft head, refusals).

`tests/section_writes.rs` replays every case natively against a fake child and compares answers and files byte for byte; 422 bodies compare as JSON. Unit tests and proptests cover the configparser emulation and renderers on hostile text.
