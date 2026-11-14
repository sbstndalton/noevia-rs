# storage-tools (noevia#2759, slices 1 and 1b)

noevia's own storage file tools, over the account's existing storage connection (any WebDAV
server the connection points at), replacing the curated external Files box.

This crate is **inert**: a leaf crate nothing depends on, no binary, no wiring. It holds:

* the box definition `storage-files` (server `noevia`), with JSON Schemas and `manifest_line()` in
  the tool-box manifest shape:
  * reads: `storage_list_folder`, `storage_read_file`, `storage_find_files`;
  * writes (`write: true`; the host puts each behind the person's approval): `storage_write_file`
    (`mode` `create` or `replace`, content up to 512 KiB), `storage_create_folder`,
    `storage_delete_file` (files only);
* strict argument parsing (`parse`): unknown tools and keys, wrong types, strings over 500 bytes,
  odd extensions, oversized or missing content, an unknown mode and a write on the storage root
  are refused with short model-facing messages;
* the Diary-folder refusal (`private_refusal`, and `run`'s `skip`): fail closed when the Diary
  folder setting normalises to nothing, or when the connection is not WebDAV/Nextcloud (S3 paths
  all sit under the corpus root). The host passes the Diary folder as `skip` for calls bound for
  an external provider, and for every write whatever the provider; a write without `skip` is
  guarded by the connection's own Diary folder (`corpus_root`), so no write lands inside it. There
  are no Unicode normalisation tables here: a Diary folder with a non-ASCII character blocks every
  path with a non-ASCII character (fail closed; ASCII Diary folders keep the plain rule);
* reads through `server-storage-reads` (`protocol::execute` over any `Transport`; the owned child
  in production). `storage_find_files` is a bounded breadth-first walk (40 listings, 50 matches,
  depth 6, 60 s) that never descends into the skipped folder. The 60 s limit only stops new
  listings: one already running keeps the read engine's 31 s budget, so a search can take up to
  about 91 s;
* writes through `server-storage-reads::writes`, WebDAV/Nextcloud only (S3 and local connections
  refuse every write):
  * create sends `If-None-Match: *`. Not overwriting an existing file relies on the server
    honouring it (the storage client checks first on a space address, noevia#2751); a server that
    answers 200/204 instead of 201 gets a success with `created: false` and a `warning` that an
    existing file may have been replaced;
  * replace reads the file's version first (Depth 0 PROPFIND) and writes under `If-Match` with it,
    refusing a missing file, a folder, or a file with no version. It guards only the window
    between its own PROPFIND and PUT, not a stale approval: the wiring slice must pass the version
    shown at approval (TODO for the host; no API change yet);
  * delete reads the version first: a missing file answers `deleted: false`; a folder or a file
    with no version (nothing to guard the DELETE with) is refused; otherwise it deletes under
    `If-Match`;
  * create folder answers `existed: true` when anything is at the path already, a file included
    (MKCOL 405); nothing is changed then.

No output or message names the connection's base URL, user name or secret.

Later slices: wiring into the in-app tool dispatch behind a default-off switch (writes behind the
approval card); the provider egress crate learning the `storage_` prefix; then MOVE/COPY in the
owned child. CalDAV/CardDAV tools are their own item.

Tests: `tests/tools.rs`, a fake transport serving a small synthetic tree and recording each
request's method, path and headers.
