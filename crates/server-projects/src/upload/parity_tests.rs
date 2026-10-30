// Included into `upload::tests` (noevia#2191 round 8 review): the findings of the review of the native
// upload and the Node tests the move deleted, ported as the Rust behaviour. The rig, the fakes and the
// helpers of the module above are in scope.

use crate::source_storage::StorageError;

fn legacy_body(name: &str, bytes: &[u8], extra: &str) -> String {
    format!(
        r#"{{"name":"{name}","dataBase64":"{}"{extra}}}"#,
        b64(bytes)
    )
}

const WITH_SOURCE: &str = r#"{"id":"proj-a","name":"A","chats":[],"instructionSkills":{},"projectFolder":"root/proj-a","sourceFolders":["root/proj-a"]}"#;
const NO_FOLDER: &str = r#"{"id":"proj-a","name":"A","chats":[],"instructionSkills":{}}"#;

/// Turns projects.json into something the store cannot read: the next save fails.
fn break_the_store(dir: &std::path::Path) {
    let file = dir.join(store::PROJECTS_FILE);
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
}
fn mend_the_store(dir: &std::path::Path, project: &str) {
    let file = dir.join(store::PROJECTS_FILE);
    std::fs::remove_dir(&file).unwrap();
    std::fs::write(file, format!("{{\"projects\":[{project}]}}")).unwrap();
}

fn discards(rig: &Rig) -> Vec<String> {
    rig.storage
        .log()
        .into_iter()
        .filter(|l| l.starts_with("DISCARD "))
        .collect()
}

