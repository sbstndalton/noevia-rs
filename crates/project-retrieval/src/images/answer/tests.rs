#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

//! `answerImages`, `captionOnce` and the chat.cjs steps around them, with the expected texts read
//! off multimodal-rag.cjs and chat.cjs (the recorded half is `tests/image_answer_parity.rs`).

use super::*;
use crate::images::Bytes;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Waker;

fn j(text: &str) -> JValue {
    js_json::parse(text).unwrap()
}

fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..1000 {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
    panic!("the future never finished");
}

fn image(name: &str, mime: &str, text: &str) -> Resolved {
    Resolved {
        name: name.to_owned(),
        file: JValue::from(name),
        mime: JValue::from(mime),
        hash: JValue::Str(format!("img:{name}")),
        asset_id: None,
        score: j("0.5"),
        bytes: Bytes::File(PathBuf::from(format!("/nowhere/{name}"))),
        text: text.to_owned(),
    }
}

/// Scripted ports: bytes by name, captions by name, a probe answer; every call is counted.
#[derive(Default)]
struct Fake {
    sees: bool,
    bytes: Vec<(String, Vec<u8>)>,
    captions: Vec<(String, Caption)>,
    probes: Cell<usize>,
    asked: RefCell<Vec<String>>,
}

impl Ports for Fake {
    async fn can_see(&self) -> bool {
        self.probes.set(self.probes.get() + 1);
        self.sees
    }
    async fn caption(&self, image: &Resolved) -> Option<Caption> {
        self.asked.borrow_mut().push(image.name.clone());
        self.captions
            .iter()
            .find(|(n, _)| *n == image.name)
            .map(|(_, c)| c.clone())
    }
    fn bytes(&self, image: &Resolved) -> Option<Vec<u8>> {
        self.bytes
            .iter()
            .find(|(n, _)| *n == image.name)
            .map(|(_, b)| b.clone())
    }
    fn frame(&self, kind: &str, label: &str, text: &str) -> String {
        format!("<{kind}|{label}|{text}>")
    }
}

fn on_box() -> Turn<'static> {
    Turn::new(false, "an external provider", true)
}

#[test]
fn no_images_nothing_to_say_and_no_probe() {
    let fake = Fake::default();
    let d = block_on(answer_images(&[], on_box(), &fake));
    assert_eq!(d, Decided::default());
    assert_eq!(fake.probes.get(), 0);
}

#[test]
fn off_box_gets_names_only_and_no_probe_no_caption() {
    let fake = Fake {
        sees: true,
        bytes: vec![("a.png".into(), b"x".to_vec())],
        ..Fake::default()
    };
    let images = [
        image("a.png", "image/png", "secret text"),
        image("b\u{2028}.png", "image/png", ""),
    ];
    let d = block_on(answer_images(
        &images,
        Turn::new(true, "Cloud Co", false),
        &fake,
    ));
    assert_eq!(
        d.notes,
        ["Images in this project look relevant to this message (\"a.png\", \"b\\u2028.png\") but were not sent: images, and text read from them, are never sent to Cloud Co automatically. Say so if asked about them; do not guess at their contents."]
    );
    assert_eq!(
        d.warning,
        "Relevant project images were not sent: images are never sent to Cloud Co automatically."
    );
    assert!(d.parts.is_empty() && d.sent.is_empty());
    assert_eq!(fake.probes.get(), 0);
    assert!(fake.asked.borrow().is_empty());
}

#[test]
fn a_model_that_can_see_gets_the_images_up_to_the_caps() {
    let fake = Fake {
        sees: true,
        bytes: ["1.png", "2.png", "3.png", "4.png"]
            .iter()
            .map(|n| ((*n).to_owned(), b"foo".to_vec()))
            .collect(),
        ..Fake::default()
    };
    let images: Vec<Resolved> = ["1.png", "2.png", "3.png", "4.png"]
        .iter()
        .map(|n| image(n, "image/png", ""))
        .collect();
    let d = block_on(answer_images(&images, on_box(), &fake));
    assert_eq!(d.sent, ["1.png", "2.png", "3.png"]);
    assert_eq!(d.parts.len(), 3);
    assert_eq!(
        js_json::stringify(&d.parts[0]).unwrap(),
        r#"{"type":"image_url","image_url":{"url":"data:image/png;base64,Zm9v"}}"#
    );
    // The fourth is past maxImages: it has no text, so it is captioned, which fails: name only.
    assert_eq!(*fake.asked.borrow(), ["4.png"]);
    assert_eq!(d.notes.len(), 1);
    assert!(d.notes[0].contains("(\"4.png\"), but only their file names are known"));
    assert_eq!(
        d.warning,
        "Only the file names of a relevant image were used: the Vision model could not caption them."
    );
    assert_eq!(fake.probes.get(), 1);
}

