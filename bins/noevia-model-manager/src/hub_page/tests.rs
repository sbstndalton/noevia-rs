//! The one set of bound tests for the Hub page limits (noevia#2000) and the measurement of real
//! Hub pages against them (noevia#1999).
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::measure::{
    filled, fixture_pages, heap, longest_string, nodes, read_page, tree_bound, GGUF_LARGE, MIB,
    PICKLE_IMPORTS,
};
use super::*;

/// noevia#1985, noevia#1996: hostile pages at the cap are refused as a failed request with the
/// read buffer never past the cap, a page one byte over is not read, and the largest trees the
/// limits admit (each shape) stay under `tree_bound()` (68 MiB): with the page being parsed,
/// at most 84 MiB. The parse stops at the first node or character over a limit, so what a
/// refused page allocated is a prefix of a page the limits admit. A 64 MiB page of bare numbers
/// was about 2 GiB.
#[tokio::test]
async fn hostile_pages_are_refused_within_a_bound() {
    let cap = PAGE_CAP;
    // Escaped non-ASCII text (`é` is 6 page bytes for one character) and raw 4-byte text.
    let hostile = [
        ("bare numbers", filled("[1", ",1", "]", cap)),
        ("deep nesting", filled("", "[", "", cap)),
        ("escaped text", filled("[\"", "\\u00e9", "\"]", cap)),
        ("raw 4-byte text", filled("[\"", "\u{1F600}", "\"]", cap)),
        ("object keys", filled("{\"k\": 1", ", \"k\": 1", "}", cap)),
        ("empty arrays", filled("[[]", ", []", "]", cap)),
        ("empty objects", filled("[{}", ", {}", "]", cap)),
        (
            "file entries",
            filled(
                "[{}",
                r#", {"type": "file", "path": "a.gguf", "lastCommit": {"date": "d"}}"#,
                "]",
                cap,
            ),
        ),
    ];
    for (name, page) in hostile {
        let body = read_page(&page).await.unwrap_or_else(|| panic!("{name}"));
        assert_eq!(body.len(), cap, "{name}");
        assert!(body.capacity() <= cap, "{name}: {}", body.capacity());
        assert!(parse_page(body).is_none(), "{name} was parsed");
    }
    // A page of exactly the cap is read whole and accepted; one byte more is not read.
    let full = read_page(&filled("[", " ", "]", cap)).await.unwrap();
    assert_eq!(full.len(), cap);
    assert!(full.capacity() <= cap);
    assert_eq!(parse_page(full), Some(Value::Arr(Vec::new())));
    let mut over = filled("[", " ", "]", cap);
    over.push(b' ');
    assert!(read_page(&over).await.is_none());

    let bound = tree_bound();
    assert!(bound + cap <= 84 * MIB, "{bound}");
    let largest = [
        ("bare numbers", filled("[1", ",1", "]", 2 * PAGE_NODES - 1)),
        (
            "object keys",
            format!("{{{}}}", vec!["\"k\": 1"; PAGE_NODES / 2 - 1].join(", ")).into_bytes(),
        ),
        // noevia#1995: nested single-element arrays, the most heap per node (each level a
        // `Vec` of 4 values), 501 levels deep.
        (
            "nested arrays",
            filled(
                "[",
                &format!("{}1{}, ", "[".repeat(500), "]".repeat(500)),
                "1]",
                1003 * (PAGE_NODES / 501 - 1) + 3,
            ),
        ),
        // Strings one character past a power of two: the most doubling slack.
        (
            "long strings",
            filled(
                "[\"\"",
                &format!(", \"{}\"", "\u{1F600}".repeat(8 * 1024 + 1)),
                "]",
                cap,
            ),
        ),
    ];
    for (name, page) in largest {
        let body = read_page(&page).await.unwrap_or_else(|| panic!("{name}"));
        assert!(body.capacity() <= cap, "{name}");
        let tree = parse_page(body).unwrap_or_else(|| panic!("{name}"));
        let held = heap(&tree);
        assert!(held <= bound, "{name}: {held} > {bound}");
        eprintln!(
            "{name}: tree {:.1} MiB (bound {:.1} MiB)",
            held as f64 / MIB as f64,
            bound as f64 / MIB as f64
        );
    }
    // One node or one character more is refused.
    let numbers = filled("[1", ",1", "]", 2 * PAGE_NODES + 1);
    assert!(parse_page(numbers).is_none());
    let long = format!("[\"{}\"]", "a".repeat(PAGE_STRING_CHARS + 1)).into_bytes();
    assert!(parse_page(long).is_none());
    let longest = format!("[\"{}\"]", "a".repeat(PAGE_STRING_CHARS)).into_bytes();
    assert!(parse_page(longest).is_some());
    // Nesting: 512 levels parse, 513 do not.
    let nested = |depth: usize| format!("{}{}", "[".repeat(depth), "]".repeat(depth)).into_bytes();
    assert!(parse_page(nested(JSON_DEPTH)).is_some());
    assert!(parse_page(nested(JSON_DEPTH + 1)).is_none());
    // A UTF-16 page (decoded, the raw bytes dropped before the parse) still reads.
    let utf16: Vec<u8> = "[1, \"x\"]"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    assert_eq!(
        parse_page(utf16),
        Some(Value::Arr(vec![
            Value::Int(model_files::BigInt::from_u64(1)),
            Value::Str("x".into())
        ]))
    );
}

