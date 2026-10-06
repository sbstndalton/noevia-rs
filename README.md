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

## Slices

- `crates/gguf` + `bins/gguf-meta` (sbstndalton/noevia#896): bounded GGUF metadata
  reader. `gguf-meta <path>` prints the same summary JSON as model-manager's
  `gguf_meta.py`. Its differential corpus is regenerated from the Python reference with
  `NOEVIA_GGUF_META=<noevia>/services/model-manager/app/gguf_meta.py python3 tools/gen-fixtures.py`.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The toolchain is pinned in `rust-toolchain.toml`.
