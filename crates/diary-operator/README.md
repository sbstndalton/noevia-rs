# Offline Diary operator support

`diary-backup-restore BACKUP_ROOT MANIFEST_JSON NEW_DIRECTORY` verifies a downloaded
immutable backup and restores only into a new directory. It never chooses a latest
manifest, contacts a server, or overwrites an existing destination. Payloads are
verified before creating anything. Each file is created exclusively, flushed and
fsynced. Like the original operator, a filesystem failure during materialization
can leave a partial new destination; there is no staging rename or parent-directory
fsync. The argument parser preserves the original Python 3.12 help/error text.

`diary-config [CONFIG_YAML]` prints the resolved configuration for offline diagnosis.
It applies environment overrides in their original order, including the legacy
CORPUS_REMOTE_ROOT alias. Output can contain credentials; treat it like the source
configuration. `DIARY_CONFIG` selects the file; `DIARY_CONFIG_DIR` selects the installed
default config directory (`/app/config`). The library also provides open-mode policy,
YAML logging-template loading and system-prompt file loading. It starts no server.

The native YAML event parser retains SafeLoader's YAML 1.1 scalar resolution,
duplicate-key ordering, merge precedence and shared aliases during environment
overrides. The value layer preserves Python integers, nonfinite numbers, float
spelling, key equality/coercion and JSON ensure_ascii behavior. The JSON reader
handles duplicate keys, Unicode escapes (including lone escaped surrogates), BOMs
and Python's UTF-8/16/32 byte-input detection. Decimal integers retain the reference
runtime's default 4,300-digit guard. Unicode decimal digit blocks are from the
reference Python 3.12 Unicode 15.0 database.

Configuration files are explicitly decoded as UTF-8, like `Path.read_text("utf-8")`;
UTF-16/32 byte detection applies only to backup JSON, which Python reads as bytes.
Tagged bytes/date/datetime/set values are not supported types of any runtime config
field. Their diagnostic JSON projection raises the same TypeError as the original
Python projection, rather than silently converting them to strings or nulls.

The Python HTTP service still imports its original config module until the later
standalone-server move. The restore function remains an imported smoke/test oracle;
its historical Python CLI spelling execs the native binary in Services. See Services
`diary/OPERATOR-PORT.md` for the complete cross-repository consumer audit.

## Oracle and regression tests

`tests/fixtures/SOURCE.json` identifies and hashes the original public Python files.
Run generators with Python 3.12 and PyYAML, using an evidence directory **outside**
the checkout. No generator imports app.py, starts a server, or contacts storage.
`oracle.py record SERVICES_REPO EVIDENCE` freezes the original source from the named
Git commit; `replay NATIVE_BINARY EVIDENCE` checks restore bytes, modes, side effects,
stdout, exception text and exit codes. The managed-storage dependency is reduced to
its unchanged safe_key function to avoid unrelated runtime imports.

`config_oracle.py`, `edge_oracle.py`, `json_oracle.py`, `argv_oracle.py` and
`io_oracle.py` record/replay config, schema/encoding, JSON, argument and filesystem
error cases. Frozen edge fixtures include their exact input bytes and are replayed
by `cargo test -p diary-operator` without requiring Python. Scalar/namespace validation
and filesystem side effects remain covered by the separate restore oracle.

Argument errors and warnings are compared byte-for-byte. Only interpreter frames
from the legacy CLI/loader's **uncaught exceptions** are omitted: config decoding,
construction, integer-conversion, type/cycle/JSON-projection failures, backup JSON
parsing, restore validation, and filesystem exceptions. Their complete exception
class/message, multiline YAML location diagnostics and Python exception notes are
compared without shortening or reclassifying them. The generators retain full raw
crash traces privately; the original restore oracle binds only its known fixture
path prefix for portability. Help, usage, warnings and handled argument errors have
no traceback normalization.