#[test]
fn the_byte_budget_unknown_mimes_and_unreadable_files_fall_back_to_text() {
    let fake = Fake {
        sees: true,
        bytes: vec![
            ("big.png".into(), vec![0; 10]),
            ("next.png".into(), vec![0; 10]),
            ("small.png".into(), vec![0; 5]),
            ("odd.bmp".into(), vec![0; 1]),
        ],
        ..Fake::default()
    };
    let images = [
        image("big.png", "image/png", ""),
        image("next.png", "image/png", "next text"),
        image("small.png", "image/png", ""),
        image("odd.bmp", "image/bmp", "odd text"),
        image("gone.png", "image/png", "gone text"),
    ];
    let turn = Turn {
        max_total_bytes: 15,
        ..on_box()
    };
    let d = block_on(answer_images(&images, turn, &fake));
    // 10 fits, the next 10 does not (5 left), the 5 does.
    assert_eq!(d.sent, ["big.png", "small.png"]);
    assert_eq!(
        d.notes,
        [
            "<image text|next.png|next text>",
            "<image text|odd.bmp|odd text>",
            "<image text|gone.png|gone text>"
        ]
    );
    assert_eq!(d.warning, "");
    assert!(fake.asked.borrow().is_empty());
}

#[test]
fn an_image_over_the_size_limit_is_never_sent() {
    let fake = Fake {
        sees: true,
        bytes: vec![("huge.png".into(), vec![0; MAX_BYTES + 1])],
        ..Fake::default()
    };
    let turn = Turn {
        max_total_bytes: usize::MAX,
        ..on_box()
    };
    let d = block_on(answer_images(
        &[image("huge.png", "image/png", "t")],
        turn,
        &fake,
    ));
    assert!(d.parts.is_empty());
    assert_eq!(d.notes, ["<image text|huge.png|t>"]);
}

#[test]
fn a_blind_model_gets_text_then_captions_then_names() {
    let fake = Fake {
        sees: false,
        bytes: vec![("a.png".into(), b"foo".to_vec())],
        captions: vec![
            (
                "b.png".into(),
                Caption {
                    caption: "a red apple".into(),
                    model: Some("vision-role".into()),
                },
            ),
            (
                "c.png".into(),
                Caption {
                    caption: "a lemon".into(),
                    model: None,
                },
            ),
            (
                "d.png".into(),
                Caption {
                    caption: String::new(),
                    model: Some("vision-role".into()),
                },
            ),
        ],
        ..Fake::default()
    };
    let long = "x".repeat(MAX_TEXT_UNITS + 10);
    let images = [
        image("a.png", "image/png", &long),
        image("b.png", "image/png", ""),
        image("c.png", "image/png", ""),
        image("d.png", "image/png", ""),
        image("e.png", "image/png", ""),
    ];
    let d = block_on(answer_images(&images, Turn::new(false, "x", false), &fake));
    assert!(d.parts.is_empty());
    assert_eq!(d.notes.len(), 4);
    assert_eq!(
        d.notes[0],
        format!("<image text|a.png|{}>", "x".repeat(MAX_TEXT_UNITS))
    );
    assert_eq!(
        d.notes[1],
        "<image caption|b.png (caption by vision-role)|a red apple>"
    );
    assert_eq!(d.notes[2], "<image caption|c.png|a lemon>");
    assert_eq!(
        d.notes[3],
        "Images in this project look relevant to this message (\"d.png\", \"e.png\"), but only their file names are known: the answering model cannot see images and no Vision model is configured to caption them. Say so if asked about them; do not guess at their contents."
    );
    assert_eq!(
        d.warning,
        "Only the file names of relevant images were used: no Vision model is configured to caption them."
    );
    assert_eq!(*fake.asked.borrow(), ["b.png", "c.png", "d.png", "e.png"]);
}

