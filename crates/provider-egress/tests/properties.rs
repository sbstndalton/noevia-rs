//! Properties of the provider-egress port: no input panics; the linear DAV-URL matcher agrees with
//! a brute-force backtracking reading of the JS regexes; a storage call is let through only when
//! every path argument is known and outside the Diary folder (never more than the JS lets out:
//! the JS refuses exactly the known in-folder paths, and the port also refuses every unknown one);
//! unknown providers count as external; and the inputs that make the JS regexes quadratic
//! (noevia#1209) stay linear here. The JS side of "never more than the JS" runs against the real
//! JS in noevia-core's differential test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use prompt_framing::js::{to_lower, units};
use prompt_framing::json::Value;
use proptest::prelude::*;
use provider_egress::{
    call, canonical_path, diary_folder, external, external_strict, tool_refusal, Folder, Provider,
    Storage,
};
use std::time::{Duration, Instant};

// ── Brute-force reference of canonicalPath's regex ──────────────────────────

fn ci(s: &[u16], at: usize, lit: &str) -> bool {
    let f = |c: u16| {
        if (0x41..=0x5a).contains(&c) {
            c + 32
        } else {
            c
        }
    };
    lit.bytes()
        .enumerate()
        .all(|(k, b)| s.get(at + k).is_some_and(|&c| f(c) == f(u16::from(b))))
}
const LT: [u16; 4] = [0x0a, 0x0d, 0x2028, 0x2029];

/// `(?:dav\/files\/[^/]+|webdav)(\/.*)?$` at q, backtracking every quantifier in order.
fn tail_ref(s: &[u16], q: usize) -> Option<Option<usize>> {
    let n = s.len();
    let mut ends = Vec::new();
    if ci(s, q, "dav/files/") {
        let r = q + 10;
        let mut e = r;
        while e < n && s[e] != b'/' as u16 {
            e += 1;
        }
        // [^/]+ greedy: longest first.
        for k in (r + 1..=e).rev() {
            ends.push(k);
        }
    } else if ci(s, q, "webdav") {
        ends.push(q + 6);
    }
    for e in ends {
        // (\/.*)? greedy: with the group, `.*` longest first; then without.
        if s.get(e) == Some(&(b'/' as u16)) {
            let mut m = e + 1;
            while m < n && !LT.contains(&s[m]) {
                m += 1;
            }
            for k in (e + 1..=m).rev() {
                if k == n {
                    return Some(Some(e));
                }
            }
        }
        if e == n {
            return Some(None);
        }
    }
    None
}

fn at_ref(s: &[u16], p: usize) -> Option<Option<usize>> {
    for skip in [1usize, 0] {
        if skip == 1 && s.get(p) != Some(&(b'/' as u16)) {
            continue;
        }
        if ci(s, p + skip, "remote.php/") {
            if let Some(g) = tail_ref(s, p + skip + 11) {
                return Some(g);
            }
        }
    }
    None
}

fn full_ref(s: &[u16]) -> Option<Option<usize>> {
    let n = s.len();
    let letter = |c: u16| (c | 0x20) >= b'a' as u16 && (c | 0x20) <= b'z' as u16 && c < 0x80;
    if s.first().is_some_and(|&c| letter(c)) {
        let mut i = 1;
        while i < n
            && (letter(s[i])
                || (b'0' as u16..=b'9' as u16).contains(&s[i])
                || b"+.-".iter().any(|&b| s[i] == u16::from(b)))
        {
            i += 1;
        }
        if ci(s, i, "://") {
            let h0 = i + 3;
            let mut h1 = h0;
            while h1 < n && s[h1] != b'/' as u16 {
                h1 += 1;
            }
            for k in (h0..=h1).rev() {
                if let Some(g) = at_ref(s, k) {
                    return Some(g);
                }
            }
        }
    }
    (0..=n).find_map(|p| at_ref(s, p))
}

