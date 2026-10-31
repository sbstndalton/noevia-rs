// Included into `sync::tests` (noevia#2191 round 8 review): the findings of the review of the native sync
// and the Node tests the move deleted, ported as the Rust behaviour. The rig, the fakes and the constants
// of the module above are in scope.

fn wall() -> i64 {
    crate::wall_ms()
}

fn png(extra: usize) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend(std::iter::repeat_n(0u8, extra));
    v
}

fn project_json(rig: &Rig) -> JValue {
    let list = store::read(&rig.dir()).unwrap();
    store::find(&list, "proj-a").unwrap().clone()
}

fn set_project(rig: &Rig, change: impl FnOnce(&mut JValue)) {
    store::update(
        &rig.dir(),
        server_store::RustProjects::owned(),
        |projects| {
            let p = store::find_mut(projects, "proj-a").unwrap();
            change(p);
            ((), true)
        },
    )
    .unwrap();
}

fn unindexed(rig: &Rig) -> Vec<String> {
    outbox::jobs(&rig.dir())
        .unwrap()
        .iter()
        .filter(|j| j.kind == "unindex")
        .filter_map(|j| j.file.clone())
        .collect()
}

fn login_refused() -> StorageError {
    StorageError::new(502, "storage returned 401").with_upstream(401)
}

fn at_http(status: u16) -> StorageError {
    StorageError::new(502, format!("storage returned {status}")).with_upstream(status)
}

// ── the review's findings ────────────────────────────────────────────────────────────────────────

/// Part 1 M2: a hold is dated by the moment of the refusal. A sync whose request time is long past (a
/// job that waited, a sync of hours) used to write a hold that was already over, so the next automatic
/// call went straight through as a probe (#1429).
#[test]
fn a_refusal_in_a_sync_with_an_old_request_time_still_holds_the_next_automatic_call() {
    let rig = Rig::new(PROJECT);
    assert_eq!(
        rig.ctx().now_ms,
        1_700_000_000_000,
        "the request time is far in the past"
    );
    rig.storage
        .list_errors
        .borrow_mut()
        .insert("docs".into(), login_refused());
    let (reply, _) = rig.sync();
    assert_eq!(reply.status, 200);
    let hold =
        storage_latch::blocked(rig.root.path(), USER, &rig.storage.fingerprint(), wall()).unwrap();
    assert_eq!(hold.map(|h| h.kind), Some("login"));
}

/// Part 1 L6: a latch that cannot be read holds an automatic call for a minute; an explicit press runs.
#[test]
fn a_latch_that_cannot_be_read_counts_as_held() {
    let rig = Rig::new(PROJECT);
    // The latch file is a directory: opening it as a database fails.
    std::fs::create_dir_all(storage_latch::path(rig.root.path())).unwrap();
    let held = automatic_hold(rig.ctx(), &Storage::Remote(&rig.storage), false)
        .expect("an unreadable latch holds");
    assert_eq!(held.status, 503, "{held:?}");
    assert!(
        matches!(&held.body, Body::Bytes { headers, .. } if headers.iter().any(|(k, v)| *k == "Retry-After" && v == "60")),
        "a throttle's answer, for a minute: {held:?}"
    );
    assert!(
        automatic_hold(rig.ctx(), &Storage::Remote(&rig.storage), true).is_none(),
        "an explicit press still runs"
    );
}

