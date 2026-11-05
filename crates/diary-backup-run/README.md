# Diary legacy backup worker

`diary-backup-run run-v1` reads one newline-terminated JSON control record containing
`db`, `tenant`, `destination`, `corpusRoot`, `now` and `lockFd`. The caller holds the existing
managed-backup flock through process exit and reap. The broker passes that locked
file descriptor with pass_fds. The native process leaves the inherited descriptor
open until kernel process teardown, so broker death cannot release the flock before
the native final database write and exit. The descriptor identity is checked against
the managed-backup.lock beside the supplied database. The child opens the database
read/write without create, checks tenant/active, snapshots metadata, fetches each body
by path plus version, and writes only the existing `backups` row.

The child emits `{type:"verify",path,data}` records with standard-base64 payloads.
The host uses its existing gated backend to perform immutable create-and-verify and
answers `{result:"ok"|"failed"|"loginRefused"}`. No credential enters the child.
A terminal `{type:"done",outcome:{result:...}}` reports idle, abandoned, complete or
failed. Only a complete run proves storage contact. The adapter has a 300-second
total deadline, bounded control frames, and kills/reaps on failure; it never reruns
the Python worker. The existing remote client retains its request deadlines.

Manifest bytes use diary-backup-format's Python JSON writer and existing immutable
keys. No receipt table, namespace, schema migration, restore change, scheduler or
account-concurrency change is introduced. Synthetic differential tests byte-compare
the complete remote tree and restore using the unchanged Python operator CLI.