/// canonicalPath on NFC-inert ASCII-only text (decoding and lowercasing are then plain), with the
/// brute-force regex.
fn canonical_ref(v: &[u16]) -> Vec<u16> {
    let mut text = v.to_vec();
    for _ in 0..3 {
        let has = (0..text.len()).any(|i| {
            text[i] == b'%' as u16
                && text
                    .get(i + 1)
                    .is_some_and(|&c| c < 0x80 && (c as u8).is_ascii_hexdigit())
                && text
                    .get(i + 2)
                    .is_some_and(|&c| c < 0x80 && (c as u8).is_ascii_hexdigit())
        });
        if !has {
            break;
        }
        match prompt_framing::js::decode_uri_component(&text) {
            Some(t) => text = t,
            None => break,
        }
    }
    for c in &mut text {
        if *c == b'\\' as u16 {
            *c = b'/' as u16;
        }
    }
    let rest: Vec<u16> = match full_ref(&text) {
        Some(Some(g)) => text[g..].to_vec(),
        Some(None) => Vec::new(),
        None => {
            // ^[a-z][a-z0-9+.-]*:\/\/[^/]*
            let s = &text;
            let letter = |c: u16| c < 0x80 && (c as u8).is_ascii_alphabetic();
            let mut out = s.clone();
            if s.first().is_some_and(|&c| letter(c)) {
                let mut i = 1;
                while i < s.len()
                    && s[i] < 0x80
                    && ((s[i] as u8).is_ascii_alphanumeric() || b"+.-".contains(&(s[i] as u8)))
                {
                    i += 1;
                }
                if ci(s, i, "://") {
                    let mut h = i + 3;
                    while h < s.len() && s[h] != b'/' as u16 {
                        h += 1;
                    }
                    out = s[h..].to_vec();
                }
            }
            out
        }
    };
    let mut out: Vec<&[u16]> = Vec::new();
    for seg in rest.split(|&c| c == b'/' as u16) {
        if seg.is_empty() || seg == [b'.' as u16] {
            continue;
        }
        if seg == [b'.' as u16, b'.' as u16] {
            out.pop();
            continue;
        }
        out.push(seg);
    }
    to_lower(&out.join(&(b'/' as u16)))
}

const PIECES: &[&str] = &[
    "/",
    "//",
    "remote.php",
    "REMOTE.PHP/",
    "remote.php/",
    "dav/files/",
    "webdav",
    "WebDav/",
    "https://",
    "h",
    "x",
    "u",
    "\n",
    "\r",
    "\u{2028}",
    "a+b-c.d:",
    "://",
    "..",
    ".",
    "\\",
    "%2F",
    "%25",
    "%41",
    "Diary",
    "%",
];

fn ascii_path() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(PIECES), 0..9).prop_map(|p| p.concat())
}

const SEGS: &[&str] = &[
    "Diary",
    "diary",
    "DIARY",
    "Work",
    "..",
    ".",
    "",
    "%2F",
    "%2e%2e",
    "%2544",
    "Tagebücher",
    "Ημερ",
    "Cafe\u{301}",
    "remote.php",
    "webdav",
    "https:",
    "日記",
    "\u{3a3}",
    "x\u{2028}y",
];

fn seg_path() -> impl Strategy<Value = String> {
    (
        prop::collection::vec(prop::sample::select(SEGS), 0..6),
        prop::sample::select(&["/", "\\", "//"][..]),
    )
        .prop_map(|(s, sep)| s.join(sep))
}

fn chatgpt() -> Provider {
    Provider {
        kind: Some(units("chatgpt-oauth")),
        label: units("ChatGPT"),
        ..Provider::default()
    }
}

