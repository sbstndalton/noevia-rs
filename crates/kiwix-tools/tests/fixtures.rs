//! core `server/kiwix.cjs` against the answers Node gave (`tools/gen-fixtures.cjs` →
//! `tests/fixtures/kiwix-node.v1.json`): the toolbox entry, every base URL, and every tool call, with a
//! `get` that answers from the URL table the Node stub served and records what it was asked for.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use js_json::{wtf16, JValue};

fn fixture() -> JValue {
    wtf16::parse(include_str!("fixtures/kiwix-node.v1.json")).unwrap()
}

fn s(v: &JValue) -> &str {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v:?}"))
}

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        other => panic!("not an array: {other:?}"),
    }
}

#[test]
fn the_toolbox_entry() {
    let f = fixture();
    assert_eq!(kiwix_tools::box_json(), s(f.get("box")));
    assert_eq!(kiwix_tools::ID, s(f.get("id")));
    let names: Vec<&str> = arr(f.get("names")).iter().map(s).collect();
    assert_eq!(names, kiwix_tools::NAMES);
}

#[test]
fn base_urls() {
    let f = fixture();
    let bases = arr(f.get("bases"));
    for b in bases {
        let raw = s(b.get("raw"));
        let got = kiwix_tools::base(raw);
        match b.get("origin") {
            JValue::Str(origin) => assert_eq!(
                got.map(|b| b.origin().to_owned()),
                Ok(origin.clone()),
                "{raw:?}"
            ),
            _ => assert_eq!(
                got.map(|b| b.origin().to_owned()),
                Err(s(b.get("error")).to_owned()),
                "{raw:?}"
            ),
        }
    }
    assert_eq!(bases.len(), 43);
}

#[test]
fn tool_calls() {
    let f = fixture();
    let served = f.get("served");
    let cases = arr(f.get("cases"));
    for c in cases {
        let name = s(c.get("name"));
        let base = kiwix_tools::base(s(c.get("base"))).unwrap();
        let cap = c.get("cap").as_num().unwrap() as usize;
        let mut urls: Vec<String> = Vec::new();
        let mut get = |url: &str| -> Result<String, String> {
            urls.push(url.to_owned());
            let answer = served.get(url);
            if let JValue::Str(reject) = answer.get("reject") {
                return Err(reject.clone());
            }
            let status = answer
                .get("status")
                .as_num()
                .unwrap_or_else(|| panic!("{name}: {url} was not served"));
            if (200.0..300.0).contains(&status) {
                Ok(s(answer.get("body")).to_owned())
            } else {
                Err(kiwix_tools::status_error(status as u16))
            }
        };
        let out = kiwix_tools::execute(&base, s(c.get("tool")), c.get("args"), cap, &mut get);
        let want = match c.get("out") {
            JValue::Null => None,
            other => Some(s(other).to_owned()),
        };
        assert_eq!(out, want, "{name}");
        let want_urls: Vec<&str> = arr(c.get("urls")).iter().map(s).collect();
        assert_eq!(urls, want_urls, "{name}");
    }
    assert_eq!(cases.len(), 129);
}
