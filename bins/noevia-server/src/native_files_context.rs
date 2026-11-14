//! A project turn's knowledge-file context, answered natively (council p9, retrieval area): core
//! `rag.cjs` `filesContext(projectId, files, query, userId, onSources, options)` for the request
//! `chat_prepare::retrieval::request` builds, so the native chat preparation (track R8,
//! chat_prepare_shadow.rs) can cover a project turn that has knowledge files instead of skipping it.
//! Nothing calls this yet: the preparation still skips those turns, and Node's own `filesContext`
//! asks the front only for its search (`/v1/retrieval/search`, native_retrieval.rs).
//!
//! In `filesContext`'s order:
//! 1. no files: `null`, and `onSources` is not called;
//! 2. the files split by length (`lengthOf`: a source whose text is in its own file counts its
//!    saved `chars`), the manifest names every file (a nullish file throws there, as Node's
//!    `f.name` does), and each file's notice is `document-sources.cjs` `notice(f)`, else its
//!    attachment's reason (`document_read::notice`; a throw there is the turn's throw);
//! 3. when sqlite-vec registers and a file is over `DIRECT_INJECT_MAX`: `searchProject` for the
//!    question, natively ([`native_retrieval`](crate::native_retrieval)), past the guards
//!    `searchProjectNow` applies for an account (a removed account, or a project the front
//!    tombstoned in the account's index outbox: no hits);
//! 4. `project_retrieval::context::files_context` (the composition, recorded from core by track
//!    R4), and what it placed and injected as `onSources(placed, injected)` reports it, as JSON
//!    with Node's key order (`{file, body, score, kind}` for an excerpt, `{file, body, kind}` for a
//!    file).
//!
//! The answer is [`Native::Answer`], which `chat_prepare::retrieval::retrieved` takes as it takes
//! Node's, or [`Native::Defer`] with a fixed label (no data) when Node must answer:
//! - `off`: `NOEVIA_RUST_RETRIEVAL` is off (one variable for both processes; the search would embed
//!   the question a second time beside Node's otherwise), or there is no data directory;
//! - `images`: multimodal retrieval was asked (`options.images`; chat-prepare keeps that turn in
//!   Node too, `Stopped::Images`);
//! - `project id`, `file name`, `file content`, `attachment reason`: a value of a kind no stored
//!   record holds (Node would coerce it with `String()`; not guessed here);
//! - `source length`: a source whose saved `chars` puts it on the other side of
//!   `DIRECT_INJECT_MAX` than its text does (Node sorts it by `chars`, the composition by the text);
//! - `shared index`: the request has no account (Node searches the shared `RAG_DIR` itself);
//! - whatever the native search defers for (a shared-engine target, an unreadable configuration or
//!   reranker settings, `deadline`).
//!
//! Named differences from Node:
//! - Node's in-process wanted versions (`fileVersions`, a re-index Node runs itself) are not this
//!   process's: the search filters by none, as a research job's does;
//! - an index outbox that cannot be read counts as the project deleted, so the search answers no
//!   hits (Node's guard reads it as not deleted, noevia#2728); the turn still gets the files' heads;
//! - those of `project_retrieval::context` (a head cut inside a surrogate pair) and of the native
//!   search (named in native_retrieval.rs).
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use chat_prepare::retrieval::{Answer, Request, Source as ReadableFile};
use js_json::JValue;
use project_retrieval::context::{self, FileIn, Input, Source, DIRECT_INJECT_MAX};
use project_retrieval::hits::Hit;
use project_retrieval::search::Versions;

use crate::native_retrieval::{Inputs, Mode, Outcome, STORE_DEADLINE};

