#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

//! The selection rules, with the expected values read off multimodal-rag.cjs and chat.cjs (the
//! recorded half is `tests/image_answer_parity.rs`).

use super::*;
use std::path::Path;

const H1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const H2: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const H3: &str = "3333333333333333333333333333333333333333333333333333333333333333";
const V1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn j(text: &str) -> JValue {
    js_json::parse(text).unwrap()
}

fn dirs() -> Dirs<'static> {
    Dirs {
        assets: Path::new("/a"),
        documents: Some(Path::new("/d")),
    }
}

/// A QA project: two image sources (apple, banana) and one document with a figure.
fn project() -> JValue {
    j(&format!(
        r#"{{"id":"qa-proj","files":[
            {{"name":"apple.png","attachment":{{"id":"{H1}"}},"content":"  APPLE label text \n"}},
            {{"name":"banana.jpg","attachment":{{"id":"{H2}"}}}},
            {{"name":"docs/report.pdf","document":{{"availableVersion":"{V1}"}}}}
        ],"assets":[
            {{"id":"img-1","name":"apple.png","mime":"image/png","sourceName":"apple.png"}},
            {{"id":"img-2","name":"banana.jpg","mime":"image/jpeg","sourceName":"banana.jpg"}}
        ]}}"#
    ))
}

fn asset_hit(id: &str, file: &str, hash: &str, score: f64) -> JValue {
    j(&format!(
        r#"{{"file":"{file}","kind":"image","hash":"img:{hash}","score":{score},"meta":{{"source":"{file}","assetId":"{id}"}}}}"#
    ))
}

fn figure_hit(score: f64) -> JValue {
    j(&format!(
        r#"{{"file":"docs/report.pdf","kind":"image","hash":"img:{H3}","score":{score},"meta":{{"source":"docs/report.pdf","page":2,"figure":1,"version":"{V1}","figureFile":"{V1}.fig0.png","mime":"image/png","caption":" A chart ","text":"Q3 revenue\n"}}}}"#
    ))
}

#[test]
fn base64_matches_buffer() {
    assert_eq!(base64(b""), "");
    assert_eq!(base64(b"f"), "Zg==");
    assert_eq!(base64(b"fo"), "Zm8=");
    assert_eq!(base64(b"foo"), "Zm9v");
    assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    assert_eq!(base64(&[0xff, 0xfe, 0x00]), "//4A");
}

#[test]
fn part_is_a_data_url() {
    let p = part("image/png", b"foo");
    assert_eq!(
        js_json::stringify(&p).unwrap(),
        r#"{"type":"image_url","image_url":{"url":"data:image/png;base64,Zm9v"}}"#
    );
}

#[test]
fn figure_file_rule() {
    assert!(figure_file(&format!("{V1}.fig0.png")));
    assert!(figure_file(&format!("{V1}.fig123.webp")));
    assert!(figure_file(&format!("{V1}.fig9.jpg")));
    assert!(!figure_file(&format!("{V1}.fig1234.png")));
    assert!(!figure_file(&format!("{V1}.fig.png")));
    assert!(!figure_file(&format!("{V1}.fig1.gif")));
    assert!(!figure_file(&format!("{V1}.fig1.png.x")));
    assert!(!figure_file(&format!("{}.fig1.png", V1.to_uppercase())));
    assert!(!figure_file(&format!("x{V1}.fig1.png")));
    assert!(!figure_file("short.fig1.png"));
    assert!(!figure_file(""));
}

#[test]
fn plain_asset_names_only() {
    assert!(plain_asset_name("img-0af"));
    for bad in ["", ".", "..", "../x", "a/b", "a\\b", "a\0b"] {
        assert!(!plain_asset_name(bad), "{bad:?}");
    }
}