/// Part 2 L7: a subfolder refused the login latches it; the listing that follows did not prove the login.
#[test]
fn a_subfolder_refused_the_login_leaves_the_latch_set_and_each_reason_is_its_own() {
    let rig = Rig::new(PROJECT);
    rig.storage.put("docs/top.md", b"top");
    rig.storage.put("docs/a/one.md", b"one");
    rig.storage.put("docs/b/two.md", b"two");
    rig.storage
        .list_errors
        .borrow_mut()
        .insert("docs/a".into(), at_http(500));
    rig.storage
        .list_errors
        .borrow_mut()
        .insert("docs/b".into(), login_refused());
    let (reply, body) = rig.sync();
    assert_eq!(reply.status, 200);
    let skips = skipped(&body);
    let of = |dir: &str| {
        skips
            .iter()
            .find(|s| s.get("file").as_str() == Some(dir))
            .unwrap()
            .clone()
    };
    assert_eq!(
        of("docs/a").get("reason").as_str(),
        Some("Could not read this subfolder; previous sources retained."),
        "the first subfolder failed before the login was refused: its reason is its own"
    );
    assert_eq!(
        of("docs/b").get("code").as_str(),
        Some("storageLoginRejected")
    );
    let hold =
        storage_latch::blocked(rig.root.path(), USER, &rig.storage.fingerprint(), wall()).unwrap();
    assert_eq!(
        hold.map(|h| h.kind),
        Some("login"),
        "the listing did not clear the refusal"
    );
}

