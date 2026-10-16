# Native download targets read (noevia#1318)

`native-download-targets` with `MODEL_MANAGER_DOWNLOAD_TARGETS_IMPL=rust` (and `MODEL_MANAGER_IMPL=rust`) serves `GET /api/v1/download-targets` from the Rust front. Default `python`; nothing changes until the owner switches it on.

## What it answers

The models folder first (`{"id":"","label":"Models folder","path":MODELS_DIR}`), then every name declared in `MODEL_DOWNLOAD_TARGETS` (comma-separated, stripped as Python's `str.strip` does) or found as a mount point directly inside the models folder, sorted by code point, keeping only plain (non-symlink) folders whose name has no `/` and does not start with `.`.

A mount point is a top-level, non-symlink folder that `os.path.ismount` would report (another device than its parent, or the same inode), or whose path or resolved path is listed in `/proc/self/mountinfo` (field 5, `\NNN` escapes decoded to code points, `posixpath.normpath`-normalised). This is Python's `services._mount_points` / `_is_mount_point` rule, so same-filesystem bind mounts are found.

Nothing is written. Other methods keep the existing 405, and `POST /api/v1/downloads` stays with Python, which validates its `target` against the same two settings (the child is given the same canonical `MODELS_DIR` and `MODEL_DOWNLOAD_TARGETS`).

## Ownership

- Child confirmation `NOEVIA_NATIVE_MODEL_MANAGER_DOWNLOAD_TARGETS_CONFIRMED=1` only in native mode; the Python child then refuses the GET with 503. In python mode an inherited value is removed.
- `rust` without `MODEL_MANAGER_IMPL=rust` or without the compiled feature refuses startup before the child starts.
- Like #1889, settings the native read cannot name exactly as Python does keep Python serving the GET, with one log line: a case-folded alias of either name, a `MODELS_DIR` that is not absolute and in pathlib's normal form, non-UTF-8 values, or more than 1,024 declared names.

## Differential evidence

`tools/gen-download-targets-oracle.py` runs the actual services FastAPI app (pinned `noevia-services@1d44f5e`) on 20 synthetic trees and synthetic mount tables: declared names (dedupe, sort, whitespace, hidden, slashes, files, symlinks, missing), mount-listed folders, escaped spaces and tabs, normpath forms, every `splitlines` separator, short lines, odd and out-of-byte escapes, invalid UTF-8 tables, symlinked folders, a symlinked models folder (realpath match), a missing models folder, and JSON escapes in names. CI regenerates it on Linux and requires identical bytes; `tests/download_targets.rs` rebuilds each case and compares status, content type and body.

## Deliberate differences

- More than 65,536 top-level entries in the models folder, or a mount table over 4 MiB, answer FastAPI's `500 text/plain Internal Server Error` instead of being read in full.
- The resolved-path match uses the resolved models folder joined with the entry name (the entry is known not to be a symlink), which is what `os.path.realpath` returns for it.
