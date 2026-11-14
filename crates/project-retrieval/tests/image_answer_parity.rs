#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

//! F16: the answer side of retrieved images (#1726) recorded from noevia-core
//! `multimodal-rag.cjs` (`generate_image_answer_fixtures.cjs`): `assetPlan`, `resolveHit` against
//! a fake workspace (the path each image would be read from) and `answerImages` with scripted
//! ports. Until the fixture is recorded the replay is skipped; with
//! `IMAGE_ANSWER_ORACLE_REQUIRED=1` (CI, the builder) a missing fixture fails. A case marked
//! `knownDifference` is the module's named difference (the quoting of names in the two notes) and
//! must differ from Node exactly there.

use std::future::Future;
use std::path::Path;
use std::pin::pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use js_json::JValue;
use project_retrieval::images::answer::{answer_images, Caption, Ports, Turn};
use project_retrieval::images::{
    asset_plan, resolve_hit, Bytes, Dirs, IdSet, ImageRetrieval, Resolved,
};

fn fixture() -> Option<JValue> {
    let path = format!(
        "{}/tests/fixtures/F16-image-answer.json",
        env!("CARGO_MANIFEST_DIR")
    );
    match std::fs::read_to_string(&path) {
        Ok(text) => Some(js_json::parse(&text).unwrap()),
        Err(_) if std::env::var("IMAGE_ANSWER_ORACLE_REQUIRED").as_deref() == Ok("1") => {
            panic!("{path} is missing: record it with generate_image_answer_fixtures.cjs")
        }
        Err(_) => {
            eprintln!("skipped: {path} not recorded yet");
            None
        }
    }
}

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(items) => items,
        _ => panic!("not an array: {v:?}"),
    }
}

fn s(v: &JValue) -> &str {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v:?}"))
}

fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

/// A recorded JS `Set` (`undefined` as `{undefined: true}`).
fn recorded_set(v: &JValue) -> IdSet {
    arr(v)
        .iter()
        .map(|x| {
            if x.get("undefined").as_bool() == Some(true) {
                JValue::Undefined
            } else {
                x.clone()
            }
        })
        .collect()
}

#[test]
fn asset_plans_match_node() {
    let Some(f) = fixture() else { return };
    for c in arr(f.get("plan")) {
        let r = ImageRetrieval {
            hits: arr(c.get("hits")).to_vec(),
            indexed: arr(c.get("indexed")).to_vec(),
        };
        let plan = asset_plan(c.get("assets"), &r);
        let out = c.get("out");
        let name = s(c.get("name"));
        assert_eq!(
            plan.ride_along,
            recorded_set(out.get("rideAlong")),
            "{name}: rideAlong"
        );
        assert_eq!(
            plan.retrieved,
            recorded_set(out.get("retrieved")),
            "{name}: retrieved"
        );
        assert_eq!(
            plan.indexed,
            recorded_set(out.get("indexed")),
            "{name}: indexed"
        );
    }
}

#[test]
fn resolved_hits_match_node() {
    let Some(f) = fixture() else { return };
    let project = f.get("project");
    for c in arr(f.get("resolve")) {
        let name = s(c.get("name"));
        let dirs = Dirs {
            assets: Path::new("/a"),
            documents: (c.get("noDocuments").as_bool() != Some(true)).then_some(Path::new("/d")),
        };
        let got = resolve_hit(project, c.get("hit"), dirs);
        let out = c.get("out");
        let Some(r) = got else {
            assert_eq!(out, &JValue::Null, "{name}: Node resolved it");
            continue;
        };
        assert!(
            out.is_object(),
            "{name}: Node dropped it, the port resolved {r:?}"
        );
        assert_eq!(r.name, s(out.get("name")), "{name}: name");
        assert_eq!(&r.file, out.get("file"), "{name}: file");
        assert_eq!(&r.mime, out.get("mime"), "{name}: mime");
        assert_eq!(&r.hash, out.get("hash"), "{name}: hash");
        assert_eq!(
            r.asset_id.as_ref().unwrap_or(&JValue::Undefined),
            out.get("assetId"),
            "{name}: assetId"
        );
        assert_eq!(&r.score, out.get("score"), "{name}: score");
        assert_eq!(r.text, s(out.get("text")), "{name}: text");
        // Node's path.join of the same parts (no case here leaves the directory).
        match &r.bytes {
            Bytes::File(p) => assert_eq!(p.to_str().unwrap(), s(out.get("path")), "{name}: path"),
            Bytes::Refused => panic!("{name}: no recorded id is refused"),
        }
    }
}

