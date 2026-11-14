#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
//! The expected strings are typed from core `rag.cjs` `filesContext` (the manifest, the coverage
//! line, the frames), not taken from the composition under test.

use std::cell::Cell;

use chat_prepare::retrieval::{retrieved, Answer, Request, Source as ReadableFile};
use js_json::JValue;
use project_retrieval::hits::Hit;

use super::*;

const COVERAGE_TAIL: &str = "\nContext may contain excerpts only. Use read_project_file with PDF startPage/endPage and offset for pages beyond the summary. Do not treat missing excerpts or failed/partial sources as evidence of absence.";
const LEGACY: &str = "Legacy text sources have no page completeness metadata.";

fn frame(kind: &str, label: &str, text: &str) -> String {
    let u = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
    String::from_utf16(&prompt_framing::framing::frame_untrusted(
        &u(kind),
        &u(label),
        &u(text),
    ))
    .unwrap()
}

fn file(name: &str, content: &str) -> ReadableFile {
    ReadableFile::plain(JValue::obj([
        ("name", JValue::Str(name.into())),
        ("content", JValue::Str(content.into())),
    ]))
}

fn request<'a>(project: &'a JValue, files: Vec<ReadableFile>, query: &'a str) -> Request<'a> {
    Request {
        project_id: project,
        readable: (0..files.len()).collect(),
        files,
        query,
        user: "0123abcd-0000-4000-8000-0000000000c1",
        attached: false,
        images: false,
    }
}

fn hit(file: &str, body: &str, score: f64) -> Hit {
    Hit {
        file: file.into(),
        body: body.into(),
        score,
    }
}

/// Runs [`answer_with`], counting the searches it asks; `hits` is the search's answer.
fn run(request: &Request<'_>, rag: bool, hits: Outcome) -> (Native, usize, Option<String>) {
    let asked = Cell::new(0);
    let project = Cell::new(None);
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let out = rt.block_on(answer_with(
        request,
        rag,
        context::DEFAULT_MAX_CHARS,
        |p: String| {
            asked.set(asked.get() + 1);
            project.set(Some(p));
            async move { hits }
        },
    ));
    (out, asked.get(), project.take())
}

fn answered(native: Native) -> (Option<String>, Option<(JValue, JValue)>) {
    match native {
        Native::Answer(Answer::Answered { block, reported }) => (block, reported),
        other => panic!("not answered: {other:?}"),
    }
}

fn json(v: &JValue) -> String {
    js_json::stringify(v).unwrap()
}

#[test]
fn no_files_is_null_and_reports_nothing() {
    let project = JValue::Str("qa-proj".into());
    let req = request(&project, Vec::new(), "anything");
    let (out, asked, _) = run(&req, true, Outcome::Hits(Vec::new()));
    assert_eq!(answered(out), (None, None));
    assert_eq!(asked, 0);
}

#[test]
fn small_files_are_injected_whole_and_named_ones_are_reported() {
    let project = JValue::Str("qa-proj".into());
    let files = vec![file("qa-notes.md", "alpha beta"), file("qa-a.txt", "gamma")];
    let req = request(&project, files, "what does qa-notes.md say?");
    let (out, asked, _) = run(&req, true, Outcome::Hits(vec![hit("qa-a.txt", "x", 0.9)]));
    // No file is over DIRECT_INJECT_MAX: no search.
    assert_eq!(asked, 0);
    let (block, reported) = answered(out);
    let small = frame("file", "qa-a.txt", "gamma");
    let notes = frame("file", "qa-notes.md", "alpha beta");
    // Smallest frame first (Array.prototype.sort by text length).
    let expected = [
        "Sources attached to this project (2): \"qa-notes.md\", \"qa-a.txt\". Excerpts of the relevant ones follow; ask to read a file in full if you need more of it.".to_owned(),
        format!("Source completeness: {LEGACY}{COVERAGE_TAIL}"),
        small,
        notes,
    ]
    .join("\n\n");
    assert_eq!(block.as_deref(), Some(expected.as_str()));
    let (placed, injected) = reported.unwrap();
    // #1457: a whole file is a reply source only when the question names it.
    assert_eq!(
        json(&placed),
        r#"[{"file":"qa-notes.md","body":"alpha beta","kind":"file"}]"#
    );
    assert_eq!(
        json(&injected),
        r#"[{"file":"qa-a.txt","body":"gamma","kind":"file"},{"file":"qa-notes.md","body":"alpha beta","kind":"file"}]"#
    );
}

