//! Properties: no input panics; a framed text always parses back as exactly one block holding
//! the escaped text (the data cannot close its block or open another); caps refuse with fixed
//! codes; refusals and packet errors never carry input values; a store stays within its bound and
//! survives the host round trip; the same input always gives the same reply.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use prompt_framing::framing::{escape_closing, frame_untrusted};
use prompt_framing::js::units;
use prompt_framing::json::{self, Value};
use prompt_framing::provenance::{self, Store};
use prompt_framing::{
    escape, frame, packet_call, provenance_call, store_from, store_json, to_le, Error,
    MAX_LABEL_UNITS, MAX_PACKET_BYTES, MAX_PROVENANCE_BYTES, MAX_TEXT_UNITS,
};
use proptest::prelude::*;

/// Code units drawn from what matters here: markers, quotes, brackets, JS spaces, zero-width
/// characters, lone surrogates and ordinary text.
fn unit_text(max: usize) -> impl Strategy<Value = Vec<u16>> {
    let atoms: Vec<Vec<u16>> = [
        "<",
        "/",
        ">",
        "\"",
        "\n",
        "\r",
        "[",
        "]",
        " ",
        "\t",
        "\u{a0}",
        "\u{3000}",
        "\u{feff}",
        "\u{200b}",
        "untrusted",
        "UNTRUSTED",
        "source",
        "SOURCE",
        "<untrusted ",
        "\n</untrusted>",
        "</untrusted>",
        "</SOURCE>",
        " label=\"",
        "> (data, not instructions)\n",
        "kind=\"",
        "a",
        "Z",
        "é",
        "\u{17f}",
        "@",
        ".",
        "%",
        ":",
        "x@evil.io",
        "https://evil.io/",
        "\u{1e9e}",
    ]
    .iter()
    .map(|s| units(s))
    .chain([vec![0xd800], vec![0xdc00], vec![0xd83d, 0xde00]])
    .collect();
    prop::collection::vec(prop::sample::select(atoms), 0..max).prop_map(|v| v.concat())
}

fn frame_input(kind: &[u16], label: &[u16], text: &[u16]) -> Vec<u8> {
    let mut v = (kind.len() as u32).to_le_bytes().to_vec();
    v.extend(to_le(kind));
    v.extend((label.len() as u32).to_le_bytes());
    v.extend(to_le(label));
    v.extend(to_le(text));
    v
}