/// noevia#2013: a full-size page, exactly `PAGE_CAP` bytes of real content (not only blanks), is
/// read whole and accepted when it is within the node and string limits.
#[tokio::test]
async fn a_full_size_page_is_accepted() {
    let cap = PAGE_CAP;
    let unit = format!(", \"{}\"", "a".repeat(60_000));
    let page = filled("[\"a\"", &unit, "]", cap);
    assert_eq!(page.len(), cap);
    let body = read_page(&page).await.unwrap();
    assert_eq!(body.len(), cap);
    assert!(body.capacity() <= cap);
    let Some(Value::Arr(items)) = parse_page(body) else {
        panic!("a full-size page was refused");
    };
    assert!(items.len() > 200);
    // One byte less is accepted too.
    let body = read_page(&filled("[", " ", "]", cap - 1)).await.unwrap();
    assert_eq!(parse_page(body), Some(Value::Arr(Vec::new())));
}

/// The reader refuses a failed stream and a page past the cap, whatever the chunking.
#[tokio::test]
async fn the_reader_stops_at_the_cap_and_at_a_transport_error() {
    type Chunk = Result<bytes::Bytes, std::io::Error>;
    let chunk = |n: usize| -> Chunk { Ok(bytes::Bytes::from(vec![b' '; n])) };
    let exact = read_capped(futures_util::stream::iter([chunk(10), chunk(6)]), 16).await;
    assert_eq!(exact.map(|b| b.len()), Some(16));
    let over = read_capped(futures_util::stream::iter([chunk(10), chunk(7)]), 16).await;
    assert!(over.is_none());
    let failed: [Chunk; 2] = [chunk(1), Err(std::io::Error::other("reset"))];
    assert!(read_capped(futures_util::stream::iter(failed), 16)
        .await
        .is_none());
    let empty = read_capped(futures_util::stream::iter(Vec::<Chunk>::new()), 16).await;
    assert_eq!(empty, Some(Vec::new()));
}

/// noevia#1999: real `expand=true` pages (tests/fixtures/hub-tree/README.md) read through the
/// capped reader and parsed under the shared limits, with the nodes and the longest string
/// counted as the parser counts them. The estimate behind `PAGE_NODES` was 30-60 nodes per entry;
/// measured, it is about 40 to 310 (a pickle file's `pickleImports` list adds 7 nodes per
/// import). Even the worst measured entry would need hundreds of entries on one page to reach the
/// node cap, where the Hub sends 50.
#[tokio::test]
async fn real_hub_pages_stay_far_below_the_limits() {
    for (name, fixture) in [("gguf", GGUF_LARGE), ("pickle", PICKLE_IMPORTS)] {
        let (mut entries, mut total, mut page_nodes, mut string) = (0, 0, 0, 0);
        let mut entry_nodes = (usize::MAX, 0);
        let mut peak_tree = 0;
        for page in fixture_pages(fixture) {
            let text = String::from_utf8(page.clone()).unwrap();
            let body = read_page(&page).await.unwrap();
            let tree = parse_page(body).unwrap_or_else(|| panic!("{name}: a real page is refused"));
            let Value::Arr(items) = &tree else {
                panic!("{name}: not a listing")
            };
            // The counts are the parser's own: the page parses at exactly its count and not at
            // one node or one character fewer.
            let (n, longest) = (nodes(&tree), longest_string(&tree));
            let at = |nodes: usize, string_chars: usize| {
                model_files::json::parse_with_limits(
                    &text,
                    Limits {
                        nodes,
                        string_chars,
                        ..page_limits()
                    },
                )
                .is_ok()
            };
            assert!(at(n, longest), "{name}");
            assert!(!at(n - 1, longest), "{name}: node count is off");
            assert!(!at(n, longest - 1), "{name}: string length is off");
            for item in items {
                let each = nodes(item);
                entry_nodes = (entry_nodes.0.min(each), entry_nodes.1.max(each));
            }
            entries += items.len();
            total += n;
            page_nodes = page_nodes.max(n);
            string = string.max(longest);
            peak_tree = peak_tree.max(heap(&tree));
            assert!(items.len() <= 50, "{name}: the Hub pages by 50");
        }
        eprintln!(
            "{name}: {entries} entries, {:.1} nodes per entry (min {}, max {}), \
             largest page {page_nodes} nodes, longest string {string} chars, \
             largest tree {:.2} MiB",
            total as f64 / entries as f64,
            entry_nodes.0,
            entry_nodes.1,
            peak_tree as f64 / MIB as f64
        );
        // A wide margin: 20 times the busiest real page fits the node cap, and 100 times the
        // longest real string fits the string cap.
        assert!(page_nodes * 20 <= PAGE_NODES, "{name}: {page_nodes}");
        assert!(string * 100 <= PAGE_STRING_CHARS, "{name}: {string}");
        // At the worst entry, a page needs this many entries to reach the cap (the Hub's is 50).
        assert!(PAGE_NODES / entry_nodes.1 >= 8 * 50, "{name}");
    }
}