/// Part 2 M2: a document the sync's OCR allowance left open gets no queued job; the next sync goes on.
#[test]
fn a_document_deferred_by_the_ocr_budget_stays_open_without_a_job_and_the_next_sync_continues_it() {
    let mut rig = Rig::new(PROJECT);
    rig.engine.ocr = true;
    rig.storage.put("docs/scan.pdf", b"%PDF-1.4 synthetic");
    rig.engine.replies.borrow_mut().push(Ok(wtf16::parse(
        r#"{"total":3,"pages":[{"number":1,"text":"","status":"ocr-needed","method":"docling"},{"number":2,"text":"","status":"ocr-needed","method":"docling"},{"number":3,"text":"","status":"ocr-needed","method":"docling"}]}"#,
    ).unwrap()));
    rig.engine.ocr_replies.borrow_mut().push(Ok(vec![
        wtf16::parse(r#"{"number":1,"text":"page one"}"#).unwrap(),
        wtf16::parse(r#"{"number":2,"text":"page two"}"#).unwrap(),
    ]));
    let budgeted_sync = |rig: &Rig, pages: usize| {
        let progress = |_: &str| {};
        let storage = Storage::Remote(&rig.storage);
        run_budgeted(
            &SyncEnv {
                ctx: rig.ctx(),
                project: "proj-a",
                storage: &storage,
                engine: &rig.engine,
                limits: &LIMITS,
                catalogue: None,
                project_root: "root",
                progress: &progress,
                lock_wait: Duration::from_secs(5),
            },
            pages,
        )
    };
    // Budget 2 of 3 pages.
    let reply = budgeted_sync(&rig, 2);
    assert_eq!(reply.status, 200);
    let doc = rig.files()[0].get("document").clone();
    assert_eq!(
        doc.get("state").as_str(),
        Some("partial"),
        "the pages read are kept"
    );
    assert!(
        doc.get("extraction").is_nullish(),
        "not marked queued: {doc:?}"
    );
    assert!(
        extract_queue::pending(&rig.dir(), "proj-a", "docs/scan.pdf").is_none(),
        "an unbudgeted queue would run every page and hold the OCR slot for hours"
    );
    // The next sync continues it within its own allowance: the third page, no new Docling read.
    rig.engine
        .ocr_replies
        .borrow_mut()
        .push(Ok(vec![wtf16::parse(
            r#"{"number":3,"text":"page three"}"#,
        )
        .unwrap()]));
    let reply = budgeted_sync(&rig, 2);
    assert_eq!(reply.status, 200);
    assert!(
        rig.engine.ocr_replies.borrow().is_empty(),
        "the third page was read"
    );
    assert_eq!(
        rig.engine.asked.borrow().len(),
        1,
        "the document was extracted once"
    );
    let text = crate::sources::text(&rig.dir(), "proj-a", &rig.files()[0]).unwrap();
    assert!(text.contains("page three"), "{text}");
}

/// Part 2 L1: a managed file too big to be a vision image takes the image its name had out of the list
/// (retired; the prune drops what no chat names).
#[test]
fn a_managed_image_replaced_by_one_too_big_for_vision_retires_the_old_asset() {
    let project = r#"{"id":"proj-a","name":"A","chats":[],"instructionSkills":{},"projectFolder":"root/proj-a","sourceFolders":["root/proj-a"],"assets":[{"id":"img-old","name":"pic.png","mime":"image/png","bytes":10,"sourceName":"root/proj-a/Images/pic.png","storagePath":"root/proj-a/Images/pic.png"}]}"#;
    let mut rig = Rig::new(project);
    // Without Docling an image is a managed image (with Docling it would be read as a document).
    rig.engine.docling = false;
    rig.storage
        .put("root/proj-a/Images/pic.png", &png(9 * 1024 * 1024));
    let (reply, body) = rig.sync();
    assert_eq!(reply.status, 200, "{body:?}");
    let p = project_json(&rig);
    let assets = match p.get("assets") {
        JValue::Arr(a) => a.clone(),
        other => panic!("{other:?}"),
    };
    assert!(
        assets
            .iter()
            .all(|a| a.get("id").as_str() != Some("img-old")),
        "the stale vision image is gone: {assets:?}"
    );
}

/// Part 2 L3: the folder allocated for the loose images is the project's even when every move fails.
#[test]
fn the_folder_allocated_for_loose_images_is_saved_even_when_every_move_fails() {
    let rig = Rig::new(
        r#"{"id":"proj-a","name":"Alpha","chats":[],"instructionSkills":{},"assets":[{"id":"img-1","name":"a.png","mime":"image/png","bytes":12}]}"#,
    );
    let assets = store::asset_dir(&rig.dir(), "proj-a");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join("img-1"), png(4)).unwrap();
    // Storage takes the folder but refuses every file.
    rig.storage.write_errors.borrow_mut().insert(
        "root/Alpha/Images/a-img-1.png".into(),
        StorageError::new(502, "could not write (500)"),
    );
    for name in ["root/Alpha", "root/alpha", "root/proj-a"] {
        rig.storage.write_errors.borrow_mut().insert(
            format!("{name}/Images/a-img-1.png"),
            StorageError::new(502, "could not write (500)"),
        );
    }
    let (reply, body) = rig.sync();
    assert_eq!(reply.status, 200, "{body:?}");
    let first = project_json(&rig)
        .get("projectFolder")
        .as_str()
        .map(str::to_owned);
    assert!(
        first.as_deref().is_some_and(|f| !f.is_empty()),
        "saved: {:?}",
        project_json(&rig)
    );
    let made: Vec<String> = rig.storage.dirs.borrow().iter().cloned().collect();
    // A second sync allocates nothing new: no "Name (2)" next to an orphan.
    let (reply, _) = rig.sync();
    assert_eq!(reply.status, 200);
    assert_eq!(
        project_json(&rig)
            .get("projectFolder")
            .as_str()
            .map(str::to_owned),
        first
    );
    let after: Vec<String> = rig.storage.dirs.borrow().iter().cloned().collect();
    assert_eq!(made, after, "the same folder, not a new one");
}

/// Part 1 L5: a sync that cannot get the project's source lock says 429 instead of waiting without end.
#[test]
fn a_sync_waits_a_bounded_time_for_the_project_lock_and_then_says_429() {
    let rig = Rig::new(PROJECT);
    rig.storage.put("docs/a.md", b"a");
    let lock = project_documents::source_lock(USER, "proj-a");
    let held = lock.lock().unwrap();
    let progress = |_: &str| {};
    let started = Instant::now();
    let reply = run(&SyncEnv {
        ctx: rig.ctx(),
        project: "proj-a",
        storage: &Storage::Remote(&rig.storage),
        engine: &rig.engine,
        limits: &LIMITS,
        catalogue: None,
        project_root: "root",
        progress: &progress,
        lock_wait: Duration::from_millis(100),
    });
    assert_eq!(reply.status, 429);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(rig.storage.log().is_empty(), "nothing was read");
    drop(held);
    let (reply, _) = rig.sync();
    assert_eq!(reply.status, 200);
}


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

fn index_job_times(rig: &Rig) -> Vec<i64> {
    let conn = rusqlite::Connection::open(outbox::path(&rig.dir())).unwrap();
    let mut stmt = conn
        .prepare("SELECT created_at FROM jobs WHERE kind = 'index'")
        .unwrap();
    let times: Vec<i64> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    times
}

/// Re-review N5: a sync that waited for the project's lock (a job queued behind another operation) dates
/// its records by when it runs, not by when its context was made. (That a call that did not wait keeps the
/// context's time is `lock_restamped`'s own test; the project's lock is process-wide.)
#[test]
fn a_sync_that_waited_for_the_project_lock_dates_its_records_by_when_it_ran() {
    let rig = Rig::new(PROJECT);
    rig.storage.put("docs/a.md", b"a");
    let before = wall();
    let holder = hold_lock_for(USER, "proj-a", 150);
    let (reply, _) = rig.sync();
    assert_eq!(reply.status, 200);
    holder.join().unwrap();
    let times = index_job_times(&rig);
    assert!(!times.is_empty());
    assert!(
        times.iter().all(|t| *t >= before),
        "stamped when it ran, not at the context's time: {times:?} < {before}"
    );

}

/// Part 1 M3 / part 2 M1: an account removed while a sync runs stays removed (#1425): the staging folder
/// of the next file is not made below a user folder that is gone, and the sync ends 410.
#[test]
fn an_account_removed_mid_sync_is_not_recreated_and_the_sync_ends_410() {
    let rig = Rig::new(PROJECT);
    rig.storage.put("docs/a.md", b"a");
    rig.storage.put("docs/b.pdf", b"%PDF-1.4 synthetic");
    rig.storage.put("docs/c.md", b"c");
    let dir = rig.dir();
    *rig.storage.on_read.borrow_mut() = Some(Box::new(move |path| {
        if path == "docs/a.md" {
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }));
    let (reply, _) = rig.sync();
    assert_eq!(reply.status, 410, "{reply:?}");
    assert!(!rig.dir().exists(), "users/<id> was not made again");
    let log = rig.storage.log();
    assert!(
        !log.contains(&"GET docs/b.pdf".to_owned()),
        "the staging folder of the next file is not made below a folder that is gone"
    );
    assert!(
        !log.contains(&"READ docs/c.md".to_owned()),
        "the sync stops at the first file of a removed account: nothing more is read for it: {log:?}"
    );
}

/// The same, with the account removed while a document is being extracted: neither the original nor the
/// result record makes the folder again, and the sync ends 410 instead of reporting a file error.
#[test]
fn an_account_removed_while_a_document_is_extracted_is_not_recreated() {
    let rig = Rig::new(PROJECT);
    rig.storage.put("docs/r.pdf", b"%PDF-1.4 synthetic");
    rig.engine
        .replies
        .borrow_mut()
        .push(Ok(wtf16::parse(GOOD).unwrap()));
    let dir = rig.dir();
    *rig.engine.on_docling.borrow_mut() = Some(Box::new(move || {
        std::fs::remove_dir_all(&dir).unwrap();
    }));
    let (reply, _) = rig.sync();
    assert_eq!(reply.status, 410, "{reply:?}");
    assert!(
        !rig.dir().exists(),
        "neither the result record nor projects.json came back"
    );
}

// ── #1313: a listing that failed is not an empty folder ───────────────────────────────────────────

/// DAV listing failures keep the attached sources and the cached originals; only a valid empty listing is
/// authoritative, and a detach removes only the selected folder's sources (core's storage-client
/// "DAV HTTP listing failures retain attached project sources and cached originals (#1313)").
#[test]
fn listing_failures_retain_sources_originals_and_index_entries_and_an_empty_listing_does_not() {
    let project = r#"{"id":"proj-a","name":"A","chats":[],"instructionSkills":{},"projectFolder":"root/proj-a","sourceFolders":["root/proj-a","linked","other"],"files":[{"name":"local.txt","content":"unrelated local upload"}]}"#;
    let rig = Rig::new(project);
    rig.storage
        .put("root/proj-a/Text/remote.txt", b"original remote bytes");
    rig.storage.put("linked/notes.md", b"linked knowledge");
    rig.storage.put("other/other.md", b"other knowledge");
    let (reply, _) = rig.sync();
    assert_eq!(reply.status, 200);
    let sorted = |rig: &Rig| {
        let mut files = rig.files();
        files.sort_by(|a, b| a.get("name").as_str().cmp(&b.get("name").as_str()));
        files
    };
    let before = sorted(&rig);
    assert_eq!(before.len(), 4);
    let cache_dir = crate::lifecycle::uploads_dir(&rig.dir(), "proj-a");
    let cache = || -> Vec<(String, Vec<u8>)> {
        let mut out: Vec<_> = std::fs::read_dir(&cache_dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    std::fs::read(e.path()).unwrap(),
                )
            })
            .collect();
        out.sort();
        out
    };
    let cached = cache();
    assert!(!cached.is_empty(), "the managed file's original is cached");
    let texts = |files: &[JValue]| -> Vec<(String, String)> {
        files
            .iter()
            .map(|f| {
                (
                    f.get("name").as_str().unwrap().to_owned(),
                    crate::sources::text(&rig.dir(), "proj-a", f).unwrap_or_default(),
                )
            })
            .collect()
    };
    for (folder, status) in [
        ("root/proj-a", 404),
        ("linked", 404),
        ("linked", 401),
        ("linked", 403),
        ("linked", 503),
        ("root/proj-a/Text", 404),
    ] {
        rig.storage
            .list_errors
            .borrow_mut()
            .insert(folder.into(), at_http(status));
        let (reply, body) = rig.sync();
        assert_eq!(reply.status, 200, "HTTP {status} at {folder}");
        assert_eq!(
            sorted(&rig),
            before,
            "HTTP {status} at {folder} must retain the exact sources"
        );
        assert_eq!(texts(&sorted(&rig)), texts(&before));
        assert!(
            skipped(&body).iter().any(|s| {
                (s.get("folder").as_str() == Some(folder) || s.get("file").as_str() == Some(folder))
                    && s.get("retained").as_bool() == Some(true)
            }),
            "HTTP {status} at {folder}: {:?}",
            skipped(&body)
        );
        assert!(
            unindexed(&rig).is_empty(),
            "a failed listing unindexes nothing"
        );
        assert_eq!(cache(), cached, "a failed listing prunes no original");
        rig.storage.list_errors.borrow_mut().clear();
    }
    // A subfolder that returns 500 keeps its own sources, too.
    rig.storage.put("linked/sub/deep.md", b"deep");
    let (_, _) = rig.sync();
    let with_sub = sorted(&rig);
    rig.storage
        .list_errors
        .borrow_mut()
        .insert("linked/sub".into(), at_http(500));
    let (_, body) = rig.sync();
    assert_eq!(sorted(&rig), with_sub);
    assert!(skipped(&body)
        .iter()
        .any(|s| s.get("file").as_str() == Some("linked/sub")
            && s.get("retained").as_bool() == Some(true)));
    assert!(unindexed(&rig).is_empty());
    rig.storage.list_errors.borrow_mut().clear();
    // A valid empty listing is authoritative: the folder stays attached, its sources go and are unindexed.
    rig.storage
        .files
        .borrow_mut()
        .retain(|p, _| !p.starts_with("linked/"));
    rig.storage
        .dirs
        .borrow_mut()
        .retain(|d| !d.starts_with("linked/"));
    rig.storage.dirs.borrow_mut().insert("linked".into());
    let (_, body) = rig.sync();
    assert!(skipped(&body).is_empty(), "{:?}", skipped(&body));
    let p = project_json(&rig);
    assert!(
        strs(p.get("sourceFolders")).contains(&"linked".to_owned()),
        "stays attached"
    );
    assert!(!rig.names().iter().any(|n| n.starts_with("linked/")));
    let mut gone = unindexed(&rig);
    gone.sort();
    assert_eq!(gone, vec!["linked/notes.md", "linked/sub/deep.md"]);
    // Detaching one folder removes only that folder's sources.
    set_project(&rig, |p| {
        store::set(
            p,
            "sourceFolders",
            JValue::Arr(vec![s("root/proj-a"), s("linked")]),
        );
    });
    let (_, _) = rig.sync();
    let names = rig.names();
    assert!(!names.iter().any(|n| n.starts_with("other/")));
    assert!(names.contains(&"local.txt".to_owned()));
    assert!(names.contains(&"root/proj-a/Text/remote.txt".to_owned()));
    assert!(unindexed(&rig).contains(&"other/other.md".to_owned()));
}

