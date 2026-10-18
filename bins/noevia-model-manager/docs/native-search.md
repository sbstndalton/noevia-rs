# Native search reads (noevia#1318 slice 14)

`native-search` with `MODEL_MANAGER_SEARCH_IMPL=rust` serves `GET /api/v1/search` and `GET /api/v1/search/repo`. The default is `python`. One selector owns both routes: they share the Hub client, the token, the listing cache and the ranking rules. The confirmed child (`NOEVIA_NATIVE_MODEL_MANAGER_SEARCH_CONFIRMED=1`) refuses both GETs with 503, so each read is answered by one process. The child gets the same `MODELS_DIR`, `MODELS_INI_PATH` and `DATA_DIR`. A case-folded spelling of any of them keeps Python serving the routes, with one log line.

Only GET on the exact paths is native. The path is judged as Starlette routes it (percent-decoded once), so `/api/v1/%73earch` is native and `/api/v1/search/` is not. Other methods get the front's 405 and the slash variants its 404, as without the native search. The routes keep a forwarded route's time budget (600 s), because they wait on the Hub.

## What runs where

| Part | Python (`api.py`, `hf.py`, `discover.py`) | Native |
|---|---|---|
| Query validation and its 422 | FastAPI / pydantic | `search/params.rs` |
| Hub model search, the second unfiltered pass | `hf.search_models` | `search.rs` |
| Tree listings, 1 h cache, 40 candidates | `hf.repo_detail`, `api._repo_files_cached` | `search/tree.rs` over the draft-head reader, `search.rs` |
| Judging, filters, ranking, `hubUrl` | `discover.py` | `search/discover.rs` |
| Owner avatars: cache and fetch | `hf.owner_avatars`, `db.get_avatar`, `db.set_avatar` | `search.rs`, `search/store.rs` |
| `trusted_quantisers` setting | `api._trusted_quantisers` | `search.rs` |
| Remote header (first MiB) and its caches | `hf.gguf_header` | `search.rs` |
| File groups of one repository, sizes | `api.search_repo`, `utils.human_bytes` | `search.rs`, `search/py.rs` |
| **GPU budget, downloaded files, trusted sets** | `helpers._backend_list`, `helpers._downloaded_and_still_present`, `discover.TRUSTED_*` | **asked of the child** |
| **Fit chips, context estimates, native context** | `services.vram_fit_chips`, `helpers._preset_estimates_with_reason` | **asked of the child** |