#[test]
fn a_large_file_is_searched_once_and_only_listed_files_hits_are_kept() {
    let project = JValue::Str("qa-proj".into());
    let long = "word ".repeat(1000);
    let files = vec![file("qa-long.md", &long), file("qa-s.md", "tiny")];
    let req = request(&project, files, "the question");
    let hits = vec![
        hit("qa-long.md", "an excerpt", 0.812),
        hit("qa-gone.md", "a file no longer listed", 0.9),
    ];
    let (out, asked, asked_for) = run(&req, true, Outcome::Hits(hits));
    assert_eq!(asked, 1);
    assert_eq!(asked_for.as_deref(), Some("qa-proj"));
    let (block, reported) = answered(out);
    let block = block.unwrap();
    let excerpt = frame("excerpt", "qa-long.md", "an excerpt");
    assert!(block.contains(&excerpt), "{block}");
    assert!(!block.contains("no longer listed"));
    // With an excerpt placed, the large file's head is not added; the small file still is.
    assert!(!block.contains("file excerpts"));
    assert!(block.contains(&frame("file", "qa-s.md", "tiny")));
    let (placed, injected) = reported.unwrap();
    assert_eq!(
        json(&placed),
        r#"[{"file":"qa-long.md","body":"an excerpt","score":0.812,"kind":"excerpt"}]"#
    );
    assert_eq!(
        json(&injected),
        r#"[{"file":"qa-long.md","body":"an excerpt","score":0.812,"kind":"excerpt"},{"file":"qa-s.md","body":"tiny","kind":"file"}]"#
    );
}

#[test]
fn without_hits_or_without_the_index_a_large_file_gives_its_head() {
    let project = JValue::Str("qa-proj".into());
    let long = format!("{}{}", "a".repeat(24_000), "TAIL");
    for (rag, searches) in [(true, 1), (false, 0)] {
        let req = request(&project, vec![file("qa-long.md", &long)], "q");
        let (out, asked, _) = run(&req, rag, Outcome::Hits(Vec::new()));
        assert_eq!(asked, searches, "rag {rag}");
        let (block, reported) = answered(out);
        let block = block.unwrap();
        assert!(block.contains(&frame("file excerpts", "qa-long.md", &"a".repeat(24_000))));
        assert!(!block.contains("TAIL"));
        let (placed, injected) = reported.unwrap();
        // Not named by the question: injected, not a reply source.
        assert_eq!(json(&placed), "[]");
        assert_eq!(injected.as_arr_len(), 1);
    }
}

#[test]
fn a_search_that_defers_defers_the_whole_context() {
    let project = JValue::Str("qa-proj".into());
    let req = request(&project, vec![file("qa-long.md", &"b".repeat(3000))], "q");
    let (out, asked, _) = run(&req, true, Outcome::Defer("shared engine target"));
    assert_eq!(asked, 1);
    assert_eq!(out, Native::Defer("shared engine target"));
}

#[test]
fn images_stay_with_node_and_odd_values_defer() {
    let project = JValue::Str("qa-proj".into());
    let mut req = request(&project, vec![file("qa.md", "x")], "q");
    req.images = true;
    assert_eq!(
        run(&req, true, Outcome::Hits(Vec::new())).0,
        Native::Defer("images")
    );

    let number = JValue::Num(7.0);
    let req = request(&number, vec![file("qa.md", "x")], "q");
    assert_eq!(
        run(&req, true, Outcome::Hits(Vec::new())).0,
        Native::Defer("project id")
    );

    let nameless = ReadableFile::plain(JValue::obj([
        ("name", JValue::Num(1.0)),
        ("content", JValue::Str("x".into())),
    ]));
    let req = request(&project, vec![nameless], "q");
    assert_eq!(
        run(&req, true, Outcome::Hits(Vec::new())).0,
        Native::Defer("file name")
    );

    let no_text = ReadableFile::plain(JValue::obj([("name", JValue::Str("qa.md".into()))]));
    let req = request(&project, vec![no_text], "q");
    assert_eq!(
        run(&req, true, Outcome::Hits(Vec::new())).0,
        Native::Defer("file content")
    );
}