#[test]
fn base_name_and_trim() {
    assert_eq!(base_name(&j(r#""a/b/c.png""#)), "c.png");
    assert_eq!(base_name(&j(r#""c.png""#)), "c.png");
    assert_eq!(base_name(&JValue::Undefined), "");
    assert_eq!(base_name(&j("12")), "12");
    // JavaScript's trim also strips U+FEFF and U+3000; Rust's does not strip U+FEFF.
    assert_eq!(js_trim("\u{feff}\u{3000} x \n"), "x");
}

#[test]
fn strict_equality_is_primitive_only() {
    assert!(strict_eq(&j("1"), &j("1.0")));
    assert!(strict_eq(&JValue::Undefined, &JValue::Undefined));
    assert!(!strict_eq(&JValue::Undefined, &JValue::Null));
    assert!(!strict_eq(&j(r#""1""#), &j("1")));
    assert!(!strict_eq(&j("{}"), &j("{}")));
    assert!(!strict_eq(&j("[]"), &j("[]")));
}

#[test]
fn asset_plan_sends_seen_images_only_when_retrieved() {
    let r = ImageRetrieval {
        hits: vec![asset_hit("img-1", "apple.png", H1, 0.71)],
        indexed: vec![
            asset_hit("img-1", "apple.png", H1, 0.0),
            figure_hit(0.0),
            j(r#"{"meta":{"assetId":""}}"#),
            JValue::Null,
        ],
    };
    let plan = asset_plan(project().get("assets"), &r);
    assert_eq!(plan.indexed.iter().collect::<Vec<_>>(), [&j(r#""img-1""#)]);
    assert_eq!(
        plan.retrieved.iter().collect::<Vec<_>>(),
        [&j(r#""img-1""#)]
    );
    // banana has no vector yet: it rides along.
    assert_eq!(
        plan.ride_along.iter().collect::<Vec<_>>(),
        [&j(r#""img-2""#)]
    );
}

#[test]
fn asset_plan_keeps_an_id_less_asset_riding_along() {
    let assets = j(r#"[null, {"name":"x"}, {"id":"img-9"}, {"id":"img-9"}]"#);
    let plan = asset_plan(&assets, &ImageRetrieval::default());
    // `undefined` is an element of the JS Set too; duplicates collapse.
    assert_eq!(
        plan.ride_along.iter().collect::<Vec<_>>(),
        [&JValue::Undefined, &j(r#""img-9""#)]
    );
    assert!(asset_plan(&j("{}"), &ImageRetrieval::default())
        .ride_along
        .is_empty());
}

#[test]
fn resolve_an_image_source() {
    let r = resolve_hit(
        &project(),
        &asset_hit("img-1", "apple.png", H1, 0.71),
        dirs(),
    )
    .unwrap();
    assert_eq!(r.name, "apple.png");
    assert_eq!(r.file, j(r#""apple.png""#));
    assert_eq!(r.mime, j(r#""image/png""#));
    assert_eq!(r.hash, JValue::Str(format!("img:{H1}")));
    assert_eq!(r.asset_id, Some(j(r#""img-1""#)));
    assert_eq!(r.score, j("0.71"));
    assert_eq!(r.bytes, Bytes::File(Path::new("/a/img-1").to_path_buf()));
    assert_eq!(r.text, "APPLE label text");
}

#[test]
fn an_image_whose_bytes_changed_since_indexing_is_dropped() {
    assert!(resolve_hit(
        &project(),
        &asset_hit("img-1", "apple.png", H2, 0.7),
        dirs()
    )
    .is_none());
    // The source is gone.
    assert!(resolve_hit(&project(), &asset_hit("img-1", "gone.png", H1, 0.7), dirs()).is_none());
    // The asset is gone.
    assert!(resolve_hit(
        &project(),
        &asset_hit("img-7", "apple.png", H1, 0.7),
        dirs()
    )
    .is_none());
}

#[test]
fn an_asset_without_a_name_takes_its_source_base_name() {
    let p = j(&format!(
        r#"{{"files":[{{"name":"pics/kiwi.webp","attachment":{{"id":"{H1}"}}}}],
            "assets":[{{"id":"img-3","mime":"image/webp"}}]}}"#
    ));
    let r = resolve_hit(&p, &asset_hit("img-3", "pics/kiwi.webp", H1, 0.5), dirs()).unwrap();
    assert_eq!(r.name, "kiwi.webp");
    assert_eq!(r.text, "");
}

#[test]
fn a_source_without_an_attachment_id_matches_only_img_undefined() {
    let p = j(r#"{"files":[{"name":"x.png"}],"assets":[{"id":"img-4"}]}"#);
    let hit = j(r#"{"file":"x.png","hash":"img:undefined","meta":{"assetId":"img-4"}}"#);
    assert!(resolve_hit(&p, &hit, dirs()).is_some());
}

#[test]
fn an_asset_id_that_is_not_a_file_name_is_never_read() {
    let p = j(&format!(
        r#"{{"files":[{{"name":"x.png","attachment":{{"id":"{H1}"}}}}],"assets":[{{"id":"../../etc/x"}}]}}"#
    ));
    let hit = asset_hit("../../etc/x", "x.png", H1, 0.5);
    let r = resolve_hit(&p, &hit, dirs()).unwrap();
    assert_eq!(r.bytes, Bytes::Refused);
    assert_eq!(r.bytes.read(), None);
}

#[test]
fn resolve_a_document_figure() {
    let r = resolve_hit(&project(), &figure_hit(0.6), dirs()).unwrap();
    assert_eq!(r.name, "report.pdf page 2 figure 1");
    assert_eq!(r.file, j(r#""docs/report.pdf""#));
    assert_eq!(r.asset_id, None);
    assert_eq!(
        r.bytes,
        Bytes::File(Path::new("/d").join(format!("{V1}.fig0.png")))
    );
    // Caption and text, each kept untrimmed, joined, then trimmed as a whole.
    assert_eq!(r.text, "A chart \nQ3 revenue");
}

#[test]
fn a_figure_of_an_older_version_or_without_a_document_dir_is_dropped() {
    let mut p = project();
    if let JValue::Obj(items) = &mut p {
        let files = items.iter_mut().find(|(k, _)| k == "files").unwrap();
        files.1 = j(&format!(
            r#"[{{"name":"docs/report.pdf","document":{{"availableVersion":"{H1}"}}}}]"#
        ));
    }
    assert!(resolve_hit(&p, &figure_hit(0.6), dirs()).is_none());
    let no_docs = Dirs {
        assets: Path::new("/a"),
        documents: None,
    };
    assert!(resolve_hit(&project(), &figure_hit(0.6), no_docs).is_none());
    let bad = j(&format!(
        r#"{{"file":"docs/report.pdf","meta":{{"version":"{V1}","figureFile":"../{V1}.fig0.png"}}}}"#
    ));
    assert!(resolve_hit(&project(), &bad, dirs()).is_none());
}

#[test]
fn a_figure_without_text_has_empty_text_and_numbers_as_js_writes_them() {
    let hit = j(&format!(
        r#"{{"file":"docs/report.pdf","hash":"img:{H3}","meta":{{"page":3,"figure":2.5,"version":"{V1}","figureFile":"{V1}.fig1.jpg","caption":"   ","text":7}}}}"#
    ));
    let r = resolve_hit(&project(), &hit, dirs()).unwrap();
    assert_eq!(r.name, "report.pdf page 3 figure 2.5");
    assert_eq!(r.text, "");
    // No assetId and no figureFile: nothing to resolve.
    assert!(resolve_hit(&project(), &j(r#"{"file":"apple.png"}"#), dirs()).is_none());
}

fn retrieval() -> ImageRetrieval {
    ImageRetrieval {
        hits: vec![
            asset_hit("img-1", "apple.png", H1, 0.71),
            figure_hit(0.6),
            // The same figure twice: listed once.
            figure_hit(0.5),
        ],
        indexed: vec![
            asset_hit("img-1", "apple.png", H1, 0.0),
            asset_hit("img-2", "banana.jpg", "stale", 0.0),
        ],
    }
}

#[test]
fn select_with_retrieval_on() {
    let s = select(
        &project(),
        &j(r#""what is in the picture?""#),
        &JValue::Undefined,
        true,
        Some(&retrieval()),
        dirs(),
    );
    let plan = s.plan.as_ref().unwrap();
    assert_eq!(
        plan.retrieved.iter().collect::<Vec<_>>(),
        [&j(r#""img-1""#)]
    );
    // Both images have a vector: none rides along, and banana (not retrieved) is named only.
    assert_eq!(s.named.as_ref().unwrap().len(), 0);
    assert!(s.ride_along.is_empty());
    assert_eq!(s.unattached_names, ["banana.jpg"]);
    let names: Vec<&str> = s.retrieved.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["apple.png", "report.pdf page 2 figure 1"]);
}

#[test]
fn an_image_named_in_the_question_is_retrieved_with_its_hash_from_the_source_now() {
    let s = select(
        &project(),
        &j(r#""Compare it with BANANA.JPG please""#),
        &JValue::Undefined,
        true,
        Some(&retrieval()),
        dirs(),
    );
    let plan = s.plan.as_ref().unwrap();
    assert_eq!(
        plan.retrieved.iter().collect::<Vec<_>>(),
        [&j(r#""img-1""#), &j(r#""img-2""#)]
    );
    assert!(s.unattached_names.is_empty());
    let banana = s.retrieved.iter().find(|r| r.name == "banana.jpg").unwrap();
    // The index row's hash was stale; the pushed copy takes the source's attachment id.
    assert_eq!(banana.hash, JValue::Str(format!("img:{H2}")));
    assert_eq!(banana.score, j("0"));
    // Appended after the hits.
    assert_eq!(s.retrieved.last().unwrap().name, "banana.jpg");
}

#[test]
fn turn_image_ids_replace_the_plan_and_are_not_repeated() {
    let s = select(
        &project(),
        &j(r#""banana.jpg""#),
        &j(r#"["img-1"]"#),
        true,
        Some(&retrieval()),
        dirs(),
    );
    assert!(s.plan.is_none());
    assert_eq!(s.ride_along, [0]);
    assert_eq!(s.unattached_names, ["banana.jpg"]);
    // apple is attached already; the figure is still retrieved.
    let names: Vec<&str> = s.retrieved.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["report.pdf page 2 figure 1"]);
}

#[test]
fn without_retrieval_every_image_rides_along_as_before() {
    for (multimodal, retrieval) in [(false, Some(retrieval())), (true, None)] {
        let s = select(
            &project(),
            &j(r#""apple""#),
            &JValue::Undefined,
            multimodal,
            retrieval.as_ref(),
            dirs(),
        );
        assert_eq!(s.plan, None);
        assert_eq!(s.named, None);
        assert_eq!(s.ride_along, [0, 1]);
        assert!(s.unattached_names.is_empty());
        assert!(s.retrieved.is_empty());
    }
    // An empty turnImageIds list attaches nothing and names every image.
    let s = select(&project(), &JValue::Null, &j("[]"), false, None, dirs());
    assert!(s.ride_along.is_empty());
    assert_eq!(s.unattached_names, ["apple.png", "banana.jpg"]);
}

#[test]
fn a_nameless_or_null_asset_never_matches_the_question() {
    let p = j(&format!(
        r#"{{"files":[{{"name":"x.png","attachment":{{"id":"{H1}"}}}}],
            "assets":[null,{{"id":"img-5","mime":"image/png"}}]}}"#
    ));
    let r = ImageRetrieval {
        hits: vec![],
        indexed: vec![asset_hit("img-5", "x.png", H1, 0.0)],
    };
    let s = select(
        &p,
        &j(r#""undefined""#),
        &JValue::Undefined,
        true,
        Some(&r),
        dirs(),
    );
    assert!(s.retrieved.is_empty());
    // The null entry is named as Node's String(undefined) writes it.
    assert_eq!(s.unattached_names, ["undefined", "undefined"]);
}

#[test]
fn with_hash_overwrites_in_place() {
    let h = with_hash(&j(r#"{"hash":"old","file":"x"}"#), "img:new".to_owned());
    assert_eq!(
        js_json::stringify(&h).unwrap(),
        r#"{"hash":"img:new","file":"x"}"#
    );
    let h = with_hash(&j(r#"{"file":"x"}"#), "img:new".to_owned());
    assert_eq!(
        js_json::stringify(&h).unwrap(),
        r#"{"file":"x","hash":"img:new"}"#
    );
}
