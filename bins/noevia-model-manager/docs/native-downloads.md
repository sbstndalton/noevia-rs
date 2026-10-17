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
- **TLS:** this feature builds reqwest with rustls and webpki roots. Native check-updates has no TLS backend on its own (noevia#1952).
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
