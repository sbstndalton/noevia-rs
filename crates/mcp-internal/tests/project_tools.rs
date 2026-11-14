//! core's `server/mcp-internal-tools.test.cjs`, the project cases, case by case (the Diary cases stay
//! with core), plus the paths those tests leave implicit (pin refusals, size limits, expectedCount
//! coercion, clipping, the write's expectations) and one call through [`Handler`] end to end.
//!
//! `harness()` is core's: project `proj-a` with notes.md, a folder file and a document; `writeTextFile`
//! changes the stored file or adds one. planEdit is faked as the rule those tests rely on (a plain text
//! file under its own name is editable; a folder file and a document are refused; a missing name lists
//! what there is); planEdit itself is the `project-edit-target` crate's, with its own oracle.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use js_json::JValue;
use mcp_internal::project_tools::{
    self, EditPlan, Expect, Hit, Ports, Project, ProjectFile, MAX_TEXT_BYTES,
};
use mcp_internal::{mint, Ctx, Handler, Mint, Step};
use std::cell::RefCell;

type SearchFn = Box<dyn Fn() -> Result<Vec<Hit>, String>>;

struct Fake {
    project: RefCell<Project>,
    written: RefCell<Vec<(String, String, Option<Expect>)>>,
    rag: bool,
    search: SearchFn,
    searched: RefCell<Vec<(String, String, JValue)>>,
    cap: usize,
}

fn file(name: &str, content: &str) -> ProjectFile {
    ProjectFile {
        name: name.into(),
        content: Some(content.into()),
        size: Some(js_json::js_len(content)),
        ..ProjectFile::default()
    }
}

fn harness() -> Fake {
    Fake {
        project: RefCell::new(Project {
            id: "proj-a".into(),
            files: vec![
                file("notes.md", "alpha\nbeta\nalpha\n"),
                ProjectFile {
                    source: Some("Documents/Shared".into()),
                    ..file("synced.md", "from a folder")
                },
                ProjectFile {
                    document: true,
                    ..file("scan.pdf", "page text")
                },
            ],
        }),
        written: RefCell::new(vec![]),
        rag: true,
        search: Box::new(|| Ok(vec![])),
        searched: RefCell::new(vec![]),
        cap: 8000,
    }
}

impl Ports for Fake {
    fn cap(&self) -> usize {
        self.cap
    }
    fn get_project(&self, id: &JValue) -> Option<Project> {
        let p = self.project.borrow();
        (id.as_str() == Some(p.id.as_str())).then(|| p.clone())
    }
    fn read_project_file(&self, project: &Project, args: &JValue) -> Result<Option<String>, String> {
        Ok(Some(format!(
            "read {} from {}",
            js_json::to_js_string(args.get("name")).unwrap(),
            project.id
        )))
    }
    fn rag_available(&self) -> bool {
        self.rag
    }
    fn search(&self, project_id: &str, query: &str, user_id: &JValue) -> Result<Vec<Hit>, String> {
        self.searched
            .borrow_mut()
            .push((project_id.into(), query.into(), user_id.clone()));
        (self.search)()
    }
    fn write_text_file(
        &self,
        _project: &Project,
        name: &str,
        text: &str,
        expect: Option<&Expect>,
    ) -> Result<(), String> {
        self.written
            .borrow_mut()
            .push((name.into(), text.into(), expect.cloned()));
        let mut p = self.project.borrow_mut();
        match p.files.iter_mut().find(|f| f.name == name) {
            Some(f) => f.content = Some(text.into()),
            None => p.files.push(file(name, text)),
        }
        Ok(())
    }
    fn upload_path_for(&self, _project: &Project, name: &str) -> String {
        format!("uploads/{name}")
    }
    fn plan_edit(&self, project: &Project, name: &JValue) -> Result<EditPlan, String> {
        let wanted = js_json::to_js_string(name).unwrap();
        let Some(f) = project.files.iter().find(|f| f.name == wanted) else {
            let names: Vec<&str> = project.files.iter().map(|f| f.name.as_str()).collect();
            return Err(format!(
                "there is no file called {wanted:?} in this project. Available: {}",
                names.join(", ")
            ));
        };
        if f.source.is_some() {
            return Err(format!("\"{}\" comes from an attached folder", f.name));
        }
        if f.document {
            return Err(format!("\"{}\" is not a plain text file", f.name));
        }
        Ok(EditPlan {
            file: f.clone(),
            write_name: f.name.clone(),
            target: f.name.clone(),
            account: None,
            adopt: false,
        })
    }
    fn target_digest(&self, target: &str, account: Option<&str>) -> String {
        format!("digest({target},{})", account.unwrap_or(""))
    }
}

