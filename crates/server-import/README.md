# Native conversation-import planner foundation

`plan_import(&JValue, Snapshot) -> Result<String, Error>` produces the complete
conversation import plan as compact JSON. This is a pure library: no parser,
clock, callbacks, HTTP, filesystem, credential lookup, tenant selection or writes.
The caller supplies a tenant snapshot, a finite generated-ID sequence and an
explicit `now`. Defaults are **5000 messages / 200000 UTF-16 units per content**.

Predecessor: noevia-core `0a2040392b09915bcaeef3f5d9b4cbf4b163f27d`,
`server/chat-import.cjs`, Git blob `1dbad49bf7d547325b070fdb19294b5229171111`.
Source contract controls also use `server/chat-import.test.cjs` at that revision.
No Node oracle, differential fixtures, compiler, tests or mutants have been run
for this foundation. The static tests are derived from reading the actual source;
they are not recorded Node outputs or current parity certification.

## Supported contract

The planner preserves envelope validation before the 2000-chat limit; raw arrays
are JavaScript objects and become untitled chats; other raw scalars are skipped.
It preserves title truthiness and String coercion, finite-number timestamps,
existing safe-ID skips, unsafe/tombstoned/duplicate ID replacement, collision
consumption order, first-seen group order and last matching snapshot project.
Imported project IDs cannot select a project: only a name matches supplied
snapshot data. Histories filter user/assistant string-content messages **before**
retaining the last message window. Tools filter string names before retaining
first 50, always with `status: "done"`; reasoning, arguments, results and unknown
fields are dropped. Titles/content/tool names use exact UTF-16 prefixes.

Safe raw IDs match `c-` followed by 1..80 ASCII letters/digits/underscore/hyphen.
Consumed generated IDs must also match this format; unused unsafe supply entries
are not consumed. A generated safe tombstoned ID is intentionally allowed: the
actual Node generated-ID collision loop checks used/existing IDs only.

Snapshot names use bounded String coercion and ECMAScript whitespace trim for
matching; only the trimmed grouping name must be ASCII. Input project names trim
then truncate to 120 UTF-16 units before the ASCII lowercase grouping key.
Matching snapshot names retain their original JSON value in the output.
String-hint coercion supports plain arrays, primitives and objects (including
an own `valueOf`); an own noncallable `toString` is refused. Object serialization
orders canonical integer keys 0..4294967294 numerically before other keys in
insertion order, recursively. Index sorting is O(k log k); other passes are
bounded by input sizes, and this library makes no blanket linear-time claim.

## Explicit strict admission

The API accepts an **already constructed** finite plain JSON `JValue`.
Parsing, caller input allocation and eventual result consumption are outside
this API's contract. Missing properties use `JValue::get`'s Undefined sentinel;
manually supplied Undefined values, duplicate object keys and nonfinite numbers
are refused. Lone-surrogate JSON cannot be represented by Rust strings. If a
requested prefix splits an astral character's surrogate pair, this library
returns `Unsupported(SurrogateCut)`; it never silently drops or replaces it.
Non-ASCII grouping names return `Unsupported(GroupingName)` until complete
ECMAScript Unicode lowercase behavior is supported.

Admission caps are 64 MiB for combined compact input+explicit snapshot
serialization, depth 64, 262144 values+object keys, output 64 MiB, chats 2000,
snapshot projects 2000 and each existing/tombstone/ID-supply list 16000.
Depth counts the full compact snapshot: root at 0, projects array at 1,
project object at 2 and name value at 3. A snapshot name containing 61 nested
arrays with a scalar leaf reaches depth 64; 62 nested arrays are refused.
Clock must be finite; options must be positive and at most their defaults.
The snapshot serialization counted for admission uses
`{projects:[{id,name}],existingChatIds:[],tombstones:[],generatedIds:[],now,maxMessages,maxChars}`
with quoted JSON keys and values, and all delimiters/options are charged.
Transient normalized snapshot keys are additionally capped at 64 MiB in total.
All input-dependent collection/string growth uses checked sizes and fallible
reservation. Scalar number formatting delegates to the existing `js-json`
helper; its fixed numeric temporaries and caller allocations are not an OOM
certificate. These are logical bounds, **not process RSS or allocator guarantees**.
The two Node readable errors have exact 400 status/public messages via
`node_public_error`; limits, unsupported input, exhausted IDs and allocation
failure have no fabricated Node public 400 mapping.

## Integration remains separate

No server bin uses this crate. `/api/import/conversations` remains Node-owned
and runtime behavior remains unchanged. No selector, capability, supervisor,
route ownership, release pin, journal or shared-store handoff is added here.
A later authenticated native route must own the complete journal/recovery,
CSRF/tenant checks, store coordination and external-folder writes, with a
switch default off, current Node differential evidence, front 0-diff replay and
negative controls. This foundation does not complete M4 import.

Static contract/property tests cover meaningful future mutation controls:
filter-after-window, first instead of last message window, role/unknown-field
leaks, tool 51, tombstoned raw-ID retention, generated tombstone rejection,
unsafe ID acceptance, group reorder/first-wins snapshot, UTF-8 truncation,
truthiness 0, missing clock fallback, integer-key order and silent chat 2001
truncation. No current mutant execution is claimed.

Server deployments remain revoked by the owner because of Jellyfin lag. No
DaServer copy/build/image/restart/live check is part of this source foundation.