/// The recorded ports: bytes of a length (`null` = the read throws), captions by name (`"throw"`
/// = the caption fails), the probe answer (`"throw"` = the probe fails).
struct Recorded<'a> {
    case: &'a JValue,
    probes: Mutex<usize>,
    asked: Mutex<Vec<String>>,
}

impl Ports for Recorded<'_> {
    async fn can_see(&self) -> bool {
        *self.probes.lock().unwrap() += 1;
        self.case.get("sees").as_bool() == Some(true)
    }
    async fn caption(&self, image: &Resolved) -> Option<Caption> {
        self.asked.lock().unwrap().push(image.name.clone());
        let made = self.case.get("captions").get(&image.name);
        if !made.is_object() {
            return None;
        }
        Some(Caption {
            caption: made.get("caption").as_str().unwrap_or_default().to_owned(),
            model: made.get("model").as_str().map(str::to_owned),
        })
    }
    fn bytes(&self, image: &Resolved) -> Option<Vec<u8>> {
        let i = arr(self.case.get("images"))
            .iter()
            .find(|i| i.get("name").as_str() == Some(image.name.as_str()))?;
        let n = i.get("bytes").as_num()?;
        Some(vec![b'x'; n as usize])
    }
    fn frame(&self, kind: &str, label: &str, text: &str) -> String {
        format!("<{kind}|{label}|{text}>")
    }
}

#[test]
fn answers_match_node() {
    let Some(f) = fixture() else { return };
    for c in arr(f.get("answer")) {
        let name = s(c.get("name"));
        let images: Vec<Resolved> = arr(c.get("images"))
            .iter()
            .map(|i| Resolved {
                name: s(i.get("name")).to_owned(),
                file: i.get("name").clone(),
                mime: i.get("mime").clone(),
                hash: JValue::Str(format!("img:{}", s(i.get("name")))),
                asset_id: None,
                score: JValue::Num(0.0),
                bytes: Bytes::Refused,
                text: s(i.get("text")).to_owned(),
            })
            .collect();
        let label = c
            .get("providerLabel")
            .as_str()
            .unwrap_or("an external provider");
        let mut turn = Turn::new(
            c.get("external").as_bool() == Some(true),
            label,
            c.get("hasVisionRole").as_bool() == Some(true),
        );
        if let Some(n) = c.get("maxImages").as_num() {
            turn.max_images = n as usize;
        }
        if let Some(n) = c.get("maxTotalBytes").as_num() {
            turn.max_total_bytes = n as usize;
        }
        let ports = Recorded {
            case: c,
            probes: Mutex::new(0),
            asked: Mutex::new(Vec::new()),
        };
        let d = block_on(answer_images(&images, turn, &ports));
        let out = c.get("out");
        let parts: Vec<(String, usize)> = arr(out.get("parts"))
            .iter()
            .map(|p| {
                (
                    s(p.get("mime")).to_owned(),
                    p.get("bytes").as_num().unwrap() as usize,
                )
            })
            .collect();
        let got_parts: Vec<(String, usize)> = d
            .parts
            .iter()
            .map(|p| {
                let url = s(p.get("image_url").get("url"));
                let (head, b64) = url.split_once(";base64,").unwrap();
                let mime = head.strip_prefix("data:").unwrap().to_owned();
                let pad = b64.bytes().rev().take_while(|&b| b == b'=').count();
                (mime, b64.len() / 4 * 3 - pad)
            })
            .collect();
        assert_eq!(got_parts, parts, "{name}: parts");
        let sent: Vec<&str> = arr(out.get("sent")).iter().map(s).collect();
        assert_eq!(d.sent, sent, "{name}: sent");
        assert_eq!(d.warning, s(out.get("warning")), "{name}: warning");
        assert_eq!(
            *ports.probes.lock().unwrap() as f64,
            out.get("probes").as_num().unwrap(),
            "{name}: probes"
        );
        let asked: Vec<&str> = arr(out.get("asked")).iter().map(s).collect();
        assert_eq!(
            *ports.asked.lock().unwrap(),
            asked,
            "{name}: captions asked"
        );
        let notes: Vec<&str> = arr(out.get("notes")).iter().map(s).collect();
        if c.get("knownDifference").as_str() == Some("quoteName") {
            // JSON.stringify leaves U+2028 raw; quoted-name.cjs writes it as  .
            assert_ne!(d.notes, notes, "{name}: the named difference is gone");
            let node: Vec<String> = notes
                .iter()
                .map(|n| n.replace('\u{2028}', "\\u2028"))
                .collect();
            assert_eq!(d.notes, node, "{name}: notes beyond the named difference");
        } else {
            assert_eq!(d.notes, notes, "{name}: notes");
        }
    }
}
