# noevia-rs

Rust workspace for noevia strangler slices: self-contained Rust replacements for
pieces of [noevia](https://github.com/sbstndalton/noevia), built and tested here
one slice at a time.

- Integration, releases and the issue tracker live in
  [sbstndalton/noevia](https://github.com/sbstndalton/noevia). File issues there.
- Every slice ships dark. Nothing here replaces a live code path until the owner
  switches it on.

## Layout

- `crates/` — libraries
- `bins/` — command-line binaries

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The toolchain is pinned in `rust-toolchain.toml`.
