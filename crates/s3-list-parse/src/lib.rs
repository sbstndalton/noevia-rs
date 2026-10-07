//! Bounded S3 ListObjectsV2 page parser (noevia#976, strangler slice): a byte-for-byte port of
//! noevia-core's `server/s3-listing.cjs` (`s3PageRecordsJs`), the body scan `s3List` runs on each
//! page an S3-compatible endpoint returns.
//!
//! The endpoint is user-configured and possibly hostile, so every byte is untrusted. It reuses
//! dav-parse's forward-only scanner ([`dav_parse::element_texts`]) and single-pass entity decoder
//! ([`dav_parse::decode_xml_entities`]): no DTD, no entity declarations, no recursion, work linear
//! in the body. Node keeps the fetch, the request signing and the paging (including the stop on a
//! continuation token that does not advance); this crate only reads one page:
//!
//! - each `<CommonPrefixes>`'s first `<Prefix>`, decoded, trailing `/` removed, the query prefix
//!   stripped when it starts with it, kept when non-empty, as a directory;
//! - each `<Contents>`'s first `<Key>`, decoded, skipped when it ends with `/`, the query prefix
//!   stripped when it starts with it, kept when non-empty and without `/`, as a file whose size is
//!   the first `<Size>` when that is only ASCII digits;
//! - whether the first `<IsTruncated>` is `true` (JS-trimmed, lower-cased) and the decoded first
//!   `<NextContinuationToken>`.
//!
//! Inputs past the caps below are refused with a typed [`Error`]; noevia-core's
//! `S3_PARSE_IMPL=wasm` path fails closed on any error.
#![forbid(unsafe_code)]

use dav_parse::{decode_xml_entities, element_texts, js_trim, push_json_string};
use std::fmt;

/// noevia-core reads at most 4 MiB of a listing response (`LIST_BODY_CAP`). Invalid UTF-8 there
/// decodes to U+FFFD (3 bytes for 1), so the decoded body can reach 3 x 4 MiB; anything larger did
/// not come from that read and is refused.
pub const MAX_BODY_BYTES: usize = 3 * 4 * 1024 * 1024;
/// Largest query prefix accepted, in bytes.
pub const MAX_PREFIX_BYTES: usize = 64 * 1024;
/// Most `<CommonPrefixes>` or `<Contents>` blocks scanned in one page. A 4 MiB body holds fewer
/// than 200 000 of the smallest block, so this never refuses a body Node would accept.
pub const MAX_BLOCKS: usize = 200_000;

/// Why a page was refused. Inside the caps a hostile body never fails (unusable blocks are
/// dropped, as the JS does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The body is larger than [`MAX_BODY_BYTES`].
    BodyTooLarge { len: usize, max: usize },
    /// The query prefix is larger than [`MAX_PREFIX_BYTES`].
    PrefixTooLong { len: usize, max: usize },
    /// More than [`MAX_BLOCKS`] blocks of one kind.
    TooManyEntries { max: usize },
}

impl Error {
    /// A stable machine-readable code, used across the WebAssembly boundary.
    pub fn code(&self) -> &'static str {
        match self {
            Error::BodyTooLarge { .. } => "body_too_large",
            Error::PrefixTooLong { .. } => "prefix_too_long",
            Error::TooManyEntries { .. } => "too_many_entries",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BodyTooLarge { len, max } => {
                write!(f, "listing body is {len} bytes (max {max})")
            }
            Error::PrefixTooLong { len, max } => {
                write!(f, "query prefix is {len} bytes (max {max})")
            }
            Error::TooManyEntries { max } => {
                write!(f, "listing has more than {max} entries of one kind")
            }
        }
    }
}

impl std::error::Error for Error {}

/// One listed child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The decoded name relative to the query prefix (never empty).
    pub name: String,
    /// From `<CommonPrefixes>` (a directory) rather than `<Contents>`.
    pub is_dir: bool,
    /// The ASCII digits of `<Size>`, unparsed (the caller turns them into a Number).
    pub size: Option<String>,
}

/// One parsed ListObjectsV2 page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// Directories first, then files, each in body order.
    pub entries: Vec<Entry>,
    /// `<IsTruncated>` says `true`.
    pub truncated: bool,
    /// The decoded `<NextContinuationToken>`, when present.
    pub next: Option<String>,
}

fn first<'a>(body: &'a str, name: &str) -> Option<&'a str> {
    element_texts(body, name, 1).into_iter().next()
}

fn relative<'a>(full: &'a str, query_prefix: &str) -> &'a str {
    if query_prefix.is_empty() {
        return full;
    }
    full.strip_prefix(query_prefix).unwrap_or(full)
}

