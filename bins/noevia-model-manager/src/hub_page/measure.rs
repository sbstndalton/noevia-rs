//! Test helpers shared by the Hub page tests of `hub_page`, `check_updates` and `draft_heads`:
//! heap measured from buffer capacities, hostile page builders and the real Hub fixtures.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::{read_capped, PAGE_CAP, PAGE_NODES};
use model_files::json::Value;

pub(crate) const MIB: usize = 1024 * 1024;

/// A real `expand=true` listing of a large repository of GGUF files (8 pages of 50 entries).
pub(crate) const GGUF_LARGE: &str =
    include_str!("../../tests/fixtures/hub-tree/hub-tree-gguf-large.json");
/// A real `expand=true` listing of a repository of pickle files, with `securityFileStatus` and
/// its `pickleImports` lists.
pub(crate) const PICKLE_IMPORTS: &str =
    include_str!("../../tests/fixtures/hub-tree/hub-tree-pickle-imports.json");

/// The pages of a fixture, each as the compact JSON text of the page the Hub sent.
pub(crate) fn fixture_pages(fixture: &str) -> Vec<Vec<u8>> {
    let whole: serde_json::Value = serde_json::from_str(fixture).unwrap();
    whole["pages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|page| serde_json::to_vec(page).unwrap())
        .collect()
}

/// Heap bytes of a buffer, charged as glibc's malloc does (at least 24 usable bytes, an 8-byte
/// header, 16-byte steps). Deterministic: capacities, no allocator hooks (`unsafe_code` is
/// forbidden).
pub(crate) fn charged(bytes: usize) -> usize {
    if bytes == 0 {
        0
    } else {
        (bytes.max(24) + 8).div_ceil(16) * 16
    }
}

/// Heap bytes a parsed tree holds, from its buffer capacities.
pub(crate) fn heap(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) | Value::Float(_) => 0,
        // `BigInt` keeps its digits in a `String` reserved exactly (`try_reserve_exact`).
        Value::Int(i) => charged(i.to_string().trim_start_matches('-').len()),
        Value::Str(s) => charged(s.capacity()),
        Value::Arr(items) => {
            charged(items.capacity() * std::mem::size_of::<Value>())
                + items.iter().map(heap).sum::<usize>()
        }
        Value::Obj(pairs) => {
            charged(pairs.capacity() * std::mem::size_of::<(String, Value)>())
                + pairs
                    .iter()
                    .map(|(k, v)| charged(k.capacity()) + heap(v))
                    .sum::<usize>()
        }
    }
}

/// The most a page's tree may hold: per node, a slot in its parent (an object pair is the larger)
/// with the parent's doubling slack, plus its own smallest allocation; and every string byte
/// twice (strings grow by doubling as they are unescaped), at most the page's bytes.
pub(crate) fn tree_bound() -> usize {
    let per_node = 2 * std::mem::size_of::<(String, Value)>() + charged(1);
    PAGE_NODES * per_node + 2 * PAGE_CAP
}

/// The most `check_updates::entries()` may hold beside the tree (noevia#2013): the pairs it
/// copies out, with the vector's previous buffer while it grows. A copied path or date is a
/// subtree of its own file entry, entries do not overlap, and a clone is allocated to its exact
/// length (no doubling slack, so no more than the tree's own per-node term): per node, a slot of
/// the larger kind and its own allocation (an allocation is at most 31 bytes over its length);
/// and every string byte once, at most the page's bytes. The vector holds one pair per file
/// entry, an entry being at least 9 nodes (the object, `type`, `path`, `lastCommit` and `date`
/// with their keys); it grows by doubling, so its buffer is under twice that count and the
/// previous one under once, 3 times in all.
pub(crate) fn output_bound() -> usize {
    let pair = std::mem::size_of::<(String, Value)>();
    PAGE_NODES * (pair + charged(1)) + PAGE_CAP + 3 * (PAGE_NODES / 9 + 1) * pair
}

/// The most a check holds once the page is parsed: the tree and the output built beside it
/// (noevia#2013). The raw page is gone by then, so this is not added to `PAGE_CAP`.
pub(crate) fn peak_bound() -> usize {
    tree_bound() + output_bound()
}

/// `unit` repeated into exactly `len` bytes between `open` and `close`.
pub(crate) fn filled(open: &str, unit: &str, close: &str, len: usize) -> Vec<u8> {
    let mut page = String::with_capacity(len);
    page.push_str(open);
    while page.len() + unit.len() + close.len() <= len {
        page.push_str(unit);
    }
    while page.len() + close.len() < len {
        page.push(' ');
    }
    page.push_str(close);
    assert_eq!(page.len(), len);
    page.into_bytes()
}

/// Streams `page` in 48 KiB chunks through the capped reader, as a Hub response arrives (not a
/// power of two, so plain doubling would overshoot the cap: 12 MiB, then 24 MiB).
pub(crate) async fn read_page(page: &[u8]) -> Option<Vec<u8>> {
    let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = page
        .chunks(48 * 1024)
        .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
        .collect();
    read_capped(futures_util::stream::iter(chunks), PAGE_CAP).await
}

/// JSON values and object keys in `value`, as `model_files::json` counts them against its node
/// limit.
pub(crate) fn nodes(value: &Value) -> usize {
    match value {
        Value::Arr(items) => 1 + items.iter().map(nodes).sum::<usize>(),
        Value::Obj(pairs) => 1 + pairs.iter().map(|(_, v)| 1 + nodes(v)).sum::<usize>(),
        _ => 1,
    }
}

/// The longest string (value or key) in `value`, in characters.
pub(crate) fn longest_string(value: &Value) -> usize {
    match value {
        Value::Str(s) => s.chars().count(),
        Value::Arr(items) => items.iter().map(longest_string).max().unwrap_or(0),
        Value::Obj(pairs) => pairs
            .iter()
            .map(|(k, v)| k.chars().count().max(longest_string(v)))
            .max()
            .unwrap_or(0),
        _ => 0,
    }
}
