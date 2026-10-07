//! Property tests (noevia#981): arbitrary and mutated input never panics, the text budget
//! holds, and every refusal is one of the known classes. Agreement with Python is
//! differential.rs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use docx_text::{extract_docx, TEXT_LIMIT};
use proptest::prelude::*;

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

/// A minimal stored (uncompressed) ZIP with the given members, built by hand.
fn zip(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cd = Vec::new();
    for (name, data) in members {
        let off = out.len() as u32;
        let crc = crc32(data);
        let n = name.len() as u16;
        let sz = data.len() as u32;
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&sz.to_le_bytes());
        out.extend_from_slice(&sz.to_le_bytes());
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        cd.extend_from_slice(b"PK\x01\x02");
        cd.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        cd.extend_from_slice(&crc.to_le_bytes());
        cd.extend_from_slice(&sz.to_le_bytes());
        cd.extend_from_slice(&sz.to_le_bytes());
        cd.extend_from_slice(&n.to_le_bytes());
        cd.extend_from_slice(&[0; 12]);
        cd.extend_from_slice(&off.to_le_bytes());
        cd.extend_from_slice(name.as_bytes());
    }
    let cd_off = out.len() as u32;
    out.extend_from_slice(&cd);
    out.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    let count = members.len() as u16;
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_off.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn docx(body: &str) -> Vec<u8> {
    let xml = format!("<w:document xmlns:w=\"{W}\"><w:body>{body}</w:body></w:document>");
    zip(&[("word/document.xml", xml.as_bytes())])
}

const CLASSES: &[&str] = &[
    "input",
    "container",
    "limits",
    "members",
    "encrypted_or_oversized",
    "missing",
    "decompression",
    "xml_limit",
    "xml_declarations",
    "xml_malformed",
    "namespace",
    "body",
    "nesting",
];

fn check(data: &[u8]) {
    match extract_docx(data) {
        Ok(e) => assert!(e.text.chars().count() <= TEXT_LIMIT),
        Err(r) => assert!(CLASSES.contains(&r.class())),
    }
}

/// XML-ish body fragments, mostly well-formed pieces in random order.
fn fragment() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec![
            "<w:p>",
            "</w:p>",
            "<w:r>",
            "</w:r>",
            "<w:t>",
            "</w:t>",
            "<w:tab/>",
            "<w:br/>",
            "<w:tc>",
            "</w:tc>",
            "<w:tr>",
            "</w:tr>",
            "<w:del>",
            "</w:del>",
            "text",
            " ",
            "\r\n",
            "&amp;",
            "&#x1F600;",
            "&bogus;",
            "<![CDATA[x]]>",
            "<!-- c -->",
            "<?p q?>",
            "]]>",
            "\u{a0}",
            "\u{3000}",
            "é",
            "<x:y/>",
            "<w:t xml:space=\"preserve\">",
            "<!DOCTYPE",
            "<",
            "&",
            "\u{1}",
            "\u{fffe}",
        ]),
        0..60,
    )
    .prop_map(|v| v.concat())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1500, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        check(&bytes);
    }

    #[test]
    fn arbitrary_bytes_behind_a_zip_signature_never_panic(
        bytes in prop::collection::vec(any::<u8>(), 0..512),
        tail in prop::collection::vec(any::<u8>(), 18..19),
    ) {
        let mut data = bytes;
        data.extend_from_slice(b"PK\x05\x06");
        data.extend_from_slice(&tail);
        check(&data);
    }

    #[test]
    fn mutated_documents_never_panic(
        body in fragment(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..6),
        cut in any::<prop::sample::Index>(),
        truncate in any::<bool>(),
    ) {
        let mut data = docx(&body);
        for (at, b) in &edits {
            let i = at.index(data.len());
            data[i] = *b;
        }
        if truncate {
            data.truncate(cut.index(data.len() + 1));
        }
        check(&data);
    }

    #[test]
    fn random_xml_bodies_never_panic_and_keep_the_budget(body in fragment()) {
        check(&docx(&body));
    }

    #[test]
    fn text_budget_holds_for_any_split(parts in prop::collection::vec(1usize..90_000, 1..6)) {
        let body: String = parts.iter().map(|&n| format!("<w:p><w:r><w:t>{}</w:t></w:r></w:p>", "x".repeat(n))).collect();
        let e = extract_docx(&docx(&body)).unwrap();
        // Python's walk, by hand: body > p > r > t, an emit per t and a newline per p.
        let (mut size, mut truncated) = (0usize, false);
        let emit = |len: usize, size: &mut usize, truncated: &mut bool| {
            let remaining = TEXT_LIMIT - *size;
            *truncated |= len > remaining;
            *size += remaining.min(len);
        };
        for &n in &parts {
            if size >= TEXT_LIMIT { truncated = true; continue; }
            if size >= TEXT_LIMIT { truncated = true; } else { emit(n, &mut size, &mut truncated); }
            emit(1, &mut size, &mut truncated);
        }
        prop_assert!(e.text.chars().count() <= TEXT_LIMIT);
        prop_assert_eq!(e.truncated, truncated);
        // The text is what was emitted, minus the stripped trailing newline (if it fit).
        prop_assert!(e.text.chars().count() + 1 >= size && e.text.chars().count() <= size);
    }
}

#[test]
fn caps_are_pythons() {
    assert_eq!(docx_text::MAX_MEMBERS, 1000);
    assert_eq!(docx_text::MAX_TOTAL_UNCOMPRESSED, 64 * 1024 * 1024);
    assert_eq!(docx_text::XML_LIMIT, 8 * 1024 * 1024);
    assert_eq!(docx_text::TEXT_LIMIT, 200_000);
}

#[test]
fn oversized_input_is_refused_before_parsing() {
    let data = vec![0u8; docx_text::MAX_INPUT_BYTES + 1];
    assert_eq!(extract_docx(&data).unwrap_err().class(), "input");
}

#[test]
fn deep_nesting_is_refused_not_overflowed() {
    let body = format!("{}{}", "<w:r>".repeat(100_000), "</w:r>".repeat(100_000));
    assert_eq!(extract_docx(&docx(&body)).unwrap_err().class(), "nesting");
}

#[test]
fn many_attributes_stay_linear() {
    let attrs: String = (0..100_000).map(|i| format!(" a{i}=\"v\"")).collect();
    let body = format!("<w:p{attrs}><w:r><w:t>ok</w:t></w:r></w:p>");
    assert_eq!(extract_docx(&docx(&body)).unwrap().text, "ok");
}