#[test]
fn a_nullish_file_throws_before_any_search() {
    let project = JValue::Str("qa-proj".into());
    let files = vec![
        file("qa-long.md", &"c".repeat(5000)),
        ReadableFile::plain(JValue::Null),
    ];
    let req = request(&project, files, "q");
    let (out, asked, _) = run(&req, true, Outcome::Hits(Vec::new()));
    assert_eq!(out, Native::Answer(Answer::Threw));
    assert_eq!(asked, 0);
    // chat.cjs: the turn fails before its system message.
    assert!(retrieved(Some(&req), Answer::Threw).is_err());

    // documentNotice throws too: `(d.pageStatus || []).filter` on a truthy non-array.
    let broken = ReadableFile::plain(JValue::obj([
        ("name", JValue::Str("qa-doc.pdf".into())),
        ("content", JValue::Str("t".into())),
        (
            "document",
            JValue::obj([("pageStatus", JValue::Str("x".into()))]),
        ),
    ]));
    let req = request(
        &project,
        vec![file("qa-long.md", &"c".repeat(5000)), broken],
        "q",
    );
    let (out, asked, _) = run(&req, true, Outcome::Hits(Vec::new()));
    assert_eq!(out, Native::Answer(Answer::Threw));
    assert_eq!(asked, 0);
}

#[test]
fn attachment_reasons_and_document_notices_fill_the_completeness_line() {
    let project = JValue::Str("qa-proj".into());
    let with_reason = ReadableFile::plain(JValue::obj([
        ("name", JValue::Str("qa-scan.pdf".into())),
        ("content", JValue::Str("text".into())),
        (
            "attachment",
            JValue::obj([("reason", JValue::Str("only page 1 was read".into()))]),
        ),
    ]));
    let req = request(&project, vec![with_reason], "q");
    let (block, _) = answered(run(&req, true, Outcome::Hits(Vec::new())).0);
    let block = block.unwrap();
    assert!(
        block.contains("Source completeness: \"qa-scan.pdf\": only page 1 was read\nContext may contain excerpts only."),
        "{block}"
    );

    let odd = ReadableFile::plain(JValue::obj([
        ("name", JValue::Str("qa.md".into())),
        ("content", JValue::Str("t".into())),
        ("attachment", JValue::obj([("reason", JValue::Num(3.0))])),
    ]));
    let req = request(&project, vec![odd], "q");
    assert_eq!(
        run(&req, true, Outcome::Hits(Vec::new())).0,
        Native::Defer("attachment reason")
    );

    // A document's notice wins over the reason (documentNotice(f) || ...), and it is
    // document-sources.cjs notice's own text.
    let doc = JValue::obj([
        ("name", JValue::Str("qa-doc.pdf".into())),
        ("content", JValue::Str("page text".into())),
        (
            "document",
            JValue::obj([("indexing", JValue::Str("ready".into()))]),
        ),
        (
            "attachment",
            JValue::obj([("reason", JValue::Str("ignored".into()))]),
        ),
    ]);
    let notice = document_read::notice(&doc).unwrap();
    assert!(!notice.is_empty());
    let req = request(&project, vec![ReadableFile::plain(doc)], "q");
    let (block, _) = answered(run(&req, true, Outcome::Hits(Vec::new())).0);
    let block = block.unwrap();
    assert!(block.contains(&format!("Source completeness: {notice}{COVERAGE_TAIL}")));
    assert!(!block.contains("ignored"));
}

