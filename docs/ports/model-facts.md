# Native budget save ordering

The native authority owns the effective boot/live budget. Admission reads the
start-bound atomic snapshot directly, without taking the writer mutex. The
settings row is persistence for the next start, never an admission fallback.

Every save holds the authority's persistence mutex through all of these steps.
Scan operations never acquire this mutex:

1. Capture the exact previous settings row, including its absence, and write the
   new row in one SQLite transaction. Commit it before publishing anything.
2. Publish the candidate snapshot with atomic temporary-file replacement. Only
   committed budgets can therefore become visible to admission readers.
3. If publication fails, leave the old snapshot and live state in place. Restore
   the old row (or delete the new row if it was previously absent) in a new
   transaction while still holding the same writer mutex, then return 503.

A failed initial commit never changes the snapshot or its generation. A failed
publication never exposes the candidate, so it needs no compensating snapshot or
generation change in the published snapshot. Periodic publication uses the same
persistence mutex and cannot publish an intermediate budget candidate. Concurrent
scan invalidations remain independent and are never rolled back by a failed save. Node writes its audit only after a successful native
save acknowledgement; every rejected save therefore writes no success audit.

## Crash and compensation failure

There is a deliberate window between committing the row and publishing the
snapshot. During that window readers retain the previous live budget. A crash
there causes the next start to rebuild its snapshot from the new committed row,
even if the save had not returned. This matches the Node oracle's ordering:
`store.set` occurs before live adoption and before the save completes.

If publication fails and the compensating database transaction also fails, the
authority logs an error and returns 503 while retaining the old live snapshot
and in-memory budget. The periodic publisher keeps that old live value. This is
the only persistent row/snapshot disagreement introduced by the save protocol:
the new row remains committed despite the rejected save. A restart adopts that
row; a later successful save can also reconcile it. No success audit is written.

The regression suite polls the admission snapshot during an induced commit
failure and repeated publication failures. It also checks restoration of both a
present and an absent row across restart, and injects a compensation failure to
verify the explicitly documented restart behavior.

## Scan cache (G3)

A separate short-lived mutex in the same authority owns the folder scan and
generation. It performs no database or filesystem I/O. Snapshot reads capture an
Arc under that lock, then clone its value outside it; replaced entries are also
dropped after releasing the lock. Persistence remains serialized by the budget
writer mutex. A successful save takes a final short cache fence before returning,
so it also rejects scans started during persistence. Snapshot generation stamps
are sampled without holding the scan lock during publication; concurrent scan
invalidations can advance the cache further while the snapshot is written, as
with ordinary invalidations between periodic publications. Serialized snapshot
publication keeps stamps monotonic. Budget adoption
and scan invalidation both fence publication; each process has a fresh random
epoch, so tokens from an earlier process are unusable. Entry tokens additionally
fence a conditional refresh against replacement within a generation. There is
no Node cache, publication operation, or refresh worker.

The exact public GET retains the recorded Node policy: nonempty queries bypass
cache storage; cached answers carry no-store and X-Model-Scan; entries older
than 5,000 ms trigger one background read. A refresh has the oracle's 15-second
bound, facts reads use 8 seconds, and cold public reads retain 600 seconds.
Installed facts, private scan reads and refreshes share native in-flight reads.
The private store accepts only fixed read and invalidate operations; it has no
URL selector or scan-publication endpoint.

Node's remaining model/manager writers await native invalidation before work,
before releasing the response, and when the writer settles even if the caller
leaves or the writer fails. This also fences scans started during a mutation,
which the old before-only invalidation missed (#2405). Native budget saves
clear the scan only after persistence and snapshot publication succeed.

Assembly and supervisor pairing require rust-model-scan-cache-v1 with G1 and
the matching Core consumer. Manager path validation runs on raw segments before
URL normalization, preserving the administrator/configuration gates and the
400 Invalid path response for decoded traversal or separators.

Startup reads the existing application database with READ_ONLY and never CREATE.
Budget writes require an existing database with READ_WRITE and never CREATE.
A missing/unusable store retains the existing unavailable/default-budget behavior
without leaving an empty SQLite file before the application store bootstraps.