fn plan_entries(rig: &Rig, entries: &str, remote: bool) -> (u16, JValue) {
    let r = plan(
        rig.ctx(),
        "proj-a",
        format!(r#"{{"entries":{entries}}}"#).as_bytes(),
        false,
        remote,
    );
    let Body::Json(t) = &r.body else { panic!() };
    (r.status, wtf16::parse(t).unwrap())
}

fn import_one(rig: &Rig, remote: bool, id: &str, rel: &str, content: &[u8]) -> (u16, JValue) {
    let name = rel.rsplit('/').next().unwrap();
    rig.post(
        remote,
        &organized(
            name,
            content,
            &format!(r#","relativePath":"{rel}","importId":"{id}""#),
        ),
    )
}

fn id_of(plan: &JValue) -> String {
    plan.get("importId").as_str().unwrap().to_owned()
}

// ── the account deletion fence (part 1 M3, part 2 M1; core's account-deletion-source.test.cjs) ─────

/// A document being extracted when its account is removed: the original, the result record and
/// projects.json do not bring `users/<id>` back, and the person who left gets 410, not a success.
#[test]
fn an_account_removed_while_its_document_is_extracted_is_not_recreated() {
    let rig = Rig::new(PROJECT);
    let engine = FakeEngine {
        docling: true,
        ..Default::default()
    };
    engine.replies.borrow_mut().push(Ok(wtf16::parse(
        r#"{"total":1,"pages":[{"number":1,"text":"hello","status":"native","method":"docling"}]}"#,
    )
    .unwrap()));
    let dir = rig.dir();
    let progress = |stage: &str| {
        if stage == "Extracting PDF text / OCR" {
            std::fs::remove_dir_all(&dir).unwrap();
        }
    };
    let (status, body) = rig.post_with(
        &engine,
        true,
        &organized("r.pdf", b"%PDF-1.4 synthetic", ""),
        &progress,
    );
    assert_eq!(
        (status, body.get("error").as_str()),
        (410, Some("account no longer exists")),
        "{body:?}"
    );
    assert!(
        !rig.dir().exists(),
        "neither the extraction's files nor projects.json came back"
    );
    assert!(
        !rig.root.path().join("users").join(USER).exists()
            && rig.root.path().join("users").exists(),
        "only the user's own folder is gone"
    );
}

/// The original is saved before extraction: an account removed right before that write stays removed.
#[test]
fn an_account_removed_before_the_original_is_saved_is_not_recreated() {
    let rig = Rig::new(PROJECT);
    let dir = rig.dir();
    let progress = |stage: &str| {
        if stage == "Saving original" {
            std::fs::remove_dir_all(&dir).unwrap();
        }
    };
    let (status, body) = rig.post_with(
        &rig.engine,
        true,
        &organized("notes.md", b"# one", ""),
        &progress,
    );
    assert_eq!(status, 410, "{body:?}");
    assert!(!rig.dir().exists());
    assert!(
        !rig.storage
            .files
            .borrow()
            .contains_key("root/proj-a/Text/notes.md"),
        "nothing was written to storage for them"
    );
}

/// An organized image upload waiting on storage: the removal lands between the storage write and the
/// image asset's file; the asset is not written into a folder made again, and the storage file goes.
#[test]
fn an_account_removed_while_an_image_waits_on_storage_does_not_get_its_asset_files_back() {
    let rig = Rig::new(PROJECT);
    let dir = rig.dir();
    *rig.storage.after_write.borrow_mut() = Some(Box::new(move |_, _| {
        let _ = std::fs::remove_dir_all(&dir);
    }));
    let image = [b"\x89PNG\r\n\x1a\n".as_slice(), &[0u8; 8]].concat();
    let (status, body) = rig.post(true, &organized("pic.png", &image, ""));
    assert_eq!(status, 410, "{body:?}");
    assert!(
        !rig.dir().exists(),
        "neither project-uploads nor project-assets came back"
    );
    assert!(
        !rig.storage
            .files
            .borrow()
            .contains_key("root/proj-a/Images/pic.png"),
        "the unrecorded write went again"
    );
}

/// An active account still saves originals, extraction and project metadata (the fence changes nothing
/// for it).
#[test]
fn an_active_account_still_saves_its_original_and_its_record() {
    let rig = Rig::new(PROJECT);
    let (status, body) = rig.post(true, &organized("notes.md", b"# one", ""));
    assert_eq!(status, 200, "{body:?}");
    let original = crate::lifecycle::uploads_dir(&rig.dir(), "proj-a");
    assert_eq!(std::fs::read_dir(original).unwrap().count(), 1);
    assert_eq!(rig.files().len(), 1);
}

/// The directories are made below the user's folder, one at a time, and never the user's folder.
#[test]
fn directories_are_made_below_an_existing_user_folder_and_never_the_user_folder_itself() {
    let root = tempfile::tempdir().unwrap();
    let user = root.path().join("users").join("u1");
    let deep = user.join("project-uploads").join("abc");
    let err = store::create_dirs_below(&user, &deep).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(!user.exists() && !root.path().join("users").exists());
    std::fs::create_dir_all(&user).unwrap();
    store::create_dirs_below(&user, &deep).unwrap();
    store::create_dirs_below(&user, &deep).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&deep).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(
        store::create_dirs_below(&user, root.path()).is_err(),
        "a path outside is refused"
    );
}

// ── #1664: a failure after a create-only write removes the file again ───────────────────────────────

/// An organized upload whose project save fails removes the file it just made (conditional on the ETag
/// the PUT returned), the error surfaces, and the retry is not a 409.
#[test]
fn a_failed_save_after_an_organized_write_removes_the_file_and_the_retry_is_not_a_409() {
    let rig = Rig::new(PROJECT);
    let dir = rig.dir();
    let progress = |stage: &str| {
        if stage == "Indexing extracted text" {
            break_the_store(&dir);
        }
    };
    let (status, _) = rig.post_with(
        &rig.engine,
        true,
        &organized("report.md", b"NEW", ""),
        &progress,
    );
    assert_eq!(status, 500, "the save's failure surfaces");
    assert_eq!(discards(&rig), vec!["DISCARD root/proj-a/Text/report.md"]);
    assert!(
        !rig.storage
            .files
            .borrow()
            .contains_key("root/proj-a/Text/report.md"),
        "no orphan left in storage"
    );
    mend_the_store(&rig.dir(), PROJECT);
    let (status, body) = rig.post(true, &organized("report.md", b"NEW", ""));
    assert_eq!(status, 200, "the retry is not a 409: {body:?}");
    assert_eq!(
        rig.storage
            .files
            .borrow()
            .get("root/proj-a/Text/report.md")
            .unwrap()
            .0,
        b"NEW"
    );
}

/// A file edited in storage after the write is kept (the conditional delete says 412), and the original
/// error still surfaces; a cleanup that fails does not hide it either.
#[test]
fn a_file_edited_after_the_write_is_kept_and_a_failing_cleanup_does_not_hide_the_error() {
    let rig = Rig::new(PROJECT);
    let dir = rig.dir();
    *rig.storage.after_write.borrow_mut() = Some(Box::new(|s, path| {
        s.files
            .borrow_mut()
            .insert(path.to_owned(), (b"someone edited".to_vec(), 9_999));
    }));
    let progress = |stage: &str| {
        if stage == "Indexing extracted text" {
            break_the_store(&dir);
        }
    };
    let (status, _) = rig.post_with(
        &rig.engine,
        true,
        &organized("report.md", b"NEW", ""),
        &progress,
    );
    assert_eq!(status, 500);
    assert_eq!(discards(&rig).len(), 1, "one conditional delete");
    assert_eq!(
        rig.storage
            .files
            .borrow()
            .get("root/proj-a/Text/report.md")
            .unwrap()
            .0,
        b"someone edited",
        "an edited file is kept"
    );
    // A cleanup that fails outright.
    mend_the_store(&rig.dir(), PROJECT);
    *rig.storage.after_write.borrow_mut() = None;
    *rig.storage.discard_override.borrow_mut() = Some((false, "error"));
    let progress = |stage: &str| {
        if stage == "Indexing extracted text" {
            break_the_store(&dir);
        }
    };
    let (status, _) = rig.post_with(
        &rig.engine,
        true,
        &organized("other.md", b"NEW", ""),
        &progress,
    );
    assert_eq!(status, 500, "the original error, not the cleanup's");
    assert_eq!(discards(&rig).len(), 2);
}

/// A replace never deletes, even when the save fails; the same failure on a new name does.
#[test]
fn a_replace_never_deletes_even_when_the_save_fails() {
    let recorded = format!(
        r#"{{"id":"proj-a","name":"A","chats":[],"instructionSkills":{{}},"projectFolder":"root/proj-a","files":[{{"name":"root/proj-a/Text/report.md","content":"old","source":"root/proj-a","attachment":{{"id":"{}","bytes":3,"group":"Text","state":"ready"}}}}]}}"#,
        "a".repeat(64)
    );
    let rig = Rig::new(&recorded);
    rig.storage.put("root/proj-a/Text/report.md", b"old");
    let dir = rig.dir();
    let breaking = |stage: &str| {
        if stage == "Indexing extracted text" {
            break_the_store(&dir);
        }
    };
    let (status, _) = rig.post_with(
        &rig.engine,
        true,
        &organized("other.md", b"OTHER", ""),
        &breaking,
    );
    assert_eq!(status, 500);
    assert_eq!(discards(&rig), vec!["DISCARD root/proj-a/Text/other.md"]);
    mend_the_store(&rig.dir(), &recorded);
    let (status, _) = rig.post_with(
        &rig.engine,
        true,
        &organized("report.md", b"NEW", r#","replace":true"#),
        &breaking,
    );
    assert_eq!(status, 500);
    assert_eq!(discards(&rig).len(), 1, "the replace made no delete");
    assert_eq!(
        rig.storage
            .files
            .borrow()
            .get("root/proj-a/Text/report.md")
            .unwrap()
            .0,
        b"NEW"
    );
}

/// The plain storage upload of a PDF: a save that fails, or a project removed mid-upload, removes the
/// written file with If-Match; a replace never deletes.
#[test]
fn a_plain_storage_upload_whose_record_cannot_be_saved_removes_the_file_it_wrote() {
    let engine = FakeEngine {
        docling: true,
        ..Default::default()
    };
    let ready =
        r#"{"total":1,"pages":[{"number":1,"text":"hello","status":"native","method":"docling"}]}"#;
    let rig = Rig::new(WITH_SOURCE);
    engine
        .replies
        .borrow_mut()
        .push(Ok(wtf16::parse(ready).unwrap()));
    let dir = rig.dir();
    *rig.storage.after_write.borrow_mut() = Some(Box::new(move |_, _| break_the_store(&dir)));
    let (status, _) = rig.post_with(
        &engine,
        true,
        &legacy_body("scan.pdf", b"%PDF-1.4 synthetic", ""),
        &|_| {},
    );
    assert_eq!(status, 500);
    assert_eq!(discards(&rig), vec!["DISCARD root/proj-a/scan.pdf"]);
    assert!(!rig
        .storage
        .files
        .borrow()
        .contains_key("root/proj-a/scan.pdf"));
    // Removed mid-upload: the same.
    mend_the_store(&rig.dir(), WITH_SOURCE);
    engine
        .replies
        .borrow_mut()
        .push(Ok(wtf16::parse(ready).unwrap()));
    let dir = rig.dir();
    *rig.storage.after_write.borrow_mut() = Some(Box::new(move |_, _| {
        std::fs::write(dir.join(store::PROJECTS_FILE), r#"{"projects":[]}"#).unwrap();
    }));
    let (status, _) = rig.post_with(
        &engine,
        true,
        &legacy_body("scan.pdf", b"%PDF-1.4 synthetic", ""),
        &|_| {},
    );
    assert_eq!(status, 409);
    assert_eq!(discards(&rig).len(), 2);
    // `replace: true` over the removed project: no delete.
    std::fs::write(
        rig.dir().join(store::PROJECTS_FILE),
        format!("{{\"projects\":[{WITH_SOURCE}]}}"),
    )
    .unwrap();
    engine
        .replies
        .borrow_mut()
        .push(Ok(wtf16::parse(ready).unwrap()));
    let dir = rig.dir();
    *rig.storage.after_write.borrow_mut() = Some(Box::new(move |_, _| {
        std::fs::write(dir.join(store::PROJECTS_FILE), r#"{"projects":[]}"#).unwrap();
    }));
    let (status, _) = rig.post_with(
        &engine,
        true,
        &legacy_body("scan.pdf", b"%PDF-1.4 synthetic", r#","replace":true"#),
        &|_| {},
    );
    assert_eq!(status, 409);
    assert_eq!(discards(&rig).len(), 2, "the replace path made no delete");
}

// ── #849 / #1428: what a refusing storage is called ────────────────────────────────────────────────

fn code(body: &JValue) -> Option<String> {
    body.get("code").as_str().map(str::to_owned)
}

/// The first upload's folder step: a rejected login is reported as that on both paths; any other
/// failure keeps the generic, retryable words.
#[test]
fn a_rejected_login_at_the_folder_step_is_reported_as_that_and_other_failures_stay_generic() {
    let login = StorageError::new(502, "storage returned 401").with_upstream(401);
    let rig = Rig::new(NO_FOLDER);
    rig.storage
        .folder_errors
        .borrow_mut()
        .insert("root".into(), login);
    let (status, body) = rig.post(true, &organized("note.txt", b"synthetic note", ""));
    assert_eq!(
        status, 502,
        "never 401: the client reads that as an expired noevia session"
    );
    assert_eq!(code(&body).as_deref(), Some("storageLoginRejected"));
    assert!(body
        .get("error")
        .as_str()
        .unwrap()
        .starts_with("Storage login rejected. Check your storage credentials in Settings"));
    let (status, body) = rig.post(true, &legacy_body("note.txt", b"synthetic note", ""));
    assert_eq!(
        (status, code(&body).as_deref()),
        (502, Some("storageLoginRejected"))
    );
    // Any other folder failure: the generic words, no code.
    let rig = Rig::new(NO_FOLDER);
    rig.storage.folder_errors.borrow_mut().insert(
        "root".into(),
        StorageError::new(502, "could not create folder (500)"),
    );
    let (status, body) = rig.post(true, &organized("note.txt", b"synthetic note", ""));
    assert_eq!(status, 502);
    assert_eq!(
        body.get("error").as_str(),
        Some("Could not create the storage folder; retry.")
    );
    assert_eq!(code(&body), None);
    let (_, body) = rig.post(true, &legacy_body("note.txt", b"synthetic note", ""));
    assert!(body
        .get("error")
        .as_str()
        .unwrap()
        .starts_with("Could not create the project storage folder"));
    assert_eq!(code(&body), None);
}

/// A folder that exists but whose write is refused: 401 is a rejected login on both paths, 403 its own
/// answer (#1428).
#[test]
fn a_refused_write_is_a_rejected_login_for_401_and_forbidden_for_403_on_both_paths() {
    for (status, expected) in [(401u16, "storageLoginRejected"), (403, "storageForbidden")] {
        let refusal =
            StorageError::new(502, format!("storage returned {status}")).with_upstream(status);
        let rig = Rig::new(WITH_SOURCE);
        rig.storage
            .write_errors
            .borrow_mut()
            .insert("root/proj-a/Text/note.txt".into(), refusal.clone());
        rig.storage
            .write_errors
            .borrow_mut()
            .insert("root/proj-a/note.txt".into(), refusal);
        let (s, body) = rig.post(true, &organized("note.txt", b"synthetic note", ""));
        assert_eq!(
            (s, code(&body).as_deref()),
            (502, Some(expected)),
            "organized {status}"
        );
        let (s, body) = rig.post(true, &legacy_body("note.txt", b"synthetic note", ""));
        assert_eq!(
            (s, code(&body).as_deref()),
            (502, Some(expected)),
            "plain {status}"
        );
    }
}

// ── #1454: an upload never replaces unasked ─────────────────────────────────────────────────────────

/// The plain storage upload is create-only unless replace is sent; a free name saves beside the old
/// one; a file in storage but not yet synced is not overwritten either; without storage a same-name
/// upload is refused too.
#[test]
fn the_plain_upload_is_create_only_and_a_free_name_saves_beside_the_original() {
    let rig = Rig::new(WITH_SOURCE);
    rig.storage.put("root/proj-a/report.md", b"old");
    let (status, body) = rig.post(true, &legacy_body("report.md", b"NEW", ""));
    assert_eq!(
        (status, code(&body).as_deref()),
        (409, Some("sourceExists"))
    );
    assert_eq!(
        rig.storage
            .files
            .borrow()
            .get("root/proj-a/report.md")
            .unwrap()
            .0,
        b"old"
    );
    let (status, _) = rig.post(
        true,
        &legacy_body("report.md", b"NEW", r#","replace":true"#),
    );
    assert_eq!(status, 200);
    assert_eq!(
        rig.storage
            .files
            .borrow()
            .get("root/proj-a/report.md")
            .unwrap()
            .0,
        b"NEW"
    );
    // A numbered copy lives beside the original.
    let (status, _) = rig.post(true, &organized("report (2).md", b"COPY", ""));
    assert_eq!(status, 200);
    assert_eq!(
        rig.storage
            .files
            .borrow()
            .get("root/proj-a/Text/report (2).md")
            .unwrap()
            .0,
        b"COPY"
    );
    // In storage but not in the project yet: not overwritten.
    rig.storage
        .put("root/proj-a/Text/theirs.md", b"someone else wrote this");
    let (status, body) = rig.post(true, &organized("theirs.md", b"NEW", ""));
    assert_eq!(
        (status, code(&body).as_deref()),
        (409, Some("sourceExists"))
    );
    assert_eq!(
        rig.storage
            .files
            .borrow()
            .get("root/proj-a/Text/theirs.md")
            .unwrap()
            .0,
        b"someone else wrote this"
    );
}

#[test]
fn without_storage_a_same_name_upload_is_refused_unless_replace_is_sent() {
    let rig = Rig::new(
        r#"{"id":"proj-a","name":"A","chats":[],"instructionSkills":{},"files":[{"name":"report.md","content":"old"}]}"#,
    );
    let (status, body) = rig.post(false, &legacy_body("report.md", b"NEW", ""));
    assert_eq!(
        (status, code(&body).as_deref()),
        (409, Some("sourceExists"))
    );
    let text = |rig: &Rig| crate::sources::text(&rig.dir(), "proj-a", &rig.files()[0]).unwrap();
    assert_eq!(text(&rig), "old");
    let (status, _) = rig.post(
        false,
        &legacy_body("report.md", b"NEW", r#","replace":true"#),
    );
    assert_eq!(status, 200);
    assert_eq!(rig.files().len(), 1);
    assert_eq!(text(&rig), "NEW");
}

/// #1454 with a reduced file: the name a large PDF is stored under (its text-only extraction) is the one
/// checked for a collision, before anything is written.
#[test]
fn a_reduced_pdfs_stored_name_is_checked_for_a_collision() {
    let recorded = r#"{"id":"proj-a","name":"A","chats":[],"instructionSkills":{},"projectFolder":"root/proj-a","files":[{"name":"root/proj-a/Text/big.extracted.txt","content":"old","source":"root/proj-a","attachment":{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","bytes":3,"group":"Text","state":"ready"}}]}"#;
    let rig = Rig::new(recorded);
    rig.storage
        .put("root/proj-a/Text/big.extracted.txt", b"old");
    let big: Vec<u8> = b"%PDF-1.4 "
        .iter()
        .copied()
        .chain(std::iter::repeat_n(b' ', 26 * 1024 * 1024))
        .collect();
    let wrap = Wrap::new(&rig.engine);
    *wrap.reduce.borrow_mut() = Some(Ok(wtf16::parse(
        r#"{"kind":"text","text":"only the text"}"#,
    )
    .unwrap()));
    let (status, body) = rig.post_with(&wrap, true, &organized("big.pdf", &big, ""), &|_| {});
    assert_eq!(
        (status, code(&body).as_deref()),
        (409, Some("sourceExists")),
        "{body:?}"
    );
    assert_eq!(
        rig.storage
            .files
            .borrow()
            .get("root/proj-a/Text/big.extracted.txt")
            .unwrap()
            .0,
        b"old"
    );
    assert!(
        !rig.storage.log().iter().any(|l| l.starts_with("PUT")),
        "nothing was written"
    );
}

// ── #589: the project's folder is made on the first upload, once ─────────────────────────────────────

#[test]
fn a_project_folder_is_allocated_by_the_first_upload_and_only_once() {
    let rig = Rig::new(NO_FOLDER);
    assert!(
        rig.storage.dirs.borrow().is_empty(),
        "nothing was made with the project"
    );
    let (status, body) = rig.post(true, &organized("one.md", b"1", ""));
    assert_eq!(status, 200, "{body:?}");
    let made: Vec<String> = rig
        .storage
        .dirs
        .borrow()
        .iter()
        .filter(|d| d.starts_with("root/") && d.matches('/').count() == 1)
        .cloned()
        .collect();
    assert_eq!(made.len(), 1, "{made:?}");
    let (status, _) = rig.post(true, &organized("two.md", b"2", ""));
    assert_eq!(status, 200);
    let after: Vec<String> = rig
        .storage
        .dirs
        .borrow()
        .iter()
        .filter(|d| d.starts_with("root/") && d.matches('/').count() == 1)
        .cloned()
        .collect();
    assert_eq!(
        after, made,
        "the second upload used the first one's folder, not 'Name (2)'"
    );
    let list = store::read(&rig.dir()).unwrap();
    assert_eq!(
        store::find(&list, "proj-a")
            .unwrap()
            .get("projectFolder")
            .as_str(),
        Some(made[0].as_str())
    );
}

// ── #1805: a folder import ───────────────────────────────────────────────────────────────────────────

#[test]
fn without_storage_a_folder_import_keeps_the_relative_paths_as_the_source_names() {
    let rig = Rig::new(NO_FOLDER);
    let _tickets = crate::folder_import::serial_tickets();
    let (status, planned) = plan_entries(
        &rig,
        r#"[{"path":"a/readme.md","size":1},{"path":"b/readme.md","size":1}]"#,
        false,
    );
    assert_eq!(status, 200, "{planned:?}");
    let id = id_of(&planned);
    for (rel, content) in [("a/readme.md", "A"), ("b/readme.md", "B")] {
        let (status, body) = import_one(&rig, false, &id, rel, content.as_bytes());
        assert_eq!(status, 200, "{body:?}");
    }
    let mut got: Vec<(String, String)> = rig
        .files()
        .iter()
        .map(|f| {
            (
                f.get("name").as_str().unwrap().to_owned(),
                crate::sources::text(&rig.dir(), "proj-a", f).unwrap(),
            )
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            ("a/readme.md".into(), "A".into()),
            ("b/readme.md".into(), "B".into())
        ]
    );
}

/// A path that differs only in case from an existing source is refused (409 caseCollision), even with a
/// plan made while nothing clashed; the plan itself refuses it now.
#[test]
fn an_import_path_differing_only_in_case_from_an_existing_source_is_refused() {
    let _tickets = crate::folder_import::serial_tickets();
    let rig = Rig::new(PROJECT);
    let (_, stale) = plan_entries(&rig, r#"[{"path":"docs/README.md","size":3}]"#, true);
    let (_, first) = plan_entries(&rig, r#"[{"path":"docs/readme.md","size":3}]"#, true);
    assert_eq!(
        import_one(&rig, true, &id_of(&first), "docs/readme.md", b"one").0,
        200
    );
    let (_, again) = plan_entries(&rig, r#"[{"path":"docs/README.md","size":3}]"#, true);
    let codes: Vec<_> = match again.get("skipped") {
        JValue::Arr(a) => a
            .iter()
            .map(|s| s.get("code").as_str().unwrap().to_owned())
            .collect(),
        other => panic!("{other:?}"),
    };
    assert_eq!(codes, vec!["caseCollision"], "the plan refuses it now");
    let (status, body) = import_one(&rig, true, &id_of(&stale), "docs/README.md", b"two");
    assert_eq!(
        (status, code(&body).as_deref()),
        (409, Some("caseCollision")),
        "{body:?}"
    );
    let stored: Vec<String> = rig.storage.files.borrow().keys().cloned().collect();
    assert_eq!(stored, vec!["root/proj-a/Text/docs/readme.md"]);
}

/// An upload racing a stale plan with the bytes already stored is answered as unchanged, without a write.
#[test]
fn an_upload_with_the_stored_bytes_under_a_stale_plan_is_answered_unchanged_without_a_write() {
    let _tickets = crate::folder_import::serial_tickets();
    let rig = Rig::new(PROJECT);
    let (_, first) = plan_entries(&rig, r#"[{"path":"docs/readme.md","size":7}]"#, true);
    assert_eq!(
        import_one(&rig, true, &id_of(&first), "docs/readme.md", b"v1 docs").0,
        200
    );
    let puts = || {
        rig.storage
            .log()
            .iter()
            .filter(|l| l.starts_with("PUT"))
            .count()
    };
    let before = puts();
    // Planned while the file differed (the manifest hash is of other bytes).
    let other = uploads::sha_hex(b"v1 docs, other");
    let (_, stale) = plan_entries(
        &rig,
        &format!(r#"[{{"path":"docs/readme.md","size":7,"sha256":"{other}"}}]"#),
        true,
    );
    let (status, body) = import_one(&rig, true, &id_of(&stale), "docs/readme.md", b"v1 docs");
    assert_eq!(status, 200, "{body:?}");
    assert_eq!(body.get("unchanged").as_bool(), Some(true), "{body:?}");
    assert_eq!(puts(), before, "no write");
}

/// Folders are only imported into a project a person made, never into the Diary's or a chat's context.
#[test]
fn a_folder_plan_is_refused_for_the_internal_context_projects() {
    let _tickets = crate::folder_import::serial_tickets();
    let rig = Rig::new(
        r#"{"id":"cowork-diary-extras","name":"Extras","chats":[],"instructionSkills":{}}"#,
    );
    let r = plan(
        rig.ctx(),
        "cowork-diary-extras",
        br#"{"entries":[{"path":"a.md","size":1}]}"#,
        false,
        false,
    );
    assert_eq!(r.status, 400);
    let Body::Json(t) = &r.body else { panic!() };
    assert!(t.contains("project only"), "{t}");
}

/// A refresh of the project's own folder keeps a folder import's nested files.
#[test]
fn a_refresh_of_the_projects_own_folder_keeps_a_folder_imports_nested_files() {
    let _tickets = crate::folder_import::serial_tickets();
    let rig = Rig::new(PROJECT);
    let (_, planned) = plan_entries(
        &rig,
        r#"[{"path":"docs/api/auth.md","size":4},{"path":"notes/readme.md","size":5},{"path":"readme.md","size":3}]"#,
        true,
    );
    let id = id_of(&planned);
    for (rel, c) in [
        ("docs/api/auth.md", "auth"),
        ("notes/readme.md", "notes"),
        ("readme.md", "top"),
    ] {
        assert_eq!(
            import_one(&rig, true, &id, rel, c.as_bytes()).0,
            200,
            "{rel}"
        );
    }
    let mut before: Vec<String> = rig
        .files()
        .iter()
        .map(|f| f.get("name").as_str().unwrap().to_owned())
        .collect();
    before.sort();
    assert_eq!(before.len(), 3);
    let progress = |_: &str| {};
    let storage = Storage::Remote(&rig.storage);
    let reply = crate::sync::run(&crate::sync::SyncEnv {
        ctx: rig.ctx(),
        project: "proj-a",
        storage: &storage,
        engine: &rig.engine,
        limits: &LIMITS,
        catalogue: None,
        project_root: "root",
        progress: &progress,
        lock_wait: std::time::Duration::from_secs(5),
    });
    assert_eq!(reply.status, 200);
    let mut after: Vec<String> = rig
        .files()
        .iter()
        .map(|f| f.get("name").as_str().unwrap().to_owned())
        .collect();
    after.sort();
    assert_eq!(after, before);
    let auth = rig
        .files()
        .into_iter()
        .find(|f| {
            f.get("name")
                .as_str()
                .unwrap()
                .ends_with("docs/api/auth.md")
        })
        .unwrap();
    assert_eq!(
        crate::sources::text(&rig.dir(), "proj-a", &auth).unwrap(),
        "auth"
    );
}

// ── part 1 L5: a bounded wait for the project's source lock ──────────────────────────────────────────


/// Holds the project's source lock on another thread for `ms`, and returns once it is held.
fn hold_lock_for(user: &'static str, project: &'static str, ms: u64) -> std::thread::JoinHandle<()> {
    let (held, ready) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let lock = project_documents::source_lock(user, project);
        let _held = lock.lock().unwrap();
        held.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(ms));
    });
    ready.recv().unwrap();
    handle
}

fn updated_at(rig: &Rig) -> i64 {
    let list = store::read(&rig.dir()).unwrap();
    store::find(&list, "proj-a")
        .unwrap()
        .get("updatedAt")
        .as_num()
        .unwrap() as i64
}

/// Re-review N5: an upload that waited for the project's lock dates the project by when it ran (both
/// forms). (That a call that did not wait keeps the context's time is `lock_restamped`'s own test: the project's
/// lock is process-wide, so a test here cannot know that no other test holds it.)
#[test]
fn an_upload_that_waited_for_the_project_lock_dates_its_save_by_when_it_ran() {
    // (the older form without storage saves the project; with storage it is the plain create-only write)
    for (n, (remote, body)) in [
        (true, organized("a.md", b"x", "")),
        (false, legacy_body("b.md", b"x", "")),
    ]
    .into_iter()
    .enumerate()
    {
        let rig = Rig::new(PROJECT);
        let before = crate::wall_ms();
        let holder = hold_lock_for(USER, "proj-a", 150);
        let (status, _) = rig.post(remote, &body);
        assert_eq!(status, 200, "form {n}");
        holder.join().unwrap();
        assert!(updated_at(&rig) >= before, "form {n}: {}", updated_at(&rig));

    }
}

#[test]
fn an_upload_waits_a_bounded_time_for_the_project_lock_and_then_says_429() {
    let rig = Rig::new(PROJECT);
    let lock = project_documents::source_lock(USER, "proj-a");
    let held = lock.lock().unwrap();
    let progress = |_: &str| {};
    let storage = Storage::Remote(&rig.storage);
    let env = Env {
        ctx: rig.ctx(),
        storage: &storage,
        engine: &rig.engine,
        limits: &LIMITS,
        catalogue: None,
        project_root: "root",
        progress: &progress,
        lock_wait: std::time::Duration::from_millis(100),
    };
    for body in [organized("a.md", b"x", ""), legacy_body("b.md", b"x", "")] {
        let started = std::time::Instant::now();
        let r = post(&env, "proj-a", body.as_bytes(), false);
        assert_eq!(r.status, 429, "{r:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
    assert!(rig.storage.log().is_empty(), "nothing was written");
    drop(held);
    let (status, _) = rig.post(true, &organized("a.md", b"x", ""));
    assert_eq!(status, 200);
}