#[test]
fn an_external_source_is_sorted_by_its_saved_length() {
    let project = JValue::Str("qa-proj".into());
    let external = |chars: f64, content: &str| ReadableFile {
        file: JValue::obj([
            ("name", JValue::Str("qa-ext.md".into())),
            ("content", JValue::Str(content.into())),
            ("textRef", JValue::Str("a".repeat(64))),
            ("chars", JValue::Num(chars)),
        ]),
        external: true,
    };
    // Same side of DIRECT_INJECT_MAX: answered; the large one is searched.
    let req = request(&project, vec![external(3000.0, &"d".repeat(3000))], "q");
    assert_eq!(run(&req, true, Outcome::Hits(Vec::new())).1, 1);
    // Saved length small, text large (or the reverse): Node and the composition would disagree.
    let req = request(&project, vec![external(10.0, &"d".repeat(3000))], "q");
    assert_eq!(
        run(&req, true, Outcome::Hits(Vec::new())).0,
        Native::Defer("source length")
    );
    let req = request(&project, vec![external(9000.0, "short")], "q");
    assert_eq!(
        run(&req, true, Outcome::Hits(Vec::new())).0,
        Native::Defer("source length")
    );
    // Not external (no getter in Node): the text's length counts, whatever `chars` says.
    let mut plain = external(10.0, &"d".repeat(3000));
    plain.external = false;
    let req = request(&project, vec![plain], "q");
    assert_eq!(run(&req, true, Outcome::Hits(Vec::new())).1, 1);
    // A `chars` that is not a safe integer falls back to the text's length.
    let req = request(&project, vec![external(10.5, &"d".repeat(3000))], "q");
    assert_eq!(run(&req, true, Outcome::Hits(Vec::new())).1, 1);
}

#[test]
fn an_attachments_project_is_worded_as_the_users_files() {
    let project = JValue::Str("qa-proj".into());
    let mut req = request(&project, vec![file("qa.md", "x")], "q");
    req.attached = true;
    let (block, _) = answered(run(&req, true, Outcome::Hits(Vec::new())).0);
    assert!(block
        .unwrap()
        .starts_with("The user attached these files to this chat (1): \"qa.md\"."));
}

#[test]
fn the_answer_feeds_the_turns_sources_as_nodes_does() {
    let project = JValue::Str("qa-proj".into());
    let long = "e".repeat(4000);
    let files = vec![file("qa-long.md", &long)];
    let req = request(&project, files, "q");
    let (out, _, _) = run(
        &req,
        true,
        Outcome::Hits(vec![hit("qa-long.md", "the part", 0.5)]),
    );
    let Native::Answer(answer) = out else {
        panic!("deferred")
    };
    let state = retrieved(Some(&req), answer).unwrap();
    assert!(state.files_block.is_some());
    assert_eq!(state.placed.len(), 1);
    assert_eq!(state.injected.len(), 1);
    assert_eq!(state.reply_sources.len(), 1);
    assert_eq!(
        state.reply_sources[0].get("file").as_str(),
        Some("qa-long.md")
    );
    // An excerpt of a 4,000-unit file is a partial read (#1513).
    assert_eq!(state.partial_reads.len(), 1);
}

#[test]
fn without_a_readable_store_the_account_counts_as_present() {
    // workspace.cjs isRemoved fails open: a store that cannot be read is not a removed account.
    let dir = tempfile::tempdir().unwrap();
    let user = "0123abcd-0000-4000-8000-0000000000c1";
    assert!(account_present(dir.path(), user));
    // No outbox: nothing tombstoned.
    assert!(searchable(dir.path(), user, "qa-proj"));
}

#[test]
fn safe_integers_are_javascripts() {
    assert!(safe_integer(0.0));
    assert!(safe_integer(-3.0));
    assert!(safe_integer(9_007_199_254_740_991.0));
    assert!(!safe_integer(9_007_199_254_740_992.0));
    assert!(!safe_integer(1.5));
    assert!(!safe_integer(f64::NAN));
    assert!(!safe_integer(f64::INFINITY));
    assert!(text_ref(&JValue::Str("0".repeat(64))));
    assert!(!text_ref(&JValue::Str("A".repeat(64))));
    assert!(!text_ref(&JValue::Str("0".repeat(63))));
    assert!(!text_ref(&JValue::Null));
}

trait ArrLen {
    fn as_arr_len(&self) -> usize;
}

impl ArrLen for JValue {
    fn as_arr_len(&self) -> usize {
        match self {
            JValue::Arr(a) => a.len(),
            _ => usize::MAX,
        }
    }
}
