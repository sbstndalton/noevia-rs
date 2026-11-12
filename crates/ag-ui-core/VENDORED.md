# Vendored: ag-ui-core

| | |
|---|---|
| Upstream | https://github.com/ag-ui-protocol/ag-ui |
| Path | `sdks/community/rust/crates/ag-ui-core` |
| Commit | `8a951d14144e7dde986ccec23364e01a36111a63` (`main` when vendored) |
| Licence | MIT, `LICENSE` in this directory is the upstream repository's root `LICENSE` ("Copyright (c) 2025"); no other licence file covers that path (the Rust workspace and the crate have none of their own, and `Cargo.toml` says `license = "MIT"`) |
| Vendored | 2026-11-12, sbstndalton/noevia#2446 slice 5 |
| Taken from | a `git clone` of the repository (read only, nothing of it run), not from crates.io |

Only this crate was copied (`src/`, `tests/`, `README.md`, and the licence). The sibling
`ag-ui-client` crate and the upstream `Cargo.lock` were not.

The crate reaches no network and no filesystem: its sources use `serde`, `serde_json`, `thiserror`
and `uuid` (random ids) and nothing else.

## Local changes

Everything Noevia added is marked `// noevia addition` or `noevia change`/`noevia addition` in a
doc comment. Upstream's sources otherwise build unchanged on this workspace's edition (2021) and
MSRV, and pass this workspace's lints (`unwrap_used`, `expect_used`, `panic` and `indexing_slicing`
are denied here).

1. `Cargo.toml`: rewritten for this workspace (`edition`, `rust-version`, `license`, `publish` and
   `[lints]` come from the workspace; dependencies are plain versions with the same requirements;
   `uuid` loses its `serde` feature, which nothing needs once ids are strings). The upstream file
   used edition 2024 and its own workspace.
2. `src/types/ids.rs`: `AgentId`, `ThreadId`, `RunId` and `MessageId` wrap a `String`, not a
   `Uuid` (protocol ids are opaque strings, and Noevia's stored message ids are not UUIDs). They
   serialize as the same JSON string. Added `new`, `From<String>`, `From<&str>`, `AsRef<str>` and an
   infallible `FromStr`; dropped `From<$name> for Uuid`, `AsRef<Uuid>` and `PartialEq<Uuid>`;
   `PartialEq<str>` compares strings. `ToolCallId` gains `new`, `From<String>`, `From<&str>` and
   `Display`, and its `random()` takes the first eight characters without slicing (the
   `indexing_slicing` lint).
3. `src/event.rs`: `EventType::Interrupt`, `InterruptEvent`, `InterruptReason` and
   `Event::Interrupt` (the approval-request shape upstream lacks; the other event kinds are
   unchanged), plus the two arms of `event_type()` and `timestamp()`.
4. `tests/unit.rs`: a `#![allow(...)]` of the four denied lints (upstream's tests use `unwrap`).
5. `tests/noevia.rs` (new): tests of 2 and 3.
6. `src/lib.rs`, `src/event.rs`, `src/types/input.rs`: formatting only (`cargo fmt --all --check` is a CI
   gate here: one blank line and the position of `use crate::JsonValue;` among the imports).

State deltas, snapshots of state and the rest of upstream's TODO list are not extended: this slice
does not need them.
