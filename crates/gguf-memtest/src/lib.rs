//! Test-only crate. Its integration tests install a counting global allocator (the only
//! `unsafe` in the workspace) to measure peak heap use of `gguf`, the way noevia's Python
//! tests use tracemalloc. It exports nothing and is never published.
#![forbid(unsafe_code)]