The live state stays with Python until the backend sampler, the downloads and the autoconfig move (the remaining slices of noevia#1318). It is asked through two private calls, both 404 unless confirmed, both behind the token, neither ever forwarded for a client:

- `GET /api/v1/internal/search-runtime`: `{"budget": repr(float), "downloaded": {repo: [file]}, "quantisers": [...], "labs": [...]}`. Asked once per search, after the Hub answered. The budget travels as Python's `repr`, so a non-finite figure survives.
- `POST /api/v1/internal/search-repo-runtime`: for the file groups of one repository, `{"groups": [{"bytes", "estimate"}], "mmprojBytes", "summary"}` gives, per group, `fit` (`services.vram_fit_chips`) and, for a header summary and a non-projector GGUF group, `estimates`, `reason` and `nativeCtx`. Asked once, after the header was read, and not at all for a repository without groups. The summary is the one the `gguf` crate makes of the header (differential-tested against `gguf_meta.py`), with a non-finite float already `null`, as `gguf_meta._finite` has it. At most 4096 groups.

When the child cannot answer, the route answers 502 `Python-owned endpoint unavailable`, as any forwarded route does; its own 500 is Python's 500. The child's text is spliced into the answer unchanged (`fit`, `estimates`, `downloaded`), so the figures are the ones Python computes. The trusted publisher sets stay defined in Python (`discover.TRUSTED_QUANTISERS` and `TRUSTED_LABS`); the native side holds no list.

## Behaviour (`api.search`)
- **Query:** `q`, `sort`, `limit`, `trustedOnly`, `showUnsuitable`, `minGb`, `maxGb`, `quants`, `minParams`, `maxParams`, `moe`, `vision`, `license`, `owner`. The last value of a repeated key wins, a `+` is a space, text is UTF-8 with replacement. All errors are listed together in declaration order, the input echoed.
- **Hub search:** `GET {api}/models?limit=&sort=&direction=-1[&search=]&filter=gguf`, the sort one of `downloads likes lastModified trendingScore` (`fit` and `trending` ask for `trendingScore`, any other value for `downloads`), the limit `max(1, min(int(limit or 30), 60))`. Parameters are encoded as `urllib.parse.urlencode` does. With a query and no result, a second pass without the tag filter keeps the items that look like GGUF repositories (a tag starting with `gguf`, or `gguf` in the id), or all of them if none does. A status over 399 gives `{"error": "Hugging Face returned HTTP N.<hint> <first 200 characters of the stripped body>", "results": []}` (hints for 401, 403 and 429); a transport error `Could not reach Hugging Face...`. A body that is not JSON is Python's 500; a JSON value that is not a list is no result.
- **Items:** `id`/`modelId`, `downloads`, `likes` (Python's `int()` of the value, so a numeric string counts and a bad one is a 500), `lastModified` (`str()`), `tags` (a string gives its characters, a dict its keys, a number is a 500) and `siblings` (a non-list or a list with a non-dict is a 500) follow `hf._model_from_item`, for all items, not only the first 40.
- **Candidates:** the first 40. Each repository's `.gguf` files come from its tree listing (up to 40 pages of 50, following `Link: <...>; rel="next"`), cached 3600 s. The cache is looked at once for all candidates, as the concurrent coroutines do, so duplicate ids are listed once per duplicate. A failed listing is no files and is not cached.
- **Judging (`discover.py`):** quant, parameter and sharding rules by the same patterns, which are Unicode-aware in Python and spelled out here (`\w` is letters, numbers and `_`; `\d` any decimal digit, read by `float()` as Python reads it; `\s` what `str.isspace` has; `re.I` letters with the characters Python folds onto them; `\b` and the `$` before a final newline by hand), so CJK, accented, no-break-space and non-breaking-hyphen names are judged as Python judges them (the test compares each class with Python's over every character it assigns), the size in GB, the budget (`max(1.0, budget * 1.073 - 2.5)`, or `1e6` with no backend), the reasons, `best`, the score, `popularity` (age-weighted with a 90-day half-life, `round(x, 2)`), `ageDays`, the filters and the ranking (`score`, `popularity`, `id`). Floats are written as Python writes them; a non-finite one is `null` (the routes return `-> dict`, which FastAPI serialises through pydantic). The regex tables here are newer than Python's database (Unicode 15.0 in the image; the test asserts it): a letter, digit or space assigned after 15.0 is not one for Python, so a name holding one is refused (503) instead of judged, and the test checks every character the tables take for one against the list of those.
- **Avatars:** `avatar_cache` rows younger than 7 days are used; the others are fetched (organisation overview, then user overview, 8 s) and written with the request's time, an owner that has none cached as `""`. A failure anywhere gives no avatars for the response and writes nothing, as in Python. A name that is not Hub-shaped is not asked about and caches `""`.
- **Downloaded:** `downloaded` is the child's map entry for the repository.

## Behaviour (`api.search_repo`)
- The listing is grouped by shard set in listing order, each group's files sorted by `(shard index, path)`, imatrix and tiny GGUF groups dropped, the groups sorted GGUF first, then non-projectors, then by lower-cased base name. `size` is `utils.human_bytes`.
- The first GGUF non-projector group's first file is the probe: its first MiB is read with `Range: bytes=0-1048575` (200 and 206 are read; a body that ignores the range is cut at 1 MiB), redirects are followed (`Authorization` is dropped when the origin changes), 25 s. 401 and 403 remember the repository as gated (`gated` carries the sentence), any other failure or an unreadable header is remembered as no header; a success forgets the gate. 64 headers are cached, then the cache is cleared.
- Errors: a Hub status is `{"repo", "error": "Hugging Face returned HTTP N", "groups": []}` (a status over 299 that is not followed counts), a transport error `Network error: <text>` (the text httpx gives for a timeout, which is empty, a refused connection, a lookup failure, a closed connection and a short body; messages h11 builds from bytes it read are not reproduced), a body that is not JSON is Python's 500, and a listing `model-files` refuses is the 502 `{"detail": "The repository file list could not be checked, so it was not used. See the model manager log."}`.

## Deliberate differences
- **Refusal (503 `Native search is unavailable`) instead of a guess** for conditions this side cannot follow exactly:
  - a date (`lastModified`) outside printable ASCII, whose `datetime.fromisoformat` reading this side does not follow;
  - a query value pydantic reads in a way no grammar predicts (underscores in odd places, `0-1` as -1: `search/params.rs` lists what is certain, and the corpus test checks it against pydantic itself) or a value outside the characters any number can have, which is a 422 as Python's;
  - hub fields of other JSON types than Python's `int()`, `str()` or iteration give a result this side can print (a list or dict as `lastModified`, a nested list as a tag), numbers past 62 bits, a listing size past 62 bits;
  - a page past the `hub_page` bounds in the model search or in a repository listing (a listing inside a search is a failed lookup); a `next` link to another origin; a header summary or a path (a backslash) the HTTP client would send differently; a stored token h11 would refuse, in the repository route (Python's message carries the token);
  - more than 4096 file groups in one repository.
- **A repository id that is not Hub-shaped** (an empty segment, `.`, `..`, or a character outside letters, digits and `._-`) is not sent to the Hub: in a search it has no files, in the repository route it answers `Hugging Face returned HTTP 404`. Python sends it (httpx quotes it) and the Hub answers 404.
- **Concurrency:** at most 2 listings at once (the draft-head bound, noevia#1973, held separately from the draft-head lookup own two); Python runs the 40 at once. A cold search of 40 repositories therefore takes about 20 round trips of the Hub's latency; the cache keeps it for an hour. In a request that fails because a candidate's id is not a string, Python's other lookups may already be in flight; here none starts.
- **Cache bounds Python does not have:** the listing cache holds at most 32 MiB of paths (an entry over it is not kept, and it is cleared first when a new one would pass it); the header cache is cleared at 64 entries before any insert, failures and gates included (Python clears only for a success); remembered gated repositories are capped at 4096. The caches are also cleared when the stored token changes (a listing or a header may be private to a token); Python keeps them. A request keeps what it learnt only while the token it read is the current one: every token read has a ticket, taken under the same async lock as its sqlite read so that ticket order is read order (the lock is taken before the blocking job is spawned, so a read waiting for its turn holds no blocking slot, and a request dropped meanwhile leaves at most a gap in the numbers), the caches have a generation that moves, before they are cleared, whenever a newer read sees another token (a token the client cannot send, not ASCII or refused by h11, counts as one), a request stores into a cache only if the generation it read the token under is still current (checked under the cache's lock), and a read that ends after a newer one neither clears nor moves the remembered token back. The `gated` reason of a repository answer is the one the Hub's own answer carried, not read back from the cache, so a read that raced a token change (and stored nothing) still reports it.
- **Timeouts** are httpx's (connect and every read), plus 120 s for one request with its redirects, plus a deadline for the listing phase: 90 s for all the listings of a search together (a listing still running is a failed one: no files, not cached) and 90 s for the listing of one repository (`Network error: `, as a timeout has no text). Without it a degraded Hub could keep a cold search past the front's 600 s. A search logs one line with the number of its listings the deadline cut off, so that a repository whose listing was cut off is not mistaken for an empty one; a repository route logs the one. Every Hub-supplied name in the logs is printed with `{:?}`.

## Memory (noevia#1985)
Every page is read into a buffer that never passes 16 MiB and parsed under the `hub_page` node, string and depth limits; the model-search page is parsed the same way and its items are converted in place. A tree page is projected as it arrives, and the final parse holds the lookup permit, also when the request is dropped (noevia#1998). So one repository lookup holds about 100 MiB at most (the figures of `native-draft-heads.md`), two at once about 200 MiB, and a model-search page under 100 MiB. Avatar and error bodies are read up to 1 MiB. The listing cache is bounded as above.

## Differential evidence
`tools/gen-search-oracle.py` runs the actual services app (pinned in CI) on synthetic repositories against a fake Hub (and a fake CDN a redirect leads to) on 127.0.0.1, with the clock, the hardware, the backends and the trusted sets fixed, and records for each request the status, content type and body, the Hub requests (path and query, token, range), the avatar cache and what the native side asks of the child with the child's real answers (the real private endpoints of the same app in the same state). The cases cover the query (defaults, sorts, limits, every filter, invalid and repeated values), Hub statuses and bodies (with charsets and invalid bytes), timeouts, dropped and short responses, the second pass, hostile items, listing failures and paging, the listing cache and its age, avatars (every URL form, stale and fresh cache, failures), the token (text, number, bytes, non-ASCII, one h11 refuses), the trusted setting and the downloaded files, and for one repository: grouping, sorting, sizes, quoting of paths, every header outcome (ranges, gates, redirects to another origin, garbage, size), estimates and their reasons, and every listing outcome.

It also records Python's answers for the grammars: 17,000 query values for pydantic's int, float and bool, `str.strip`, `int(str)`, 3,500 dates through `datetime.fromisoformat`, and `round`, `.1f`, `human_bytes` and int true division. The lib test replays every case on every platform, the integration test replays them through the router, and `bounds.rs` holds what has no Python counterpart (refusals, bounds, caches, admission, the child's failures). CI regenerates the fixture and compares it byte for byte.

## What remains
- The two private calls keep this slice dependent on the Python child for live state. They go when the backend sampler (`MODEL_MANAGER_BACKENDS_IMPL`, already native but dark), the download history and the autoconfig (`analyze`) are native; the budget, the downloaded map, the chips and the estimates would then be computed here from the same figures.
- The trusted publisher sets are defined in `discover.py`; the native side reads them per request. They move to configuration when the child retires.
- Outside this repository: the services Dockerfile pin, and the compose passthrough `MODEL_MANAGER_SEARCH_IMPL: ${MODEL_MANAGER_SEARCH_IMPL:-python}` in the model-loader environment (noevia). Nothing is flipped by this change.

## Native search runtime (noevia#1318 step 1c)

While this process samples the backends (`MODEL_MANAGER_BACKENDS_IMPL=rust`), the body of
`GET /api/v1/internal/search-runtime` is produced natively (`search/live.rs`) and that call is
no longer made:

| Field | Python | Native |
| --- | --- | --- |
| `budget` | `max(vram_gb of helpers._backend_list())` | `fit::budget` over the native descriptors |
| `downloaded` | `helpers._downloaded_and_still_present()` | the `done` rows of `download_history`, newest first, kept while the file (or its last path part) is in the models-folder snapshot |
| `quantisers`, `labs` | `discover.TRUSTED_QUANTISERS`, `TRUSTED_LABS` | a deployment file, see below |

**Trusted publisher sets.** They are deployment data, not source. At startup the native side
reads `SEARCH_TRUSTED_PATH` (default `/config/search-trusted.json`), a JSON object
`{"quantisers": [...], "labs": [...]}` with exactly those two keys. When the file is absent the
sets are empty. A file of any other shape is logged, and the search runtime then stays with the
child. The deploy side seeds the file from the services image (sbstndalton/noevia
`deploy/tools/seed-search-trusted.sh`), so that ranking is unchanged.

**Store readiness.** The child's startup creates the store, so the first native answer waits
once for the child's `GET /api/v1/health`. Until the child has answered, the search answers the
forwarded route's 502.

**Snapshot errors.** A models-folder snapshot this side cannot follow exactly answers the native
503, where Python could answer an empty set or its 500. That covers a walk refusal, a
zero-sized filesystem and an unreadable `models.ini`. A stored value Python would trip over
answers the 500.

**Still from the child.** `POST /api/v1/internal/search-repo-runtime` still provides the
repository read's fit chips and context estimates, because the estimates need
`autoconfig.analyze`.

**Differential.** `tools/gen-fit-oracle.py` records `main._search_runtime` with synthetic
trusted sets (the real ones never enter this repository). `search::live::tests` reproduces
those answers byte for byte.
