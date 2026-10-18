//! noevia#2000: the read and parse bounds of one Hub tree page, shared by the update check
//! (`check_updates`, one page per repository) and the draft-head lookup (`draft_heads`, every
//! page of the listing). Each page is read into a buffer that never passes [`PAGE_CAP`] and is
//! parsed under [`PAGE_NODES`], [`PAGE_STRING_CHARS`] and [`JSON_DEPTH`]; a page over any of them
//! is a failed request, as for a transport error. One set of tests (`hub_page/tests.rs`) holds the
//! bounds, including pages measured from the real Hub (`tests/fixtures/hub-tree/README.md`).
use model_files::json::{Limits, Value};

/// noevia#1985, noevia#1996: largest tree page read. A Hub page is at most a few thousand entries
/// (the `expand=true` listing is 50 per page, well under 1 MiB); a larger one is a failed request.
pub const PAGE_CAP: usize = 16 * 1024 * 1024;
/// noevia#1985, noevia#1996: most JSON values and keys in one page. A page of bare numbers at
/// [`PAGE_CAP`] would be about 8 million, each an allocated big integer. Measured on real
/// `expand=true` pages (noevia#1999, `hub_page/tests.rs`), an entry is 17 to 71 nodes for a GGUF
/// file (50 on average) and up to about 300 for a pickle file with its `pickleImports` list (7
/// nodes per import). The Hub sends 50 entries per page, so the busiest real page is under 2 % of
/// this, and it would take about 860 entries of the worst measured kind to reach it. Over it, a
/// failed request.
pub const PAGE_NODES: usize = 256 * 1024;
/// noevia#1985, noevia#1996: longest string in one page, in characters (paths, dates, commit
/// titles and scan messages are a few hundred at most: 168 in the real pages of noevia#1999).
/// Over it, a failed request.
pub const PAGE_STRING_CHARS: usize = 64 * 1024;
/// CPython parses deeper JSON (until RecursionError); this bound is a failed request instead.
pub(crate) const JSON_DEPTH: usize = 512;

/// A response body of at most `cap` bytes (`None` past it or on a transport error). The buffer
/// grows by doubling but never beyond `cap` (noevia#1985, noevia#1996).
pub(crate) async fn read_capped<S, E>(mut stream: S, cap: usize) -> Option<Vec<u8>>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
{
    use futures_util::StreamExt;
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        let len = body.len().checked_add(chunk.len())?;
        if len > cap {
            return None;
        }
        if body.capacity() < len {
            let grown = body.capacity().saturating_mul(2).max(len).min(cap);
            body.try_reserve_exact(grown - body.len()).ok()?;
        }
        body.extend_from_slice(&chunk);
    }
    Some(body)
}

/// The parse limits of one page.
pub(crate) fn page_limits() -> Limits {
    Limits {
        bytes: PAGE_CAP,
        depth: JSON_DEPTH,
        nodes: PAGE_NODES,
        string_chars: PAGE_STRING_CHARS,
        ..Limits::default()
    }
}

/// `tree.json()` of a page: `None` where `json.loads` raises, and for a page over [`PAGE_CAP`],
/// [`PAGE_NODES`], [`PAGE_STRING_CHARS`] or [`JSON_DEPTH`], a failed request.
///
/// A UTF-8 page is parsed where it lies; a UTF-16 or UTF-32 one is decoded, its raw bytes dropped
/// and the text shrunk to its length (noevia#1995) before the parse. Either way only the tree
/// outlives this call.
pub(crate) fn parse_page(body: Vec<u8>) -> Option<Value> {
    let decoded = match crate::prompts::decode_json_bytes(&body, PAGE_CAP).ok()? {
        std::borrow::Cow::Borrowed(_) => None,
        std::borrow::Cow::Owned(text) => Some(text),
    };
    match decoded {
        Some(mut text) => {
            drop(body);
            // The decoder may have grown past what it holds.
            text.shrink_to_fit();
            model_files::json::parse_with_limits(&text, page_limits()).ok()
        }
        None => {
            let text = crate::prompts::decode_json_bytes(&body, PAGE_CAP).ok()?;
            model_files::json::parse_with_limits(&text, page_limits()).ok()
        }
    }
}

#[cfg(test)]
pub(crate) mod measure;
#[cfg(test)]
mod tests;