// ── ported from the deleted Node tests ───────────────────────────────────────────────────────────

/// #1805: an attached folder is read recursively, skipping system folders and unreadable types, and a
/// nested attached folder is left to its own entry so no file is synced twice.
#[test]
fn an_attached_folder_is_read_recursively_and_a_nested_attached_folder_is_not_synced_twice() {
    let project = r#"{"id":"proj-a","name":"A","chats":[],"instructionSkills":{},"sourceFolders":["Shared/Team","Shared/Team/linked"],"files":[{"name":"upload.md","content":"kept"}]}"#;
    let rig = Rig::new(project);
    for (p, c) in [
        ("Shared/Team/top.md", "top"),
        ("Shared/Team/docs/readme.md", "docs readme"),
        ("Shared/Team/docs/api/auth.md", "auth"),
        ("Shared/Team/notes/readme.md", "notes readme"),
        ("Shared/Team/notes/2026/q4/plan.txt", "plan"),
        ("Shared/Team/.git/config.md", "git"),
        ("Shared/Team/node_modules/pkg/readme.md", "dependency"),
        ("Shared/Team/linked/own.md", "own"),
        ("Shared/Team/tool.exe", "MZ"),
    ] {
        rig.storage.put(p, c.as_bytes());
    }
    let (reply, body) = rig.sync();
    assert_eq!(reply.status, 200, "{body:?}");
    let mut got: Vec<(String, Option<String>)> = rig
        .files()
        .iter()
        .map(|f| {
            (
                f.get("name").as_str().unwrap().to_owned(),
                f.get("source").as_str().map(str::to_owned),
            )
        })
        .collect();
    got.sort();
    let team = Some("Shared/Team".to_owned());
    assert_eq!(
        got,
        vec![
            ("Shared/Team/docs/api/auth.md".to_owned(), team.clone()),
            ("Shared/Team/docs/readme.md".to_owned(), team.clone()),
            (
                "Shared/Team/linked/own.md".to_owned(),
                Some("Shared/Team/linked".to_owned())
            ),
            (
                "Shared/Team/notes/2026/q4/plan.txt".to_owned(),
                team.clone()
            ),
            ("Shared/Team/notes/readme.md".to_owned(), team.clone()),
            ("Shared/Team/top.md".to_owned(), team),
            ("upload.md".to_owned(), None),
        ]
    );
    assert!(skipped(&body).is_empty(), "{:?}", skipped(&body));
}

