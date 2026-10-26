# Inbound DAV adapter preparation

This crate is a source-only stage. It has no binary, listener, router registration, production server_dav::Backend implementation, or image selection. Node's inbound DAV listener remains authoritative.

auth.rs verifies an app password through the existing Rust server-auth path and takes one read-only snapshot of credential, account, sharing, Diary, and local-storage eligibility. It does not update app_passwords.last_used_at. A snapshot is not a mutation fence.

diary_io.rs has only fixed read endpoints (GET /api/files and POST /api/file), a numeric loopback target, identity-bound tenant assertion, byte and structural caps, no redirects or proxy, and synthetic tests. It is not wired to a listener or a server_dav backend; any future read backend must recheck live authorization at the operation boundary.

storage_ops.rs refuses PUT, MKCOL, DELETE, MOVE, COPY, and preserve before any transport call. A future mutation backend needs a shared revocation/mutation fence, guarded Diary operations, symlink-safe legacy-local handling, durable uncertain-write recovery, last-used writer ownership, and full Node method parity before exposure. source315 currently always routes PUT, so a read-only listener cannot safely expose it without a reviewed method capability gate. No listener is part of this stage.