#[test]
fn text_is_cut_by_utf16_units() {
    let fake = Fake::default();
    // 7,999 units, then a pair: the pair does not fit and is dropped (the crate's js_slice).
    let text = format!("{}\u{1f600}", "y".repeat(MAX_TEXT_UNITS - 1));
    let d = block_on(answer_images(
        &[image("t.png", "image/png", &text)],
        on_box(),
        &fake,
    ));
    assert_eq!(
        d.notes[0],
        format!("<image text|t.png|{}>", "y".repeat(MAX_TEXT_UNITS - 1))
    );
}

#[test]
fn provider_label_default() {
    assert_eq!(provider_label(&JValue::Undefined), "an external provider");
    assert_eq!(provider_label(&j(r#""""#)), "an external provider");
    assert_eq!(provider_label(&j(r#""Cloud Co""#)), "Cloud Co");
}

#[test]
fn who_may_caption() {
    let default = "local";
    // The local default provider, on the box, with a Vision model.
    assert!(may_caption(
        true,
        false,
        Some(("local", false)),
        false,
        default
    ));
    // The Vision role's own machine.
    assert!(may_caption(
        true,
        false,
        Some(("machine-7", false)),
        true,
        default
    ));
    // Any other provider, off the box, no model, no provider, or an external one: never.
    assert!(!may_caption(
        true,
        false,
        Some(("other", false)),
        false,
        default
    ));
    assert!(!may_caption(
        true,
        true,
        Some(("local", false)),
        false,
        default
    ));
    assert!(!may_caption(
        false,
        false,
        Some(("local", false)),
        false,
        default
    ));
    assert!(!may_caption(true, false, None, true, default));
    assert!(!may_caption(
        true,
        false,
        Some(("local", true)),
        false,
        default
    ));
}

#[test]
fn caption_key_is_json() {
    assert_eq!(
        caption_key(&j(r#""u1""#), &j(r#""p1""#), &j(r#""img:ab""#)),
        r#"["u1","p1","img:ab"]"#
    );
    assert_eq!(
        caption_key(&JValue::Undefined, &j(r#""p""#), &JValue::Null),
        r#"[null,"p",null]"#
    );
}

#[test]
fn caption_request_body_is_nodes() {
    let body = caption_request("vision-role", "image/jpeg", b"fo");
    assert_eq!(
        js_json::stringify(&body).unwrap(),
        format!(
            r#"{{"model":"vision-role","max_tokens":1024,"chat_template_kwargs":{{"enable_thinking":false}},"messages":[{{"role":"user","content":[{{"type":"text","text":"{DESCRIBE_PROMPT}"}},{{"type":"image_url","image_url":{{"url":"data:image/jpeg;base64,Zm8="}}}}]}}]}}"#
        )
    );
}

#[test]
fn caption_reply_rules() {
    assert_eq!(
        caption_reply(&j(
            r#"{"choices":[{"message":{"content":"  A cat. \n"},"finish_reason":"stop"}]}"#
        )),
        Some("A cat.".to_owned())
    );
    assert_eq!(
        caption_reply(&j(
            r#"{"choices":[{"message":{"content":"cut"},"finish_reason":"length"}]}"#
        )),
        None
    );
    assert_eq!(
        caption_reply(&j(r#"{"choices":[{"message":{"content":"   "}}]}"#)),
        None
    );
    assert_eq!(caption_reply(&j(r#"{"choices":[]}"#)), None);
    assert_eq!(caption_reply(&j("null")), None);
    // String(content): a number is text.
    assert_eq!(
        caption_reply(&j(r#"{"choices":[{"message":{"content":42}}]}"#)),
        Some("42".to_owned())
    );
}

fn decided(notes: &[&str], parts: usize, sent: &[&str], warning: &str) -> Decided {
    Decided {
        parts: (0..parts).map(|_| part("image/png", b"foo")).collect(),
        notes: notes.iter().map(|s| (*s).to_owned()).collect(),
        warning: warning.to_owned(),
        sent: sent.iter().map(|s| (*s).to_owned()).collect(),
    }
}

#[test]
fn apply_appends_notes_to_every_system_message() {
    let mut wire = vec![
        j(r#"{"role":"system","content":"S1"}"#),
        j(r#"{"role":"user","content":"hi"}"#),
        j(r#"{"role":"system","content":"S2"}"#),
    ];
    let mut warning = "Earlier.".to_owned();
    apply(
        &mut wire,
        &decided(&["N1", "N2"], 0, &[], "Later."),
        &mut warning,
    );
    assert_eq!(wire[0].get("content"), &j(r#""S1\n\nN1\n\nN2""#));
    assert_eq!(wire[2].get("content"), &j(r#""S2\n\nN1\n\nN2""#));
    assert_eq!(wire[1].get("content"), &j(r#""hi""#));
    assert_eq!(warning, "Earlier. Later.");
}

#[test]
fn apply_opens_the_wire_with_a_system_message_when_none() {
    let mut wire = vec![j(r#"{"role":"user","content":"hi"}"#)];
    let mut warning = String::new();
    apply(&mut wire, &decided(&["N"], 0, &[], ""), &mut warning);
    assert_eq!(
        js_json::stringify(&JValue::Arr(wire)).unwrap(),
        r#"[{"role":"system","content":"N"},{"role":"user","content":"hi"}]"#
    );
    assert_eq!(warning, "");
}

#[test]
fn apply_puts_parts_on_the_last_user_message() {
    let mut wire = vec![
        j(r#"{"role":"user","content":"first"}"#),
        j(r#"{"role":"assistant","content":"ok"}"#),
        j(r#"{"role":"user","content":"look"}"#),
    ];
    let mut warning = String::new();
    apply(&mut wire, &decided(&[], 1, &["a.png"], ""), &mut warning);
    assert_eq!(wire[0].get("content"), &j(r#""first""#));
    assert_eq!(
        js_json::stringify(wire[2].get("content")).unwrap(),
        r#"[{"type":"text","text":"look\n\n(Retrieved images: \"a.png\")"},{"type":"image_url","image_url":{"url":"data:image/png;base64,Zm9v"}}]"#
    );
    // Content that is already parts keeps them and gains the label and the parts.
    let mut wire = vec![j(
        r#"{"role":"user","content":[{"type":"text","text":"t"}]}"#,
    )];
    apply(&mut wire, &decided(&[], 2, &["a", "b"], ""), &mut warning);
    let content = wire[0].get("content");
    let JValue::Arr(items) = content else {
        panic!("parts")
    };
    assert_eq!(items.len(), 4);
    assert_eq!(
        items[1],
        j(r#"{"type":"text","text":"(Retrieved images: \"a\", \"b\")"}"#)
    );
    // Content that is neither: the label alone.
    let mut wire = vec![j(r#"{"role":"user"}"#)];
    apply(&mut wire, &decided(&[], 1, &["a"], ""), &mut warning);
    let JValue::Arr(items) = wire[0].get("content") else {
        panic!("parts")
    };
    assert_eq!(
        items[0],
        j(r#"{"type":"text","text":"\n\n(Retrieved images: \"a\")"}"#)
    );
    // No user message: nothing changes.
    let mut wire = vec![j(r#"{"role":"system","content":"s"}"#)];
    apply(&mut wire, &decided(&[], 1, &["a"], ""), &mut warning);
    assert_eq!(wire, vec![j(r#"{"role":"system","content":"s"}"#)]);
}

#[test]
fn caption_once_returns_the_cached_caption() {
    let captions = Captions::default();
    let generated = &Cell::new(0);
    let cached = Caption {
        caption: "cached".into(),
        model: Some("m".into()),
    };
    let got = block_on(captions.once(
        "k",
        || Some(cached.clone()),
        move || async move {
            generated.set(generated.get() + 1);
            None
        },
        |_, _| panic!("no write"),
    ));
    assert_eq!(got, Some(cached));
    assert_eq!(generated.get(), 0);
}

#[test]
fn caption_once_generates_trims_and_writes() {
    let captions = Captions::default();
    let written = RefCell::new(Vec::new());
    let got = block_on(captions.once(
        "k",
        || {
            Some(Caption {
                caption: String::new(),
                model: None,
            })
        },
        || async {
            Some(Caption {
                caption: " \u{3000}a dog\n".into(),
                model: Some(String::new()),
            })
        },
        |c, m| {
            written
                .borrow_mut()
                .push((c.to_owned(), m.map(str::to_owned)))
        },
    ));
    assert_eq!(
        got,
        Some(Caption {
            caption: "a dog".into(),
            model: None
        })
    );
    assert_eq!(*written.borrow(), [("a dog".to_owned(), None)]);
    assert_eq!(captions.in_flight(), 0);
    // An empty caption is no caption and is not written.
    let got = block_on(captions.once(
        "k",
        || None,
        || async {
            Some(Caption {
                caption: "  ".into(),
                model: None,
            })
        },
        |_, _| panic!("no write"),
    ));
    assert_eq!(got, None);
}

/// A future that waits until [`Gate::open`].
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
}

impl Gate {
    fn open(&self) {
        *self.open.lock().unwrap() = true;
    }
    async fn wait(&self) {
        std::future::poll_fn(|_| {
            if *self.open.lock().unwrap() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
}

#[test]
fn two_answers_asking_at_once_share_one_caption() {
    let (captions, gate) = (&Captions::default(), &Gate::default());
    let (generated, written) = (&AtomicUsize::new(0), &AtomicUsize::new(0));
    let ask = move || {
        captions.once(
            "same",
            || None,
            move || async move {
                generated.fetch_add(1, Ordering::SeqCst);
                gate.wait().await;
                Some(Caption {
                    caption: "shared".into(),
                    model: Some("vision-role".into()),
                })
            },
            move |_, _| {
                written.fetch_add(1, Ordering::SeqCst);
            },
        )
    };
    let mut first = std::pin::pin!(ask());
    let mut second = std::pin::pin!(ask());
    let mut cx = Context::from_waker(Waker::noop());
    assert!(first.as_mut().poll(&mut cx).is_pending());
    assert!(second.as_mut().poll(&mut cx).is_pending());
    assert_eq!(captions.in_flight(), 1);
    gate.open();
    let a = first.as_mut().poll(&mut cx);
    let b = second.as_mut().poll(&mut cx);
    let want = Some(Caption {
        caption: "shared".into(),
        model: Some("vision-role".into()),
    });
    assert_eq!(a, Poll::Ready(want.clone()));
    assert_eq!(b, Poll::Ready(want));
    assert_eq!(generated.load(Ordering::SeqCst), 1);
    assert_eq!(written.load(Ordering::SeqCst), 1);
    assert_eq!(captions.in_flight(), 0);
}

#[test]
fn a_dropped_leader_releases_its_followers_and_the_key() {
    let (captions, gate) = (&Captions::default(), &Gate::default());
    let ask = move || {
        captions.once(
            "k",
            || None,
            move || async move {
                gate.wait().await;
                Some(Caption {
                    caption: "late".into(),
                    model: None,
                })
            },
            |_, _| {},
        )
    };
    let mut cx = Context::from_waker(Waker::noop());
    let mut first = Box::pin(ask());
    let mut second = std::pin::pin!(ask());
    assert!(first.as_mut().poll(&mut cx).is_pending());
    assert!(second.as_mut().poll(&mut cx).is_pending());
    drop(first);
    assert_eq!(second.as_mut().poll(&mut cx), Poll::Ready(None));
    assert_eq!(captions.in_flight(), 0);
    // A later request leads a new flight.
    gate.open();
    assert_eq!(
        block_on(ask()),
        Some(Caption {
            caption: "late".into(),
            model: None
        })
    );
}

#[test]
fn different_keys_do_not_wait_for_each_other() {
    let captions = Captions::default();
    let gate = &Gate::default();
    let mut cx = Context::from_waker(Waker::noop());
    let mut slow = std::pin::pin!(captions.once(
        "a",
        || None,
        move || async move {
            gate.wait().await;
            None
        },
        |_, _| {},
    ));
    assert!(slow.as_mut().poll(&mut cx).is_pending());
    let quick = block_on(captions.once(
        "b",
        || None,
        || async {
            Some(Caption {
                caption: "b".into(),
                model: None,
            })
        },
        |_, _| {},
    ));
    assert_eq!(quick.map(|c| c.caption), Some("b".to_owned()));
    assert_eq!(captions.in_flight(), 1);
    gate.open();
    assert_eq!(slow.as_mut().poll(&mut cx), Poll::Ready(None));
}