/// #1805: a subfolder that cannot be listed keeps its previous sources and is reported; the rest refresh.
#[test]
fn a_subfolder_that_cannot_be_listed_keeps_its_previous_sources_and_the_rest_refresh() {
    let project = r#"{"id":"proj-a","name":"A","chats":[],"instructionSkills":{},"sourceFolders":["Shared/Team"],"files":[{"name":"Shared/Team/b/two.md","content":"two v1","source":"Shared/Team"}]}"#;
    let rig = Rig::new(project);
    rig.storage.put("Shared/Team/a/one.md", b"one v2");
    rig.storage.put("Shared/Team/b/two.md", b"two");
    rig.storage
        .list_errors
        .borrow_mut()
        .insert("Shared/Team/b".into(), at_http(500));
    let (reply, body) = rig.sync();
    assert_eq!(reply.status, 200);
    let texts: Vec<(String, String)> = rig
        .files()
        .iter()
        .map(|f| {
            (
                f.get("name").as_str().unwrap().to_owned(),
                crate::sources::text(&rig.dir(), "proj-a", f).unwrap(),
            )
        })
        .collect();
    assert!(texts.contains(&("Shared/Team/a/one.md".into(), "one v2".into())));
    assert!(
        texts.contains(&("Shared/Team/b/two.md".into(), "two v1".into())),
        "{texts:?}"
    );
    assert!(skipped(&body)
        .iter()
        .any(|s| s.get("file").as_str() == Some("Shared/Team/b")
            && s.get("retained").as_bool() == Some(true)));
}

