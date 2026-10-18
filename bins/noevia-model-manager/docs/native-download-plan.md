# Native download planning (#1318)

`native-download-plan` adds the default-Python selector `MODEL_MANAGER_DOWNLOAD_PLAN_IMPL=rust`.
Native selection requires the compiled feature, actual native jobs and canonical model/data/config
roots. Invalid explicit selection refuses startup before the child. The release integration must
compile the feature and pass the selector through both compose copies and retained server override.

The branch handles POST downloads and section draft-head downloads: target catalog, shards,
first four projector companions, smallest draft companion, source history, anonymous URL HEAD
sizes and contained destinations. It never calls the child plan route. Python selection preserves
the existing planning protocol. Job insertion, duplicate handling and the delete interlock stay in
Downloads.queue. Every destination validates before queue insertion or directory creation.

Listing reuses the bounded streaming DraftHeads parser: 40 pages, 16 MiB per page and existing
projected-node bounds, with same-origin pagination. Production Hub is fixed HTTPS, no automatic
redirects. Arbitrary explicit URL HEAD requests never read or send stored tokens. Redirects retain
the download transport restrictions against HTTPS downgrade and public-to-private destinations.
Transfers retain the existing token/URL policy.

Admission is two plans. Total deadline 120 seconds includes admission, source reads and listing;
HEAD has one 10-second deadline across at most 20 redirects. Cancellation releases permits.
Plans cap at the existing 1024 files and 8 MiB. Transfer sizes outside signed 64-bit representation
refuse. Bounds/capability refusal gives 503 without child fallback; compatible ordinary planner
errors retain 400/404/409/500. These limits are stricter than Python best-effort defaults. HEAD
network/HTTP errors, timeout and redirect limit retain size zero; policy-denied redirects refuse.
An abstract malformed Content-Length raises; malformed raw HTTP framing can fail earlier in the
HTTP client and retain size-zero behavior.

The actual Python generator runs api.start_download/download_draft_head under real PLANNING,
with synthetic listing/HEAD adapters and temporary SQLite/config/files. Its 26-case source-hashed
fixture covers response/job choices. Rust tests cover plans, target/destination validation,
anonymous local HTTP, redirect limits, cancellation/auth and native routing with an unavailable
child. Only three synthetic bytes transfer. No real models, Docker or external network are used.

Rollback: select python and restart, retaining native download jobs. Keep the compatibility child
installed until the separately qualified child-retirement step.
