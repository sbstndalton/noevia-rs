#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

//! A whole retrieved-images turn over real files: a QA project with an image source and a
//! document figure on disk, an image search answer, the selection, `answerImages` reading the
//! bytes through the default port, the wire steps. The expected values are chat.cjs's.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use js_json::JValue;
use project_retrieval::images::answer::{
    answer_images, apply, caption_key, Caption, Captions, Ports, Turn,
};
use project_retrieval::images::{base64, select, Dirs, ImageRetrieval, Resolved};

const H1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const H3: &str = "3333333333333333333333333333333333333333333333333333333333333333";
const V1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn j(text: &str) -> JValue {
    js_json::parse(text).unwrap()
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

fn project() -> JValue {
    j(&format!(
        r#"{{"id":"qa-proj","files":[
            {{"name":"qa-apple.png","attachment":{{"id":"{H1}"}}}},
            {{"name":"qa-report.pdf","document":{{"availableVersion":"{V1}"}}}}
        ],"assets":[{{"id":"img-1","name":"qa-apple.png","mime":"image/png","sourceName":"qa-apple.png"}}]}}"#
    ))
}

fn retrieval() -> ImageRetrieval {
    ImageRetrieval {
        hits: vec![
            j(&format!(
                r#"{{"file":"qa-apple.png","kind":"image","hash":"img:{H1}","score":0.8,"meta":{{"assetId":"img-1"}}}}"#
            )),
            j(&format!(
                r#"{{"file":"qa-report.pdf","kind":"image","hash":"img:{H3}","score":0.6,"meta":{{"page":1,"figure":1,"version":"{V1}","figureFile":"{V1}.fig0.png","mime":"image/png"}}}}"#
            )),
        ],
        indexed: vec![j(&format!(
            r#"{{"file":"qa-apple.png","kind":"image","hash":"img:{H1}","score":0,"meta":{{"assetId":"img-1"}}}}"#
        ))],
    }
}

/// Probe and captions scripted; bytes and framing are the defaults a caller would wire.
struct Turnside<'a> {
    sees: bool,
    captions: &'a Captions,
}

impl Ports for Turnside<'_> {
    async fn can_see(&self) -> bool {
        self.sees
    }
    async fn caption(&self, image: &Resolved) -> Option<Caption> {
        let key = caption_key(&j(r#""qa-user""#), &j(r#""qa-proj""#), &image.hash);
        self.captions
            .once(
                &key,
                || None,
                || async {
                    Some(Caption {
                        caption: " a bar chart ".into(),
                        model: Some("vision-role".into()),
                    })
                },
                |_, _| {},
            )
            .await
    }
    fn frame(&self, kind: &str, label: &str, text: &str) -> String {
        format!("[{kind}: {label}]\n{text}")
    }
}

fn on_disk() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("assets")).unwrap();
    std::fs::create_dir_all(root.path().join("documents")).unwrap();
    std::fs::write(root.path().join("assets/img-1"), b"PNGBYTES").unwrap();
    std::fs::write(root.path().join(format!("documents/{V1}.fig0.png")), b"FIG").unwrap();
    root
}

#[test]
fn a_model_that_can_see_gets_both_images_read_from_disk() {
    let root = on_disk();
    let assets = root.path().join("assets");
    let documents = root.path().join("documents");
    let dirs = Dirs {
        assets: &assets,
        documents: Some(&documents),
    };
    let s = select(
        &project(),
        &j(r#""what do the pictures show?""#),
        &JValue::Undefined,
        true,
        Some(&retrieval()),
        dirs,
    );
    assert!(
        s.ride_along.is_empty(),
        "the indexed apple is not sent the old way"
    );
    assert_eq!(s.retrieved.len(), 2);
    let captions = Captions::default();
    let ports = Turnside {
        sees: true,
        captions: &captions,
    };
    let d = block_on(answer_images(
        &s.retrieved,
        Turn::new(false, "x", true),
        &ports,
    ));
    assert_eq!(d.sent, ["qa-apple.png", "qa-report.pdf page 1 figure 1"]);
    let mut wire = vec![
        j(r#"{"role":"system","content":"S"}"#),
        j(r#"{"role":"user","content":"what do the pictures show?"}"#),
    ];
    let mut warning = String::new();
    apply(&mut wire, &d, &mut warning);
    assert_eq!(wire[0].get("content"), &j(r#""S""#));
    let JValue::Arr(parts) = wire[1].get("content") else {
        panic!("parts")
    };
    assert_eq!(
        parts[0],
        j(
            r#"{"type":"text","text":"what do the pictures show?\n\n(Retrieved images: \"qa-apple.png\", \"qa-report.pdf page 1 figure 1\")"}"#
        )
    );
    assert_eq!(
        parts[1].get("image_url").get("url"),
        &JValue::Str(format!("data:image/png;base64,{}", base64(b"PNGBYTES")))
    );
    assert_eq!(
        parts[2].get("image_url").get("url"),
        &JValue::Str(format!("data:image/png;base64,{}", base64(b"FIG")))
    );
    assert_eq!(warning, "");
}

#[test]
fn a_blind_model_gets_a_caption_and_a_missing_file_is_captioned_too() {
    let root = on_disk();
    std::fs::remove_file(root.path().join("assets/img-1")).unwrap();
    let assets = root.path().join("assets");
    let documents = root.path().join("documents");
    let dirs = Dirs {
        assets: &assets,
        documents: Some(&documents),
    };
    let s = select(
        &project(),
        &j(r#""chart?""#),
        &JValue::Undefined,
        true,
        Some(&retrieval()),
        dirs,
    );
    let captions = Captions::default();
    for sees in [false, true] {
        let ports = Turnside {
            sees,
            captions: &captions,
        };
        let d = block_on(answer_images(
            &s.retrieved,
            Turn::new(false, "x", true),
            &ports,
        ));
        // Seeing or not, the apple's file is gone: it is captioned (bytes are the caption's own
        // business); a blind model gets the figure captioned as well.
        let want_notes = if sees { 1 } else { 2 };
        assert_eq!(d.notes.len(), want_notes, "sees={sees}");
        assert_eq!(
            d.notes[0],
            "[image caption: qa-apple.png (caption by vision-role)]\na bar chart"
        );
        assert_eq!(d.sent.len(), usize::from(sees));
    }
    assert_eq!(captions.in_flight(), 0);
}

#[test]
fn an_off_box_turn_reads_nothing() {
    let root = on_disk();
    let assets = root.path().join("assets");
    let dirs = Dirs {
        assets: &assets,
        documents: None,
    };
    let s = select(
        &project(),
        &j(r#""apple""#),
        &JValue::Undefined,
        true,
        Some(&retrieval()),
        dirs,
    );
    // Without a document directory only the image source resolves.
    assert_eq!(s.retrieved.len(), 1);
    let captions = Captions::default();
    let ports = Turnside {
        sees: true,
        captions: &captions,
    };
    let d = block_on(answer_images(
        &s.retrieved,
        Turn::new(true, "an external provider", true),
        &ports,
    ));
    assert!(d.parts.is_empty());
    let mut wire = vec![j(r#"{"role":"user","content":"apple"}"#)];
    let mut warning = "Earlier.".to_owned();
    apply(&mut wire, &d, &mut warning);
    assert_eq!(wire[0].get("role"), &j(r#""system""#));
    assert_eq!(wire[1], j(r#"{"role":"user","content":"apple"}"#));
    assert_eq!(
        warning,
        "Earlier. Relevant project images were not sent: images are never sent to an external provider automatically."
    );
}
