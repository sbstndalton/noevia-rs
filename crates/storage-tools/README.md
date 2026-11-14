# storage-tools (noevia#2759, first slice)

noevia's own storage file tools, over the account's existing storage connection (any WebDAV
server the connection points at), replacing the curated external Files box.

This slice is **inert**: a leaf crate nothing depends on, no binary, no wiring. It holds:

* the box definition `storage-files` (server `noevia`): `storage_list_folder`,
  `storage_read_file` and `storage_find_files`, all reads, with their JSON Schemas and
  `manifest_line()` in the tool-box manifest shape;
* strict argument parsing (`parse`): unknown tools and keys, wrong types, strings over 500 bytes
  and odd extensions are refused with short model-facing messages;
* the Diary-folder refusal for calls bound for an external provider (`private_refusal`, fail
  closed when the Diary folder setting normalises to nothing);
* execution through `server-storage-reads` (`protocol::execute` over any `Transport`; the owned
  child in production). `storage_find_files` is a bounded breadth-first walk (40 listings,
  50 matches, depth 6, 60 s) that never descends into the skipped (Diary) folder. The 60 s
  limit only stops new listings: one already running keeps the read engine's 31 s budget, so a
  search can take up to about 91 s. With a skip folder, a connection that is not WebDAV/Nextcloud
  (S3 paths all sit under the corpus root) refuses every call.

No output or message names the connection's base URL, user name or secret.

Later slices: wiring into the in-app tool dispatch behind a default-off switch; the provider
egress crate learning the `storage_` prefix; the writes behind the approval card
(`storage_write_file`, `storage_create_folder`, `storage_delete`); then MOVE/COPY in the owned
child. CalDAV/CardDAV tools are their own item.

Tests: `tests/tools.rs`, a fake transport serving a small synthetic tree.
