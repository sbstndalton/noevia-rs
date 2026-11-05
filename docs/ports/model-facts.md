# Native budget save ordering

The native authority owns the effective boot/live budget. Admission reads the
start-bound atomic snapshot directly, without taking the writer mutex. The
settings row is persistence for the next start, never an admission fallback.

Every save holds the authority's writer mutex through all of these steps:

1. Capture the exact previous settings row, including its absence, and write the
   new row in one SQLite transaction. Commit it before publishing anything.
2. Publish the candidate snapshot with atomic temporary-file replacement. Only
   committed budgets can therefore become visible to admission readers.
3. If publication fails, leave the old snapshot and live state in place. Restore
   the old row (or delete the new row if it was previously absent) in a new
   transaction while still holding the same writer mutex, then return 503.

A failed initial commit never changes the snapshot or its generation. A failed
publication never exposes the candidate, so it needs no compensating snapshot or
generation change. Periodic publication uses the same mutex and cannot publish
an intermediate candidate. Node writes its audit only after a successful native
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
