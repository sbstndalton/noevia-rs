//! Chunking never changes a stream's parts: every recorded stream cut into random pieces.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::JValue;
use proptest::prelude::*;
use signin_transport::events::{parts_of, Part};
use signin_transport::texts::Texts;

fn texts() -> Texts {
    Texts {
        label: "Service".into(),
        disconnected: "d".into(),
        reconnect: "r".into(),
        transient: "t".into(),
        not_included: "n".into(),
        source: "s".into(),
    }
}

fn whole_streams() -> Vec<Vec<u8>> {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/node-transport.v1.json"
    ))
    .unwrap();
    let f = js_json::parse(&text).unwrap();
    let JValue::Obj(streams) = f.get("streams") else {
        panic!()
    };
    streams
        .iter()
        .map(|(_, st)| {
            let JValue::Arr(chunks) = st.get("chunks") else {
                panic!()
            };
            chunks
                .iter()
                .flat_map(|c| b64(c.as_str().unwrap()))
                .collect()
        })
        .collect()
}

fn b64(text: &str) -> Vec<u8> {
    let val = |c: u8| -> u32 {
        u32::from(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            _ => 63,
        })
    };
    let bytes: Vec<u8> = text.bytes().filter(|&c| c != b'=').collect();
    let mut out = Vec::new();
    for chunk in bytes.chunks(4) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &c)| acc | (val(c) << (18 - 6 * i)));
        for i in 0..chunk.len().saturating_sub(1) {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    out
}

fn split<'a>(bytes: &'a [u8], cuts: &[usize]) -> Vec<&'a [u8]> {
    let mut points: Vec<usize> = cuts.iter().map(|c| c % (bytes.len() + 1)).collect();
    points.sort_unstable();
    let mut out = Vec::new();
    let mut at = 0;
    for p in points {
        out.push(&bytes[at..p]);
        at = p;
    }
    out.push(&bytes[at..]);
    out
}

proptest! {
    #[test]
    fn random_chunking_keeps_the_parts(which in 0usize..64, cuts in proptest::collection::vec(0usize..100_000, 0..12)) {
        let streams = whole_streams();
        let bytes = &streams[which % streams.len()];
        let t = texts();
        let want: Vec<Part> = parts_of(&t, 0.0, &[bytes]);
        let got = parts_of(&t, 0.0, &split(bytes, &cuts));
        prop_assert_eq!(got, want);
    }

    #[test]
    fn arbitrary_bytes_never_panic_and_chunking_is_invisible(bytes in proptest::collection::vec(any::<u8>(), 0..400), cuts in proptest::collection::vec(0usize..1000, 0..6)) {
        // Mostly event-stream shaped bytes: prefix a data line so JSON parsing gets exercised too.
        let mut input = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"".to_vec();
        input.extend_from_slice(&bytes);
        let t = texts();
        let want = parts_of(&t, 0.0, &[&input]);
        prop_assert_eq!(parts_of(&t, 0.0, &split(&input, &cuts)), want);
    }
}