fn storage(root: &str) -> Storage {
    Storage {
        kind: Some(units("nextcloud")),
        corpus_root: units(root),
        base_url: units("https://nc.example/remote.php/dav/files/alice"),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    fn random_bytes_never_panic(op in 0u8..8, body in prop::collection::vec(any::<u8>(), 0..256)) {
        let mut input = vec![op];
        input.extend(body);
        let (status, reply) = call(&input);
        prop_assert!(status <= 1 && !reply.is_empty());
    }

    #[test]
    fn linear_matcher_is_the_regex(p in ascii_path()) {
        let u = units(&p);
        if p.contains('\u{2028}') {
            // Outside the NFC-inert table: unknown, never a guess.
            prop_assert_eq!(canonical_path(&u), None);
        } else {
            prop_assert_eq!(canonical_path(&u), Some(canonical_ref(&u)), "{:?}", p);
        }
    }

    #[test]
    fn storage_calls_pass_only_known_paths_outside_the_diary(
        paths in prop::collection::vec(seg_path(), 1..4),
        root in prop::sample::select(&["Diary", "Work/Diary", "Tagebücher", "Ημερολόγιο", " Diary "][..]),
        tree in any::<bool>(),
    ) {
        let st = storage(root);
        let name = if tree { "nc_webdav_search_files" } else { "nc_webdav_read_file" };
        let args = Value::Obj(vec![(units("paths"), Value::Arr(paths.iter().map(|p| Value::Str(units(p))).collect()))]);
        let r = tool_refusal(Some(&chatgpt()), &units(name), &args, Some(&st));
        if r.is_none() {
            let Folder::Known(folder) = diary_folder(Some(&st)) else {
                return Err(TestCaseError::fail("let through with an unknown folder"));
            };
            for p in &paths {
                let c = canonical_path(&units(p));
                prop_assert!(c.is_some(), "unknown path let through: {:?}", p);
                let c = c.unwrap();
                let mut fs = folder.clone();
                fs.push(b'/' as u16);
                prop_assert!(c != folder && !c.starts_with(&fs), "{:?}", p);
                if tree {
                    let mut cs = c.clone();
                    cs.push(b'/' as u16);
                    prop_assert!(!c.is_empty() && !folder.starts_with(&cs), "{:?}", p);
                }
            }
        }
        // A local provider is never refused here.
        prop_assert!(tool_refusal(None, &units(name), &args, Some(&st)).is_none());
    }

    #[test]
    fn unknown_providers_are_external(url in "[ -~]{0,40}", wide in any::<bool>()) {
        let base = if wide { format!("https://\u{ff4e}{url}") } else { url.clone() };
        let p = Provider { base_url: units(&base), ..Provider::default() };
        prop_assert_eq!(external_strict(Some(&p)), external(Some(&p)).unwrap_or(true));
        if wide || url.contains('%') {
            prop_assert_eq!(external(Some(&p)), None);
        }
        let mut c = chatgpt();
        c.base_url = units(&base);
        prop_assert_eq!(external(Some(&c)), Some(true));
    }
}

#[test]
fn quadratic_js_inputs_stay_linear() {
    // noevia#1209: the JS takes ~3 s on 320 KB of this; the port must stay linear at 4 MB.
    let t = Instant::now();
    let s = "/remote.php/webdav/x".repeat(200_000) + "\n";
    // No DAV match (the line terminator), so only the segments remain.
    assert!(canonical_path(&units(&s))
        .unwrap()
        .starts_with(&units("remote.php/webdav/x/")));
    let p = Provider {
        base_url: units(&format!("http://a{}b/", ".".repeat(4_000_000))),
        ..Provider::default()
    };
    let _ = external(Some(&p));
    let deep = format!("{}{}", "[".repeat(200_000), "]".repeat(200_000));
    let mut input = vec![4u8];
    input.extend(
        format!(
            r#"[{{"kind":"chatgpt-oauth","external":false,"baseUrl":"","label":""}},"nc_webdav_search_files",{},{{"kind":"nextcloud","corpusRoot":"Diary","baseUrl":""}}]"#,
            serde_like(&deep)
        )
        .as_bytes(),
    );
    assert_eq!(call(&input).0, 0);
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
}

fn serde_like(s: &str) -> String {
    let mut out = Vec::new();
    prompt_framing::json::push_str(&mut out, &units(s));
    String::from_utf8(out).unwrap()
}
