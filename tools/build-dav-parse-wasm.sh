#!/bin/sh
# Build dav-parse.wasm (noevia#967; since #976/#978/#977/#979 it also carries s3-list-parse, storage-path,
# upload-sniff and secret-envelope)
# reproducibly and print its sha256.
#   tools/build-dav-parse-wasm.sh [OUT]   (default: target/dav-parse.wasm)
# The same command runs in noevia-rs CI, noevia-core CI and noevia's web image build; noevia-core
# pins the resulting sha256 (server/dav-parse.lock), so the bytes must not depend on the host.
# Toolchain: rust-toolchain.toml (1.99.0) with the wasm32-unknown-unknown target. Host paths are
# remapped so the checkout and CARGO_HOME locations never reach the module.
set -eu
cd "$(dirname "$0")/.."
out="${1:-target/dav-parse.wasm}"
root="$(pwd -P)"
home="${CARGO_HOME:-$HOME/.cargo}"
export CARGO_INCREMENTAL=0 SOURCE_DATE_EPOCH=0
export RUSTFLAGS="--remap-path-prefix=$root=/noevia-rs --remap-path-prefix=$home=/cargo -C target-feature=-bulk-memory,-sign-ext,-multivalue,-reference-types,-nontrapping-fptoint,-mutable-globals"
unset CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_RUSTFLAGS
cargo build --locked --profile wasm-release --target wasm32-unknown-unknown -p dav-parse-wasm >&2
mkdir -p "$(dirname "$out")"
cp target/wasm32-unknown-unknown/wasm-release/dav_parse_wasm.wasm "$out"
if command -v sha256sum >/dev/null 2>&1; then sha256sum "$out"; else shasum -a 256 "$out"; fi
