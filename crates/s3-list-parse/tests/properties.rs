//! Property tests and caps for s3-list-parse (noevia#976).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use proptest::prelude::*;
use s3_list_parse::{parse_page, Error, MAX_BLOCKS, MAX_BODY_BYTES, MAX_PREFIX_BYTES};

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn never_panics_on_arbitrary_input(body in any::<String>(), prefix in any::<String>()) {
        let _ = parse_page(&body, &prefix);
        let _ = parse_page(&body, "docs/");
    }

    #[test]
    fn never_panics_on_markup_like_input(body in r#"(<|</|>|s3:|Contents|CommonPrefixes|Key|Prefix|Size|IsTruncated|NextContinuationToken|&|#|;|x|/|docs/|[0-9]| ){0,200}"#) {
        if let Ok(page) = parse_page(&body, "docs/") {
            for e in page.entries {
                prop_assert!(!e.name.is_empty());
                if !e.is_dir { prop_assert!(!e.name.contains('/')); }
                if let Some(s) = e.size { prop_assert!(!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())); }
            }
        }
    }

    // Escaped keys under the prefix list by their name, files after directories, sizes kept.
    #[test]
    fn escaped_keys_round_trip(
        files in proptest::collection::vec(("[^/]{1,16}", 0u64..u64::MAX), 0..8),
        dirs in proptest::collection::vec("[^/]{1,16}", 0..4),
        token in proptest::option::of("[^<>&]{0,16}"),
    ) {
        let mut body = String::from("<ListBucketResult><IsTruncated>true</IsTruncated>");
        if let Some(t) = &token { body.push_str(&format!("<NextContinuationToken>{}</NextContinuationToken>", esc(t))); }
        for d in &dirs { body.push_str(&format!("<CommonPrefixes><Prefix>docs/{}/</Prefix></CommonPrefixes>", esc(d))); }
        for (f, size) in &files { body.push_str(&format!("<Contents><Key>docs/{}</Key><Size>{size}</Size></Contents>", esc(f))); }
        body.push_str("</ListBucketResult>");
        let page = parse_page(&body, "docs/").unwrap();
        let want: Vec<(String, bool, Option<String>)> = dirs.iter().map(|d| (d.clone(), true, None))
            .chain(files.iter().map(|(f, s)| (f.clone(), false, Some(s.to_string())))).collect();
        let got: Vec<(String, bool, Option<String>)> = page.entries.into_iter().map(|e| (e.name, e.is_dir, e.size)).collect();
        prop_assert_eq!(got, want);
        prop_assert!(page.truncated);
        prop_assert_eq!(page.next, token);
    }

    // A key outside the prefix never lists as a file.
    #[test]
    fn nested_keys_never_list_as_files(parts in proptest::collection::vec("[a-z]{1,4}", 2..5)) {
        let key = format!("docs/{}", parts.join("/"));
        let page = parse_page(&format!("<Contents><Key>{key}</Key></Contents>"), "docs/").unwrap();
        prop_assert!(page.entries.is_empty());
    }
}

#[test]
fn caps_are_typed_errors() {
    let big = "x".repeat(MAX_BODY_BYTES + 1);
    assert!(matches!(
        parse_page(&big, ""),
        Err(Error::BodyTooLarge { .. })
    ));
    assert!(parse_page(&"x".repeat(MAX_BODY_BYTES), "").is_ok());
    let p = "p".repeat(MAX_PREFIX_BYTES + 1);
    assert!(matches!(
        parse_page("", &p),
        Err(Error::PrefixTooLong { .. })
    ));
    let many = "<Contents></Contents>".repeat(MAX_BLOCKS + 1);
    assert_eq!(
        parse_page(&many, ""),
        Err(Error::TooManyEntries { max: MAX_BLOCKS })
    );
    let ok = "<Contents></Contents>".repeat(MAX_BLOCKS);
    assert!(parse_page(&ok, "").is_ok());
}

#[test]
fn hostile_bodies_stay_linear() {
    // 4 MiB of unclosed opening tags: the forward-only scanner stops, it does not rescan.
    let body = "<Contents><Key>".repeat(4 * 1024 * 1024 / 15);
    let start = std::time::Instant::now();
    assert!(parse_page(&body, "docs/").unwrap().entries.is_empty());
    assert!(start.elapsed() < std::time::Duration::from_secs(20));
}