/// The native answer, or why Node answers.
#[derive(Debug, Clone, PartialEq)]
pub enum Native {
    Answer(Answer),
    Defer(&'static str),
}

/// The files of a request as `filesContext` reads them.
#[derive(Debug, Clone, PartialEq)]
pub struct Files {
    pub files: Vec<FileIn>,
    /// A file over `DIRECT_INJECT_MAX` by `lengthOf`: only then does `filesContext` search.
    pub large: bool,
}

/// Why [`files`] gives no files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unread {
    /// Node throws reading them: the turn's retrieval threw.
    Threw,
    Defer(&'static str),
}

/// `Number.isSafeInteger(x)`.
fn safe_integer(x: f64) -> bool {
    x.is_finite() && x.fract() == 0.0 && x.abs() <= 9_007_199_254_740_991.0
}

/// `/^[0-9a-f]{64}$/.test(String(f.textRef))` for the values a record holds.
fn text_ref(v: &JValue) -> bool {
    v.as_str()
        .is_some_and(|r| r.len() == 64 && r.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// `lengthOf(f)`: the saved `chars` of a source whose text is in its own file, else the text's
/// length in UTF-16 code units.
fn length_of(source: &ReadableFile, content: &str) -> f64 {
    let f = &source.file;
    if source.external && text_ref(f.get("textRef")) {
        if let Some(chars) = f.get("chars").as_num().filter(|c| safe_integer(*c)) {
            return chars;
        }
    }
    js_json::js_len(content) as f64
}

/// One file: its name, text, notice and attachment reason, and whether it is large.
fn file_in(source: &ReadableFile) -> Result<(FileIn, bool), Unread> {
    let f = &source.file;
    // `files.map((f) => f.name)` throws on a nullish file.
    if f.is_nullish() {
        return Err(Unread::Threw);
    }
    let name = f
        .get("name")
        .as_str()
        .ok_or(Unread::Defer("file name"))?
        .to_owned();
    let content = match f.get("content") {
        JValue::Str(s) => s.clone(),
        _ => return Err(Unread::Defer("file content")),
    };
    let length = length_of(source, &content);
    let large = length > DIRECT_INJECT_MAX as f64;
    // The composition sorts by the text itself; a saved length on the other side is not guessed.
    if large != (js_json::js_len(&content) > DIRECT_INJECT_MAX) {
        return Err(Unread::Defer("source length"));
    }
    let reason = f.get("attachment").get("reason");
    let attachment_reason = if !reason.truthy() {
        None
    } else {
        Some(
            reason
                .as_str()
                .ok_or(Unread::Defer("attachment reason"))?
                .to_owned(),
        )
    };
    Ok((
        FileIn {
            name,
            content,
            notice: None,
            attachment_reason,
        },
        large,
    ))
}

/// The request's files in order, with `documentNotice(f)` each. Node reads every name (the
/// manifest) before any notice, so a nullish file throws before a notice is computed.
pub fn files(sources: &[ReadableFile]) -> Result<Files, Unread> {
    let mut out = Vec::with_capacity(sources.len());
    let mut large = false;
    for source in sources {
        let (file, big) = file_in(source)?;
        large |= big;
        out.push(file);
    }
    for (file, source) in out.iter_mut().zip(sources) {
        let notice = document_read::notice(&source.file).map_err(|_| Unread::Threw)?;
        file.notice = (!notice.is_empty()).then_some(notice);
    }
    Ok(Files { files: out, large })
}

/// One `onSources` entry as rag.cjs builds it.
fn entry(source: &Source) -> JValue {
    let mut pairs = vec![
        ("file", JValue::Str(source.file.clone())),
        ("body", JValue::Str(source.body.clone())),
    ];
    if let Some(score) = source.score {
        pairs.push(("score", JValue::Num(score)));
    }
    pairs.push(("kind", JValue::Str(source.kind.to_owned())));
    JValue::obj(pairs)
}

fn entries(sources: &[Source]) -> JValue {
    JValue::Arr(sources.iter().map(entry).collect())
}

/// `filesContext` with its search given: `rag_available` is `ragAvailable()`, `max_chars`
/// `FILES_CONTEXT_MAX_CHARS`, and `search` is `searchProject(projectId, query, userId)`, asked at
/// most once and only when a file is large and the index can run.
pub async fn answer_with<S, F>(
    request: &Request<'_>,
    rag_available: bool,
    max_chars: usize,
    search: S,
) -> Native
where
    S: FnOnce(String) -> F,
    F: Future<Output = Outcome>,
{
    if request.images {
        return Native::Defer("images");
    }
    if request.files.is_empty() {
        return Native::Answer(Answer::Answered {
            block: None,
            reported: None,
        });
    }
    let Some(project) = request.project_id.as_str() else {
        return Native::Defer("project id");
    };
    let files = match files(&request.files) {
        Ok(f) => f,
        Err(Unread::Threw) => return Native::Answer(Answer::Threw),
        Err(Unread::Defer(why)) => return Native::Defer(why),
    };
    let excerpts: Vec<Hit> = if rag_available && files.large {
        match search(project.to_owned()).await {
            Outcome::Hits(hits) => hits,
            Outcome::Defer(why) => return Native::Defer(why),
        }
    } else {
        Vec::new()
    };
    let Some(ctx) = context::files_context(&Input {
        query: request.query,
        files: &files.files,
        excerpts: &excerpts,
        attached: request.attached,
        max_chars,
    }) else {
        return Native::Answer(Answer::Answered {
            block: None,
            reported: None,
        });
    };
    Native::Answer(Answer::Answered {
        block: Some(ctx.text),
        reported: Some((entries(&ctx.placed), entries(&ctx.injected))),
    })
}

/// `FILES_CONTEXT_MAX_CHARS` of this process, read once (rag.cjs reads it at load; one environment
/// feeds both processes).
pub fn max_chars() -> usize {
    static MAX: OnceLock<usize> = OnceLock::new();
    *MAX.get_or_init(|| {
        context::max_chars(std::env::var("FILES_CONTEXT_MAX_CHARS").ok().as_deref())
    })
}

/// Core's `userActive` (`workspace.cjs` `isRemoved`): the account's `users` row exists, disabled
/// or not; a store that cannot be read counts as present, as Node's check fails open.
pub fn account_present(data_dir: &Path, user: &str) -> bool {
    let Ok(store) = server_store::Store::open(data_dir) else {
        return true;
    };
    store
        .read(|r| {
            r.row("SELECT id FROM users WHERE id=?1", [user], |row| {
                row.get::<_, String>(0)
            })
        })
        .map_or(true, |row| row.is_some())
}

/// Whether `searchProjectNow`'s guards leave the account's project searchable: the account is
/// present ([`account_present`]), and the account's index outbox holds no tombstone of the
/// project (unreadable: not searchable, the named difference).
pub fn searchable(data_dir: &Path, user: &str, project: &str) -> bool {
    if !account_present(data_dir, user) {
        return false;
    }
    let Ok(dir) = server_projects::store::user_dir(data_dir, user) else {
        return false;
    };
    matches!(server_projects::outbox::tombstoned(&dir, project), Ok(None))
}

/// [`answer_with`] in this process: the native search of `app` (its switch, embedding client,
/// reranker and maintenance lease), within the store route's deadline.
pub async fn answer(app: Arc<crate::App>, request: &Request<'_>) -> Native {
    if app.config.retrieval.mode == Mode::Off {
        return Native::Defer("off");
    }
    let Some(data_dir) = app.config.data_dir.clone() else {
        return Native::Defer("off");
    };
    // Without an account Node searches the shared `RAG_DIR` index itself (`nativeSearch && userId`).
    if request.user.is_empty() {
        return Native::Defer("shared index");
    }
    let rag_available = embedding_worker::sqlite_vec_loader::register().is_ok();
    let user = request.user.to_owned();
    let question = request.query.to_owned();
    let search = |project: String| async move {
        let (dir, u, p) = (data_dir.clone(), user.clone(), project.clone());
        let open = tokio::task::spawn_blocking(move || searchable(&dir, &u, &p))
            .await
            .unwrap_or(false);
        if !open {
            return Outcome::Hits(Vec::new());
        }
        let a = Arc::clone(&app);
        let rerank_env =
            tokio::task::spawn_blocking(move || crate::reranker_routes::effective_env(&a).ok())
                .await
                .ok()
                .flatten();
        let held = || app.config.maint.authority.held();
        let run = app.config.retrieval.retrieve(Inputs {
            data_dir: &data_dir,
            user: &user,
            project: &project,
            question: &question,
            start: app.config.start_id.as_deref().unwrap_or_default(),
            now_ms: crate::identity::now_ms() as f64,
            maintenance_held: &held,
            rerank_env: rerank_env.as_ref(),
            versions: Versions::new(),
            limit: usize::MAX,
            until: Some(
                std::time::Instant::now() + STORE_DEADLINE - std::time::Duration::from_secs(1),
            ),
        });
        match tokio::time::timeout(STORE_DEADLINE, run).await {
            Ok(outcome) => outcome,
            Err(_) => Outcome::Defer("deadline"),
        }
    };
    answer_with(request, rag_available, max_chars(), search).await
}

#[cfg(test)]
#[path = "native_files_context/tests.rs"]
mod tests;
