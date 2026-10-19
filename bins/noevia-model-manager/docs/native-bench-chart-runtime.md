# Native benchmark chart inputs (#1318 step 3e)

GET `/api/v1/benchmark/runs/{id}` builds its chart runtime locally when the native section-autoconfig service and backend sampler are selected. Its existing benchmark-store reader and response assembly are unchanged. Without both services, the existing child runtime call remains. No new selector, database writer, startup migration or live activation is introduced.

Capacity is the largest backend VRAM value, rounded to two decimals. Each prediction uses the existing native autoconfig analysis with the section's resolved file, current settings and companion discovery. It sizes the single resolved file, even when neighboring shards exist; it does not use the section-autoconfig route's folder sum or system-role refusal. The chart compares at configured context, falling back to the recommended total context then recommended context. Equidistant frontier rows keep the first row. Only weights plus KV are shown.

The backend descriptor list is one bounded snapshot for the chart request. Discovery failures give zero capacity and null predictions. Missing/bad files, empty frontiers and per-alias analysis failures give null predictions, matching the helper's optional-estimate behavior. Existing native bounds and containment rules apply: an unsupported context spelling or magnitude, an out-of-root path, a refused header/analysis, or a read-slot deadline also leaves that optional prediction null. There is no child fallback once this native path is selected. The chart path never ingests telemetry or writes settings/files.

`tools/gen-bench-chart-oracle.py` runs the actual services chart runtime and `_predicted_vram_gb` on synthetic files. It records source digests, headers and answers. Cases cover single-file versus folder sizing, invalid/missing files, empty backends, configured and fallback contexts, first-row ties and rounding. CI regenerates the fixture from its pinned services source and requires identical bytes. Router qualification proves that an unavailable child does not prevent this native path from returning a benchmark response.

The benchmark observer, safe-default/token/download planning helpers, cache/error mirrors and startup database initialization still depend on the child. This change does not claim child retirement.

Optional chart inventory shares the sampler's ten-second negative cooldown after a timed-out
Docker read with optional section-autoconfig and search inventory. During that cooldown chart
capacity remains zero and predictions null; authoritative native autoconfig keeps its refusal.
No successful inventory values are cached.