/// Parse one page listed with `prefix=query_prefix&delimiter=/`.
pub fn parse_page(body: &str, query_prefix: &str) -> Result<Page, Error> {
    if body.len() > MAX_BODY_BYTES {
        return Err(Error::BodyTooLarge {
            len: body.len(),
            max: MAX_BODY_BYTES,
        });
    }
    if query_prefix.len() > MAX_PREFIX_BYTES {
        return Err(Error::PrefixTooLong {
            len: query_prefix.len(),
            max: MAX_PREFIX_BYTES,
        });
    }
    let mut entries = Vec::new();
    let prefixes = element_texts(body, "CommonPrefixes", MAX_BLOCKS + 1);
    if prefixes.len() > MAX_BLOCKS {
        return Err(Error::TooManyEntries { max: MAX_BLOCKS });
    }
    for block in prefixes {
        let Some(raw) = first(block, "Prefix") else {
            continue;
        };
        let decoded = decode_xml_entities(raw);
        let full = decoded.trim_end_matches('/');
        let rel = relative(full, query_prefix);
        if rel.is_empty() {
            continue;
        }
        entries.push(Entry {
            name: rel.to_owned(),
            is_dir: true,
            size: None,
        });
    }
    let contents = element_texts(body, "Contents", MAX_BLOCKS + 1);
    if contents.len() > MAX_BLOCKS {
        return Err(Error::TooManyEntries { max: MAX_BLOCKS });
    }
    for block in contents {
        let Some(raw) = first(block, "Key") else {
            continue;
        };
        let full = decode_xml_entities(raw);
        if full.ends_with('/') {
            continue;
        }
        let rel = relative(&full, query_prefix);
        if rel.is_empty() || rel.contains('/') {
            continue; // direct children only
        }
        let size = first(block, "Size")
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .map(str::to_owned);
        entries.push(Entry {
            name: rel.to_owned(),
            is_dir: false,
            size,
        });
    }
    let truncated = first(body, "IsTruncated").is_some_and(|t| js_trim(t).to_lowercase() == "true");
    let next = first(body, "NextContinuationToken").map(decode_xml_entities);
    Ok(Page {
        entries,
        truncated,
        next,
    })
}

/// The WebAssembly reply: `{"entries":[{"name":…,"isDir":…,"size":"digits"|null},…],
/// "truncated":bool,"next":"…"|null}` or `{"error":"code"}`.
pub fn reply_json(result: &Result<Page, Error>) -> String {
    let mut out = String::new();
    match result {
        Err(e) => {
            out.push_str("{\"error\":");
            push_json_string(&mut out, e.code());
            out.push('}');
        }
        Ok(page) => {
            out.push_str("{\"entries\":[");
            for (n, e) in page.entries.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                out.push_str("{\"name\":");
                push_json_string(&mut out, &e.name);
                out.push_str(if e.is_dir {
                    ",\"isDir\":true,\"size\":"
                } else {
                    ",\"isDir\":false,\"size\":"
                });
                match &e.size {
                    Some(d) => push_json_string(&mut out, d),
                    None => out.push_str("null"),
                }
                out.push('}');
            }
            out.push_str(if page.truncated {
                "],\"truncated\":true,\"next\":"
            } else {
                "],\"truncated\":false,\"next\":"
            });
            match &page.next {
                Some(t) => push_json_string(&mut out, t),
                None => out.push_str("null"),
            }
            out.push('}');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page() {
        let body = "<ListBucketResult><IsTruncated> TRUE </IsTruncated><NextContinuationToken>t&amp;1</NextContinuationToken>\
            <Contents><Key>p/a&amp;b.md</Key><Size>42</Size></Contents><Contents><Key>p/sub/x</Key></Contents>\
            <Contents><Key>p/dir/</Key></Contents><CommonPrefixes><Prefix>p/Sub/</Prefix></CommonPrefixes></ListBucketResult>";
        let page = parse_page(body, "p/").unwrap_or_else(|_| Page {
            entries: vec![],
            truncated: false,
            next: None,
        });
        assert_eq!(
            reply_json(&Ok(page)),
            r#"{"entries":[{"name":"Sub","isDir":true,"size":null},{"name":"a&b.md","isDir":false,"size":"42"}],"truncated":true,"next":"t&1"}"#
        );
        assert_eq!(
            reply_json(&parse_page("", &"p".repeat(MAX_PREFIX_BYTES + 1))),
            r#"{"error":"prefix_too_long"}"#
        );
    }
}