fn ctx() -> Ctx {
    Ctx {
        user_id: "alice".into(),
        project_id: "proj-a".into(),
        chat_id: JValue::Null,
        edit_target: None,
    }
}

/// `PINNED(stored)`: an approved edit's context, pinned to the stored file the card showed.
fn pinned(stored: &str) -> Ctx {
    Ctx {
        edit_target: Some(format!("digest({stored},)")),
        ..ctx()
    }
}

fn args(json: &str) -> JValue {
    js_json::parse(json).unwrap()
}

fn run(f: &Fake, name: &str, a: &str, c: &Ctx) -> Result<Option<String>, String> {
    project_tools::call(f, name, &args(a), c).unwrap()
}

fn ok(f: &Fake, name: &str, a: &str, c: &Ctx) -> String {
    run(f, name, a, c).unwrap().unwrap()
}

fn err(f: &Fake, name: &str, a: &str, c: &Ctx) -> String {
    run(f, name, a, c).unwrap_err()
}

fn content(f: &Fake, name: &str) -> String {
    let p = f.project.borrow();
    p.files
        .iter()
        .find(|x| x.name == name)
        .and_then(|x| x.content.clone())
        .unwrap()
}

#[test]
fn a_project_tool_outside_a_project_says_so_instead_of_failing_obscurely() {
    let f = harness();
    let none = Ctx {
        project_id: JValue::Null,
        ..ctx()
    };
    for name in project_tools::NAMES {
        let e = err(&f, name, r#"{"name":"notes.md","query":"a","text":"x","find":"a","replace":"b"}"#, &none);
        assert!(e.contains("not in a project"), "{name}: {e}");
    }
}

#[test]
fn identity_comes_from_the_token_never_from_the_arguments() {
    let f = harness();
    let out = ok(
        &f,
        "project_read_file",
        r#"{"name":"notes.md","projectId":"proj-b","userId":"bob"}"#,
        &ctx(),
    );
    assert_eq!(out, "read notes.md from proj-a");
    // Search is asked for the token's project and user.
    ok(&f, "project_search", r#"{"query":"alpha","userId":"bob"}"#, &ctx());
    assert_eq!(
        f.searched.borrow().as_slice(),
        [("proj-a".to_owned(), "alpha".to_owned(), JValue::from("alice"))]
    );
}

#[test]
fn search_filters_stale_index_rows_against_the_current_file_list() {
    let f = Fake {
        search: Box::new(|| {
            Ok(vec![
                Hit {
                    file: "notes.md".into(),
                    body: "alpha".into(),
                },
                Hit {
                    file: "detached.md".into(),
                    body: "should not surface".into(),
                },
            ])
        }),
        ..harness()
    };
    let out = ok(&f, "project_search", r#"{"query":"alpha"}"#, &ctx());
    assert_eq!(out, "[from notes.md] alpha");
}

#[test]
fn search_without_an_index_says_so_rather_than_returning_nothing() {
    let f = Fake {
        rag: false,
        search: Box::new(|| panic!("should not be called")),
        ..harness()
    };
    let out = ok(&f, "project_search", r#"{"query":"alpha"}"#, &ctx());
    assert_eq!(out, "Retrieval is not available on this deployment, so the project could not be searched. Read a file by name instead.");
}

#[test]
fn search_needs_a_query_and_names_an_empty_answer() {
    let f = harness();
    for a in [r#"{}"#, r#"{"query":"  "}"#, r#"{"query":0}"#, r#"{"query":null}"#] {
        assert_eq!(err(&f, "project_search", a, &ctx()), "a query is required", "{a}");
    }
    assert_eq!(
        ok(&f, "project_search", r#"{"query":" alpha \n"}"#, &ctx()),
        "Nothing in this project matched \"alpha\"."
    );
    // `String(query)` of a number.
    ok(&f, "project_search", r#"{"query":12}"#, &ctx());
    assert_eq!(f.searched.borrow().last().unwrap().1, "12");
    // A failing search is the tool's error.
    let broken = Fake {
        search: Box::new(|| Err("index offline".into())),
        ..harness()
    };
    assert_eq!(err(&broken, "project_search", r#"{"query":"a"}"#, &ctx()), "index offline");
}

#[test]
fn replace_requires_the_expected_number_of_matches_and_never_writes_on_a_mismatch() {
    let f = harness();
    let e = err(&f, "project_replace_text", r#"{"name":"notes.md","find":"alpha","replace":"A"}"#, &pinned("notes.md"));
    assert_eq!(e, "expected 1 match but found 2 in \"notes.md\", so nothing was changed");
    let e = err(&f, "project_replace_text", r#"{"name":"notes.md","find":"nowhere","replace":"A"}"#, &pinned("notes.md"));
    assert_eq!(e, "that exact text is not in \"notes.md\", so nothing was changed");
    assert!(f.written.borrow().is_empty(), "a mismatch must not write");
    assert_eq!(content(&f, "notes.md"), "alpha\nbeta\nalpha\n");

    let out = ok(&f, "project_replace_text", r#"{"name":"notes.md","find":"alpha","replace":"A","expectedCount":2}"#, &pinned("notes.md"));
    assert_eq!(out, "Replaced 2 occurrences in \"notes.md\".");
    assert_eq!(content(&f, "notes.md"), "A\nbeta\nA\n");
    // The write is pinned to what was read: name, content, no attachment, no adoption.
    let w = f.written.borrow();
    assert_eq!(
        w[0].2,
        Some(Expect {
            expect_name: "notes.md".into(),
            expect_content: "alpha\nbeta\nalpha\n".into(),
            expect_attachment: None,
            adopt: None,
        })
    );
}

#[test]
fn replace_expected_count_is_coerced_as_number_and_must_be_a_positive_whole_number() {
    let f = harness();
    let p = pinned("notes.md");
    for bad in ["0", "-1", "1.5", "\"x\"", "null", "{}", "false", "[]"] {
        let a = format!(r#"{{"name":"notes.md","find":"beta","replace":"B","expectedCount":{bad}}}"#);
        assert_eq!(
            err(&f, "project_replace_text", &a, &p),
            "expectedCount must be a positive whole number",
            "{bad}"
        );
    }
    // Number("2") is 2, [2] is 2, true is 1, " 0x2 " is 2: all accepted as counts.
    assert_eq!(
        err(&f, "project_replace_text", r#"{"name":"notes.md","find":"beta","replace":"B","expectedCount":"2"}"#, &p),
        "expected 2 matches but found 1 in \"notes.md\", so nothing was changed"
    );
    assert_eq!(
        err(&f, "project_replace_text", r#"{"name":"notes.md","find":"alpha","replace":"B","expectedCount":true}"#, &p),
        "expected 1 match but found 2 in \"notes.md\", so nothing was changed"
    );
    assert_eq!(
        ok(&f, "project_replace_text", r#"{"name":"notes.md","find":"alpha","replace":"B","expectedCount":[2]}"#, &p),
        "Replaced 2 occurrences in \"notes.md\"."
    );
    assert_eq!(
        ok(&f, "project_replace_text", r#"{"name":"notes.md","find":"beta","replace":"C","expectedCount":" 0x1 "}"#, &p),
        "Replaced 1 occurrence in \"notes.md\"."
    );
    assert_eq!(content(&f, "notes.md"), "B\nC\nB\n");
}

#[test]
fn replace_needs_a_find_and_replaces_with_string_of_the_argument() {
    let f = harness();
    let p = pinned("notes.md");
    for a in [
        r#"{"name":"notes.md","find":"","replace":"x"}"#,
        r#"{"name":"notes.md","replace":"x"}"#,
        r#"{"name":"notes.md","find":null,"replace":"x"}"#,
    ] {
        assert_eq!(err(&f, "project_replace_text", a, &p), "find must not be empty", "{a}");
    }
    // `replace` missing is ''; a number is its String.
    ok(&f, "project_replace_text", r#"{"name":"notes.md","find":"beta\n"}"#, &p);
    assert_eq!(content(&f, "notes.md"), "alpha\nalpha\n");
    ok(&f, "project_replace_text", r#"{"name":"notes.md","find":"alpha","replace":7,"expectedCount":2}"#, &p);
    assert_eq!(content(&f, "notes.md"), "7\n7\n");
    // Matches do not overlap (split): "aa" in "aaa" is one.
    let g = harness();
    g.project.borrow_mut().files[0].content = Some("aaa".into());
    assert_eq!(
        ok(&g, "project_replace_text", r#"{"name":"notes.md","find":"aa","replace":"b"}"#, &p),
        "Replaced 1 occurrence in \"notes.md\"."
    );
    assert_eq!(content(&g, "notes.md"), "ba");
}

#[test]
fn folder_derived_and_non_text_files_are_refused_rather_than_silently_reverted() {
    let f = harness();
    for name in ["synced.md", "scan.pdf"] {
        let a = format!(r#"{{"name":"{name}","text":"x"}}"#);
        assert!(run(&f, "project_append_file", &a, &ctx()).is_err());
        let a = format!(r#"{{"name":"{name}","find":"a","replace":"b"}}"#);
        assert!(run(&f, "project_replace_text", &a, &ctx()).is_err());
        // Pinned or not, planEdit refuses first.
        let a = format!(r#"{{"name":"{name}","text":"x"}}"#);
        assert!(run(&f, "project_append_file", &a, &pinned(name)).is_err());
    }
    assert!(f.written.borrow().is_empty());
}

#[test]
fn create_refuses_to_clobber_append_keeps_what_is_there() {
    let f = harness();
    assert_eq!(
        err(&f, "project_create_file", r#"{"name":"notes.md","text":"new"}"#, &ctx()),
        "\"notes.md\" already exists. Use project_append_file or project_replace_text."
    );
    assert_eq!(
        ok(&f, "project_create_file", r#"{"name":"fresh.md","text":"hello"}"#, &ctx()),
        "Created \"fresh.md\" (5 chars)."
    );
    assert_eq!(content(&f, "fresh.md"), "hello");
    assert_eq!(f.written.borrow()[0].2, None, "a create carries no expectations");

    assert_eq!(
        ok(&f, "project_append_file", r#"{"name":"fresh.md","text":"world"}"#, &pinned("fresh.md")),
        "Appended 5 chars to \"fresh.md\"."
    );
    assert_eq!(content(&f, "fresh.md"), "hello\nworld");
    // A trailing newline is not doubled.
    ok(&f, "project_append_file", r#"{"name":"notes.md","text":"gamma"}"#, &pinned("notes.md"));
    assert_eq!(content(&f, "notes.md"), "alpha\nbeta\nalpha\ngamma");
}

#[test]
fn create_refuses_a_name_the_folder_already_stores_under_its_upload_path() {
    let f = harness();
    f.project.borrow_mut().files.push(file("uploads/report.md", "x"));
    assert_eq!(
        err(&f, "project_create_file", r#"{"name":"report.md","text":"y"}"#, &ctx()),
        "\"report.md\" already exists. Use project_append_file or project_replace_text."
    );
}

#[test]
fn append_to_an_empty_file_adds_no_newline_and_needs_text() {
    let f = harness();
    f.project.borrow_mut().files.push(file("empty.md", ""));
    for a in [r#"{"name":"empty.md","text":""}"#, r#"{"name":"empty.md"}"#] {
        assert_eq!(err(&f, "project_append_file", a, &pinned("empty.md")), "there is nothing to append");
    }
    ok(&f, "project_append_file", r#"{"name":"empty.md","text":"first"}"#, &pinned("empty.md"));
    assert_eq!(content(&f, "empty.md"), "first");
    // Character counts are UTF-16 units, as in JS.
    assert_eq!(
        ok(&f, "project_append_file", r#"{"name":"empty.md","text":"😀é"}"#, &pinned("empty.md")),
        "Appended 3 chars to \"empty.md\"."
    );
}

#[test]
fn a_path_in_a_filename_cannot_escape_the_project() {
    let f = harness();
    ok(&f, "project_create_file", r#"{"name":"../../etc/passwd","text":"x"}"#, &ctx());
    let p = f.project.borrow();
    assert!(p.files.iter().any(|x| x.name == "passwd"), "the directory part must be stripped");
    assert!(!p.files.iter().any(|x| x.name.contains('/')), "no file name may contain a path");
}

#[test]
fn a_missing_file_names_what_the_project_does_have() {
    let f = harness();
    let e = err(&f, "project_append_file", r#"{"name":"absent.md","text":"x"}"#, &ctx());
    assert!(e.contains("Available: notes.md, synced.md, scan.pdf"), "{e}");
}

#[test]
fn an_edit_needs_the_pin_and_the_pin_must_still_name_the_resolved_file() {
    let f = harness();
    assert_eq!(
        err(&f, "project_append_file", r#"{"name":"notes.md","text":"x"}"#, &ctx()),
        "this edit was not tied to a file when it was approved, so nothing was changed"
    );
    // An empty `t` is no pin.
    let empty = Ctx {
        edit_target: Some(String::new()),
        ..ctx()
    };
    assert!(err(&f, "project_replace_text", r#"{"name":"notes.md","find":"beta","replace":"b"}"#, &empty)
        .starts_with("this edit was not tied"));
    // Pinned to another file: the name is shown as JSON.stringify(String(name)).
    assert_eq!(
        err(&f, "project_append_file", r#"{"name":"notes.md","text":"x"}"#, &pinned("other.md")),
        "\"notes.md\" now refers to \"notes.md\", which is not the file that was approved. Nothing was changed; ask again."
    );
    assert!(f.written.borrow().is_empty());
}

#[test]
fn writes_over_the_size_limit_are_refused_without_writing() {
    let f = harness();
    let big = "x".repeat(MAX_TEXT_BYTES + 1);
    assert_eq!(
        err(&f, "project_create_file", &format!(r#"{{"name":"big.md","text":"{big}"}}"#), &ctx()),
        "that text is too large to write in one call"
    );
    // Exactly the limit is fine; one more byte by appending is not (bytes, not units: é is 2).
    let edge = "x".repeat(MAX_TEXT_BYTES);
    ok(&f, "project_create_file", &format!(r#"{{"name":"edge.md","text":"{edge}"}}"#), &ctx());
    assert_eq!(
        err(&f, "project_append_file", r#"{"name":"edge.md","text":"y"}"#, &pinned("edge.md")),
        "that would make the file too large to write in one call"
    );
    let near = "x".repeat(MAX_TEXT_BYTES - 1);
    assert_eq!(
        err(&f, "project_create_file", &format!(r#"{{"name":"near.md","text":"{near}é"}}"#), &ctx()),
        "that text is too large to write in one call"
    );
    assert_eq!(
        err(&f, "project_replace_text", r#"{"name":"edge.md","find":"x","replace":"yy","expectedCount":262144}"#, &pinned("edge.md")),
        "that would make the file too large to write in one call"
    );
    assert_eq!(f.written.borrow().len(), 1);
}

#[test]
fn list_files_names_each_kind_and_size_and_clips_at_the_cap() {
    let f = harness();
    f.project.borrow_mut().files.push(ProjectFile {
        name: "image.png".into(),
        ..ProjectFile::default()
    });
    assert_eq!(
        ok(&f, "project_list_files", "{}", &ctx()),
        "Pass a name exactly as shown (or just its file name, when unique) to project_read_file.\n\
         notes.md — text, 17 chars\n\
         synced.md — from folder Documents/Shared, 13 chars\n\
         scan.pdf — document, 9 chars\n\
         image.png — text, no readable text"
    );
    let small = Fake { cap: 20, ..harness() };
    assert_eq!(
        ok(&small, "project_list_files", "{}", &ctx()),
        "Pass a name exactly \n…[truncated]"
    );
    let empty = harness();
    empty.project.borrow_mut().files.clear();
    assert_eq!(ok(&empty, "project_list_files", "{}", &ctx()), "This project has no files attached.");
}

#[test]
fn a_name_outside_the_project_tools_is_the_callers() {
    let f = harness();
    assert!(project_tools::call(&f, "diary_read_today", &args("{}"), &ctx()).is_none());
    assert!(project_tools::call(&f, "unknown", &args("{}"), &ctx()).is_none());
}

#[test]
fn every_tool_converts_for_the_catalogue_and_only_the_three_writes_are_writes() {
    let listed = mcp_internal::catalogue_of(&project_tools::definitions());
    let text = js_json::stringify(&JValue::Arr(listed)).unwrap();
    // The catalogue core lists for these six (createInternalTools → catalogueOf), byte for byte.
    assert_eq!(
        text,
        concat!(
            r#"[{"name":"project_list_files","description":"List the files attached to this chat's project.","inputSchema":{"type":"object","properties":{},"required":[]},"annotations":{"readOnlyHint":true}},"#,
            r#"{"name":"project_read_file","description":"Read one project file by its listed name (or file name, if unique). Use offset to continue a long file.","inputSchema":{"type":"object","properties":{"name":{"type":"string"},"offset":{"type":"integer","description":"characters to skip"},"startPage":{"type":"integer","description":"first page, for documents"},"endPage":{"type":"integer"}},"required":["name"]},"annotations":{"readOnlyHint":true}},"#,
            r#"{"name":"project_search","description":"Search this project's files for a phrase and return matching excerpts.","inputSchema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]},"annotations":{"readOnlyHint":true}},"#,
            r#"{"name":"project_create_file","description":"Create a new text file in this project.","inputSchema":{"type":"object","properties":{"name":{"type":"string"},"text":{"type":"string"}},"required":["name","text"]},"annotations":{"readOnlyHint":false}},"#,
            r#"{"name":"project_append_file","description":"Add text to the end of a file in this project, uploads included. Use this, not Google Drive, for project files.","inputSchema":{"type":"object","properties":{"name":{"type":"string"},"text":{"type":"string"}},"required":["name","text"]},"annotations":{"readOnlyHint":false}},"#,
            r#"{"name":"project_replace_text","description":"Replace exact text in a project file or upload (not Google Drive). expectedCount: matches to require.","inputSchema":{"type":"object","properties":{"name":{"type":"string"},"find":{"type":"string"},"replace":{"type":"string"},"expectedCount":{"type":"integer","description":"how many matches you expect; defaults to 1"}},"required":["name","find","replace"]},"annotations":{"readOnlyHint":false}}]"#,
        )
    );
}

const KEY: &[u8] = b"qa-key-0123456789abcdef0123456789";
const NOW: f64 = 1_700_000_000_000.0;

fn rpc_call(name: &str, a: &str) -> JValue {
    args(&format!(
        r#"{{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{{"name":"{name}","arguments":{a}}}}}"#
    ))
}

fn through_handler(h: &Handler, f: &Fake, m: &Mint<'_>, jti: &str, name: &str, a: &str) -> JValue {
    let token = mint(KEY, m, jti, NOW);
    let reply = match h.handle(&rpc_call(name, a), &token, NOW) {
        Step::Reply(r) => r,
        Step::Invoke(inv) => {
            inv.finish(project_tools::call(f, &inv.name, &inv.args, &inv.ctx).unwrap())
        }
    };
    assert_eq!(reply.status, 200);
    reply.body.unwrap().get("result").clone()
}

#[test]
fn an_approved_edit_runs_through_the_handler_with_the_token_pin() {
    let h = Handler::new(KEY, project_tools::definitions());
    let f = harness();
    let digest = f.target_digest("notes.md", None);
    let write = Mint {
        uid: Some("alice"),
        pid: Some("proj-a"),
        write: true,
        target: Some(&digest),
        ..Mint::default()
    };
    let out = through_handler(&h, &f, &write, "j-edit", "project_append_file", r#"{"name":"notes.md","text":"gamma"}"#);
    let text = js_json::stringify(&out).unwrap();
    assert_eq!(text, r#"{"content":[{"type":"text","text":"Appended 5 chars to \"notes.md\"."}]}"#);
    // The project in the arguments is ignored; the token's is used.
    let read = Mint {
        uid: Some("alice"),
        pid: Some("proj-a"),
        ..Mint::default()
    };
    let out = through_handler(&h, &f, &read, "j-read", "project_read_file", r#"{"name":"notes.md","projectId":"proj-b"}"#);
    assert_eq!(
        js_json::stringify(&out).unwrap(),
        r#"{"content":[{"type":"text","text":"read notes.md from proj-a"}]}"#
    );
    // A token for no project: the tool's refusal, as an error result.
    let nowhere = Mint {
        uid: Some("alice"),
        ..Mint::default()
    };
    let out = through_handler(&h, &f, &nowhere, "j-none", "project_list_files", "{}");
    assert_eq!(
        js_json::stringify(&out).unwrap(),
        r#"{"content":[{"type":"text","text":"ERROR: this chat is not in a project, so there are no project files"}],"isError":true}"#
    );
}