/// documents.test.cjs: a folder detached while its refresh is in flight is not restored when it ends.
#[test]
fn a_detached_folder_is_not_restored_when_its_in_flight_refresh_finishes() {
    let rig = Rig::new(PROJECT);
    rig.storage.put("docs/a.md", b"a");
    rig.storage.put("docs/b.md", b"b");
    let dir = rig.dir();
    *rig.storage.on_read.borrow_mut() = Some(Box::new(move |path| {
        if path == "docs/a.md" {
            store::update(&dir, server_store::RustProjects::owned(), |projects| {
                let p = store::find_mut(projects, "proj-a").unwrap();
                store::set(p, "sourceFolders", JValue::Arr(vec![]));
                ((), true)
            })
            .unwrap();
        }
    }));
    let (reply, _) = rig.sync();
    assert_eq!(reply.status, 200);
    assert_eq!(
        rig.files()
            .iter()
            .filter(|f| f.get("source").truthy())
            .count(),
        0
    );
}

/// documents.test.cjs: a sync counts the project's other sources (uploads, other folders') against the
/// source limit, never dropping them.
#[test]
fn the_sources_a_sync_keeps_count_against_the_limit_of_those_it_reads() {
    let uploads: Vec<String> = (0..998)
        .map(|n| format!(r#"{{"name":"u{n}.md","content":"x"}}"#))
        .collect();
    let project = format!(
        r#"{{"id":"proj-a","name":"A","chats":[],"instructionSkills":{{}},"sourceFolders":["docs"],"files":[{}]}}"#,
        uploads.join(",")
    );
    let rig = Rig::new(&project);
    for n in 0..5 {
        rig.storage.put(&format!("docs/f{n}.md"), b"x");
    }
    let (_, body) = rig.sync();
    assert_eq!(
        rig.names().len(),
        1000,
        "998 uploads kept and two of five read"
    );
    let skips = skipped(&body);
    assert_eq!(skips.len(), 3, "{skips:?}");
    assert_eq!(skips[0].get("code").as_str(), Some("limit"));
}

/// documents.test.cjs: a refresh moves a project's loose local images into the project's storage folder
/// without losing their bytes.
#[test]
fn a_refresh_moves_legacy_local_images_into_managed_storage_with_their_bytes() {
    let project = r#"{"id":"proj-a","name":"A","chats":[],"instructionSkills":{},"projectFolder":"root/proj-a","assets":[{"id":"img-legacy","name":"fixture.png","mime":"image/png","bytes":12}]}"#;
    let rig = Rig::new(project);
    let assets = store::asset_dir(&rig.dir(), "proj-a");
    std::fs::create_dir_all(&assets).unwrap();
    let bytes = png(4);
    std::fs::write(assets.join("img-legacy"), &bytes).unwrap();
    let (reply, body) = rig.sync();
    assert_eq!(reply.status, 200, "{body:?}");
    assert_eq!(
        rig.storage
            .files
            .borrow()
            .get("root/proj-a/Images/fixture-img-legacy.png")
            .map(|f| f.0.clone()),
        Some(bytes)
    );
    let p = project_json(&rig);
    let list = match p.get("assets") {
        JValue::Arr(a) => a.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(list.len(), 1);
    assert_eq!(
        list[0].get("storagePath").as_str(),
        Some("root/proj-a/Images/fixture-img-legacy.png")
    );
    assert_eq!(rig.files().len(), 1);
}

/// documents.test.cjs: a refresh keeps the project's managed Images/Documents/Text files: opaque
/// documents stay `stored`, images keep their asset, and a batch past the retired 40-source cap is kept.
#[test]
fn a_refresh_keeps_managed_images_documents_and_text_and_batches_past_forty() {
    let project = r#"{"id":"proj-a","name":"A","chats":[],"instructionSkills":{},"projectFolder":"root/proj-a","sourceFolders":["root/proj-a"]}"#;
    let mut rig = Rig::new(project);
    rig.engine.docling = false;
    rig.storage.put(
        "root/proj-a/Documents/fixture.docx",
        b"PK\x03\x04 synthetic",
    );
    rig.storage.put("root/proj-a/Images/fixture.png", &png(8));
    rig.storage
        .put("root/proj-a/Text/fixture.txt", b"synthetic bytes");
    for n in 0..41 {
        rig.storage
            .put(&format!("root/proj-a/Text/batch-{n}.txt"), b"synthetic");
    }
    let (reply, body) = rig.sync();
    assert_eq!(reply.status, 200, "{body:?}");
    assert_eq!(rig.files().len(), 44);
    assert!(
        skipped(&body)
            .iter()
            .all(|s| s.get("file").as_str() == Some("root/proj-a/Documents/fixture.docx")),
        "{:?}",
        skipped(&body)
    );
    let docx = rig
        .files()
        .into_iter()
        .find(|f| f.get("name").as_str().unwrap().ends_with(".docx"))
        .unwrap();
    assert_eq!(docx.get("attachment").get("state").as_str(), Some("stored"));
    let assets = match project_json(&rig).get("assets") {
        JValue::Arr(a) => a.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(assets.len(), 1);
}

/// noevia#2245: the queue worker replaced a queued record with only `name`, `content` and `document`.
/// A synced document lost its `source`, so the next sync took it for an upload: it was kept for good (the
/// folder could no longer retire it) and the folder's own copy of the same name came in beside it.
#[test]
fn a_queued_synced_document_keeps_its_source_when_the_worker_reads_it_and_a_later_sync_adds_no_copy() {
    let rig = Rig::new(PROJECT);
    rig.storage.put("docs/r.pdf", b"%PDF-1.4 synthetic");
    rig.sync(); // Docling unreachable: the document is queued, as the sync's own record
    let queued = rig.files();
    assert_eq!(queued[0].get("source").as_str(), Some("docs"));
    assert_eq!(queued[0].get("document").get("state").as_str(), Some("queued"));
    // The worker, long after the backoff, with Docling back.
    rig.engine
        .replies
        .borrow_mut()
        .push(Ok(wtf16::parse(GOOD).unwrap()));
    let later = Ctx {
        now_ms: wall() + 3_600_000, // a sync dates its jobs by the wall clock
        ..rig.ctx()
    };
    let ran = extract_queue::run_due(later, &rig.engine, &LIMITS);
    assert_eq!(ran.len(), 1);
    let read = rig.files();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].get("document").get("state").as_str(), Some("ready"));
    assert_eq!(
        read[0].get("source").as_str(),
        Some("docs"),
        "the record still names the folder it came from: {:?}",
        read[0]
    );
    // The next sync sees the same file in the same folder: still one record, still the folder's.
    rig.engine
        .replies
        .borrow_mut()
        .push(Ok(wtf16::parse(GOOD).unwrap()));
    rig.sync();
    assert_eq!(rig.names(), vec!["docs/r.pdf".to_string()]);
    assert_eq!(rig.files()[0].get("source").as_str(), Some("docs"));
    // And when the file leaves the folder the sync retires the record (an upload would have stayed).
    rig.storage.files.borrow_mut().remove("docs/r.pdf");
    rig.sync();
    assert!(
        rig.names().is_empty(),
        "a folder file that is gone is retired, not kept as an upload: {:?}",
        rig.names()
    );
}