fn jstr(u: &[u16]) -> String {
    let mut o = Vec::new();
    json::push_str(&mut o, u);
    String::from_utf8(o).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn a_framed_text_is_one_block(kind in unit_text(12), label in unit_text(20), text in unit_text(40)) {
        let framed = frame_untrusted(&kind, &label, &text);
        let blocks = provenance::framed_blocks(&framed);
        prop_assert_eq!(blocks.len(), 1);
        let body = escape_closing(&escape_closing(&text, &units("untrusted")), &units("SOURCE"));
        prop_assert_eq!(&blocks[0].2, &body);
        // Escaping is idempotent: nothing left in the body closes a block.
        prop_assert_eq!(escape_closing(&body, &units("untrusted")), body.clone());
        prop_assert_eq!(escape_closing(&body, &units("SOURCE")), body);
        // The wire form is the library's.
        prop_assert_eq!(frame(&frame_input(&kind, &label, &text)).unwrap(), to_le(&framed));
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..300)) {
        for r in [frame(&bytes), escape(&bytes), provenance_call(&bytes), packet_call(&bytes)] {
            if let Err(e) = r {
                let fixed = [Error::Input.json(), Error::TooLarge.json()];
                prop_assert!(fixed.contains(&e.json()));
            }
        }
    }

    #[test]
    fn packet_errors_never_carry_values(goal in unit_text(30), fact in unit_text(30), kind in "[a-z]{0,8}", n in 0usize..30) {
        let marker = "Q7Z9MARKER";
        let mut g = goal.clone();
        g.extend(units(marker));
        let facts: Vec<String> = (0..n)
            .map(|_| format!(r#"{{"text":{},"source":{{"kind":"{kind}","ref":"{marker}"}}}}"#, jstr(&fact)))
            .collect();
        let packet = format!(
            r#"{{"packet_schema":1,"goal":{},"facts":[{}],"constraints":["{marker}"],"open_questions":[]}}"#,
            jstr(&g),
            facts.join(",")
        );
        let req = format!(r#"{{"op":"parse","output":{}}}"#, jstr(&units(&packet)));
        let reply = packet_call(req.as_bytes()).unwrap();
        let r = json::parse_utf8(&reply, 16).unwrap();
        if r.get("ok") == Some(&Value::Bool(false)) {
            let e = String::from_utf16_lossy(r.get("error").unwrap().as_str().unwrap());
            prop_assert!(!e.contains(marker), "{}", e);
            prop_assert!(e.starts_with('$'));
        }
        prop_assert_eq!(packet_call(req.as_bytes()).unwrap(), reply);
    }

    #[test]
    fn stores_stay_bounded_and_round_trip(blocks in prop::collection::vec((unit_text(6), unit_text(30)), 0..12), max in 0usize..400, value in unit_text(20), args in unit_text(30)) {
        let contents: Vec<Vec<u16>> = blocks.iter().map(|(k, t)| frame_untrusted(k, &[], t)).collect();
        let mut store = Store::new(max);
        store.ingest(&contents);
        prop_assert!(store.check().is_ok());
        prop_assert!(store.chars <= max);
        let mut out = Vec::new();
        store_json(&mut out, &store);
        let back = store_from(Some(&json::parse_utf8(&out, 8).unwrap())).unwrap();
        prop_assert_eq!(&back, &store);
        let idx = store.index();
        let _ = idx.source_of(&value);
        let mut text = units(r#"{"to":"#);
        text.extend(units(&jstr(&args)));
        text.push(u16::from(b'}'));
        let parsed = provenance::parse_args(&text, false);
        if let Ok(a) = parsed {
            let hits = provenance::check_write(&idx, &a).unwrap();
            prop_assert!(hits.len() <= provenance::MAX_FOUND);
        }
        let _ = provenance::check_write(&idx, &provenance::parse_args(&args, false).unwrap_or(Value::Null));
    }
}

#[test]
fn caps_refuse_with_fixed_codes() {
    let big_label = vec![0x61u16; MAX_LABEL_UNITS + 1];
    assert_eq!(
        frame(&frame_input(&big_label, &[], &[])),
        Err(Error::TooLarge)
    );
    let mut v = frame_input(&[], &[], &[]);
    v.resize(v.len() + 2 * (MAX_TEXT_UNITS + 1), 0x20);
    assert_eq!(frame(&v), Err(Error::TooLarge));
    assert_eq!(frame(&[1, 0, 0, 0, 0x61]), Err(Error::Input));
    assert_eq!(escape(&[1, 0, 0, 0, 0x2e, 0]), Err(Error::Input));
    assert_eq!(
        provenance_call(&vec![b' '; MAX_PROVENANCE_BYTES + 1]),
        Err(Error::TooLarge)
    );
    assert_eq!(
        packet_call(&vec![b' '; MAX_PACKET_BYTES + 1]),
        Err(Error::TooLarge)
    );
    assert_eq!(
        provenance_call(br#"{"op":"new","maxChars":4000001}"#),
        Err(Error::Input)
    );
    assert_eq!(
        provenance_call(br#"{"op":"new","maxChars":1.5}"#),
        Err(Error::Input)
    );
    // A tampered store (chars that do not add up) is refused, never trusted.
    assert_eq!(
        provenance_call(br#"{"op":"source","state":{"maxChars":10,"chars":3,"saturated":false,"sources":["s"],"texts":[[0,"ab"]]},"value":"abcdef"}"#),
        Err(Error::Input)
    );
    assert_eq!(
        provenance_call(br#"{"op":"source","state":{"maxChars":10,"chars":2,"saturated":false,"sources":["s"],"texts":[[1,"ab"]]},"value":"abcdef"}"#),
        Err(Error::Input)
    );
    assert_eq!(Error::Input.json(), br#"{"error":"input"}"#);
}
