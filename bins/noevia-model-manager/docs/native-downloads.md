# Native downloads (noevia#1318)

`native-downloads` with `MODEL_MANAGER_DOWNLOADS_IMPL=rust` (and `MODEL_MANAGER_IMPL=rust`) owns the download job table and its reads. The default is `python`. The confirmed child (`NOEVIA_NATIVE_MODEL_MANAGER_DOWNLOADS_CONFIRMED=1`) refuses all five routes below with 503, so only one process ever holds jobs. It gets the same `MODELS_DIR`, `DATA_DIR` and `MAX_CONCURRENT_DOWNLOADS` (1..=64, canonical spelling required) as the parent.

## Routes
- `GET /api/v1/downloads`: the job list, rendered as `api._job` renders it (speed, ETA, human sizes, chunk progress), byte for byte.
- `POST /api/v1/downloads` and `POST /api/v1/sections/{name}/draft-heads/download`: starts.
  - **Planning:** the start is planned by the Python child. It runs the same route with the private header `x-noevia-download-plan: 1`. Python then resolves the target, the token and the file list, stops in `_make_job` right after `safe_dest`, and returns `{status, body, jobs}`.
  - **Queueing:** the native side re-checks `safe_dest` for each planned file and queues the jobs. A child 500 is a 500, and an unreadable plan is a 502.
- `POST /api/v1/downloads/{id}/cancel` and `POST /api/v1/downloads/clear`: as in Python.
- **Overview:** `GET /api/v1/overview` still comes from the child. Its `activeDownloads` is replaced with this table's count.

## Transfer (`downloader.py`)
- **Requests:**
  - Redirects are followed by hand, at most 20 (the 21st request is `TooManyRedirects`).
  - The Hub token is sent only to the Hub origin, as httpx's auth does per hop.
- **Resume:** resume uses the `.download` temp file and its `.validator` (ETag, otherwise Last-Modified, via a port of `parsedate_to_datetime`).
  - A changed validator restarts the file.
  - A 200 answer to a range request restarts it.
- **Large files:** large files (`parallel_min` and up) with range support are split into parallel chunks, and each chunk is retried as in Python.
- **Progress:** progress moves in httpx's `aiter_bytes` 1 MiB steps.
- **Finishing:** the size is checked, the file is renamed into place, and a `download_history` row is written as `db.record_download` writes it.

## Decisions (technical)
- **TLS:** `native-check-updates` (and so this feature) builds reqwest with rustls and webpki roots, certificate verification on (noevia#1952).
- **All-or-nothing starts (noevia#1958):** every planned destination is resolved and the job cap is checked for the whole plan before any job is inserted. A start refused part-way (a later file outside the models folder, or the cap) leaves the table unchanged.
- **Paths (noevia#1959):** when a transfer starts, `safe_dest` runs again and the destination's folder is opened once and checked (by walking `..` from the opened handle) to lie inside the models folder. Every open, stat, rename and unlink of the destination, the `.download` file and its `.validator` then goes through that handle with `O_NOFOLLOW`. A planted symlink fails the job ("Too many levels of symbolic links") instead of being written through; Python follows it. A folder swapped for a symlink while the job waited fails with `ValueError: download destination is outside the models folder`. Missing folders are still created by path (`mkdir -p`) just before the check.
- **Redirects (noevia#1960):** a redirect from https to anything else is refused, and so is a redirect from a non-local host to a loopback, private (RFC 1918, CGNAT, ULA), link-local or unspecified address or `localhost`. Only literal addresses are recognised: a public name that resolves to a private address is not caught. Python follows both.
- **Body size (noevia#1960):** on the single-stream path nothing past the declared size is written. Up to 1 MiB more is counted so the error reads as `_verify_size`'s ("downloaded file is N bytes, expected M; ..."), beyond that the transfer stops ("downloaded file is over N bytes"). A body with no declared size is abandoned after 64 GiB (`UNKNOWN_SIZE_CAP`; Hugging Face refuses files over 50 GB). The parallel path already refused bytes beyond each range.
- **Bounds:**
  - **Jobs:** 4096 jobs. A start beyond that answers 503 "too many downloads" (Python grows without bound).
  - **Plans:** 8 MiB per plan, 1024 files per plan.
  - **Error bodies:** 4 KiB read from an error answer.
- **Error texts:** OS and httpx error texts are close to Python's, not exact. For a cut-short body the text is httpx's ("peer closed connection without sending complete message body (received N bytes, expected M)").
- **Dates:** an HTTP date that overflows is treated as no date.
- **Cancel during start:** a cancel that arrives while a job is starting is kept. Python loses it.

## Differential evidence
`tools/gen-downloads-oracle.py` runs the actual services app (pinned `bf552c1`) against fake Hub and CDN servers on 127.0.0.1. Each of its 26 cases records the routes' answers, the requests every server saw, the files left in the models folder and the `download_history` rows. Timing-dependent progress is normalised.

It also records `_job` rendering, `_validator`/`_http_date` and `safe_dest` on crafted inputs. `tests/downloads.rs` replays every case against the same fakes with a fake planning child. CI regenerates the fixture and compares it byte for byte.
