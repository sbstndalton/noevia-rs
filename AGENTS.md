# Agent rules for noevia-rs

Do not rename Cowork-prefixed identifiers (env vars, images, cookies). Preserve tenant isolation and all three write-approval actions (Allow once / Decline / Allow for this chat). Synthetic fixtures only — never real Diary data. Never flip feature flags. No model/inference runs.

Issues are filed in sbstndalton/noevia.

- Library code: no `unsafe`, no panics, no `unwrap`/`expect` on input-derived data.
- Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` before pushing.
- Never use `git stash`.
