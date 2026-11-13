//! The tenant-side adapters the Rust answering exchange needs (R9): the model-context day text,
//! the memory-folder text and the durable save of one exchange, over a ready managed tenant, plus
//! the `diary_core` `Store` and `Journal` implementations built on them. Nothing calls this
//! module in production: the route that will use it comes later, behind a switch that is off by
//! default.
//!
//! Ported from `CorpusStore.get_day_text(include_markers=False)` and `CorpusStore.log_exchange`
//! (corpus_store.py) and `workspace_files.memory_text` + `reference_text` (workspace_files.py).
//! `tests/answer_store.rs` replays recorded Python outcomes
//! (`tests/answer-store-fixtures.json`, from `tests/generate_answer_store_fixtures.py`) through
//! these functions and compares them.
//!
//! # INDEX.md tenants are declined by construction
//!
//! Python enqueues an `index_month` entry, reads the standing sections and updates them when a
//! tenant's INDEX.md is enabled. This side does none of that: `corpus::layout` returns `None` for
//! a tenant with INDEX.md enabled, so a `Layout` (which every function here takes) can never
//! describe one, and the route that builds the context declines such tenants before it gets here.
//! Hence `standing_sections_text` is always empty, `index_enabled` is false and
//! `update_standing_sections` never has anything to do (`Ok(None)`).
//!
//! # Known differences from Python
//!
//! - `journal.dirty_documents`: the written document is marked dirty before the write, as the
//!   append route does (the retrieval index is not refreshed here; Python's next reindex pass
//!   picks the document up). Python's `log_exchange` does not mark anything itself: its caller
//!   reindexes afterwards.
//! - The text of a failure (`Err((false, message))`) is this side's own wording; Python's
//!   `str(exception)` differs. The kind of failure matches: only failures before the journal row
//!   exists, or a journal operation failing after it, are errors; a row whose write failed stays
//!   pending and is `Err((true, ..))` (Python's `NativeWriteUnavailable`, which it raises only
//!   with DIARY_CORPUS_WRITE_IMPL=rust, the only mode this side serves).
//! - A folder listing whose Python order this side cannot reproduce is not guessed at:
//!   `memory_text` reports `Undetermined` and the adapter records it (`AnswerStore::undetermined`)
//!   so the route can decline to Python before it calls the model. That is: two listed entries
//!   that differ only in letter case, unless both are directories (`memory` next to `Memory` is
//!   fine: directories are never read), and a letter newer than Python's Unicode tables. Python
//!   keeps the database scan order among such names, which its SQLite may choose differently.
//!   Fixture cases `same-lower-case-names`, `root-same-lower-case-names`,
//!   `mixed-tie-file-and-directory` and `case-fold-newer-than-python`.
//! - `memory_text` has no `layout` argument: Python's takes none, and a managed tenant has no
//!   remote root.

use crate::corpus::{self, Layout, ListError};
use crate::managed::Managed;
use crate::workspace::{self, Filtered, Read};
use crate::write::{self, PyVal};
use crate::writes::{self, Ctx};
use chrono::{Datelike, NaiveDate, NaiveTime};
use diary_core::assembler::{Store as ContextStore, StoreError};
use diary_core::pipeline::{Failed, Journal as ExchangeJournal};
use diary_corpus_write as cw;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};

/// `get_day_text(day, max_chars, include_markers=False)`: the day's log for the model context,
/// hidden xid markers stripped. `max_chars` 0 means no cap; a read failure degrades to "".
pub fn context_day_text(
    store: &Managed,
    layout: &Layout,
    day: NaiveDate,
    max_chars: i64,
) -> String {
    corpus::day_text_with(store, layout, day, max_chars, false)
}

/// `memory_text` found a state whose Python result it does not reproduce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Undetermined;

/// The folders `memory_text` reads, in order ("" is the diary root).
const MEMORY_FOLDERS: [&str; 6] = ["", "AI Memory", "memory", "Memory", "context", "Context"];
/// The only root files read (compared case-insensitively).
const ROOT_FILES: [&str; 3] = ["memory.md", "context.md", "instructions.md"];
const LISTED: usize = 40;
const FILES: usize = 8;
const PER_FILE_CHARS: usize = 4000;
const TOTAL_CHARS: usize = 16000;

/// The first `n` characters (code points) of `s`: Python `s[:n]`.
fn head_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((at, _)) => s.get(..at).unwrap_or(s),
        None => s,
    }
}

/// `reference_text(files, query='')`: an empty query scores every file 0, so the stable sort keeps
/// the order the files were found in.
fn reference_text(files: &[(String, String)]) -> String {
    let joined = files
        .iter()
        .take(FILES)
        .map(|(path, text)| format!("--- {path} ---\n{}", head_chars(text, PER_FILE_CHARS)))
        .collect::<Vec<_>>()
        .join("\n\n");
    head_chars(&joined, TOTAL_CHARS).to_string()
}

/// `workspace_files.memory_text(store)`: the explicit memory area, bounded, as one reference
/// block. Any failure after the first file ends the scan with the text gathered so far (Python's
/// blanket `except Exception`); a folder that cannot be listed is skipped.
pub fn memory_text(store: &Managed) -> Result<String, Undetermined> {
    let mut texts: Vec<(String, String)> = Vec::new();
    'folders: for folder in MEMORY_FOLDERS {
        let entries = match workspace::filtered_listing(store, folder, true, |_| true) {
            Filtered::Items(items) => items,
            // file_list raised (the 413, or a failed listing): this folder is skipped.
            Filtered::TooMany | Filtered::Failed => continue,
            Filtered::Undetermined => return Err(Undetermined),
        };
        for entry in entries.into_iter().take(LISTED) {
            if entry.is_dir {
                continue;
            }
            if folder.is_empty() && !ROOT_FILES.contains(&entry.name.to_lowercase().as_str()) {
                continue;
            }
            let path = if folder.is_empty() {
                entry.name.clone()
            } else {
                format!("{folder}/{}", entry.name)
            };
            match workspace::read_text(store, &path) {
                Read::Text(Some(content)) => {
                    if !content.is_empty() {
                        let kept = head_chars(&content, PER_FILE_CHARS).to_string();
                        match texts.iter_mut().find(|(p, _)| *p == path) {
                            Some(slot) => slot.1 = kept,
                            None => texts.push((path, kept)),
                        }
                    }
                }
                Read::Text(None) => {}
                // file_read raised (an unsafe name, the 512 KiB limit, a failed read): Python's
                // outer handler ends the whole scan.
                Read::BadPath(_) | Read::Failed | Read::TooLarge => break 'folders,
            }
            if texts.len() >= FILES {
                break 'folders;
            }
        }
    }
    Ok(reference_text(&texts))
}

/// `CorpusStore.log_exchange(day, topic, me_text, claude_text, now)` with a fresh xid, over the
/// tenant the caller holds (`Ctx`, under the tenant's writer slot). Returns the xid.
///
/// `Err((true, ..))`: the journal row exists but its write is not applied (Python's
/// `NativeWriteUnavailable`); the outcome is uncertain until journal replay. `Err((false, ..))`:
/// any other failure.
pub fn log_exchange(
    c: &Ctx<'_>,
    day: NaiveDate,
    now: NaiveTime,
    topic: &str,
    me_text: &str,
    claude_text: &str,
) -> Result<String, (bool, String)> {
    let failed = |e: write::Error| (false, e.into_message());
    let l = c.layout;
    let xid = write::uuid4().map_err(failed)?;
    let rendered = cw::render_subsection_header(now, topic);
    // The header TEXT is stored (no "### " prefix): the appender adds the prefix.
    let sub_header = rendered
        .strip_prefix("### ")
        .map_or_else(|| rendered.clone(), str::to_owned);
    let body = cw::render_exchange(me_text, claude_text, &xid);
    // `not journal.has_exchange() and not list_months()`: the months are listed only for a tenant
    // with no exchange yet (an unlistable template is an error only then).
    let has_exchange = c.journal.has_exchange().map_err(failed)?;
    // Existing journals/corpora are not migrated: only a tenant with neither is scaffolded.
    let scaffold = !has_exchange
        && corpus::list_months(c.reader, l)
            .map_err(|ListError::Template| {
                (
                    false,
                    "the month file template cannot be listed".to_string(),
                )
            })?
            .is_empty();
    let month = corpus::month_filename(l, day);
    let label = format!("{} {}", corpus::month_name(day.month()), day.year());
    let document = corpus::document_path(l, day);
    let day_iso = day.format("%Y-%m-%d").to_string();
    let payload = write::py_json(&[
        ("scaffold", PyVal::Bool(scaffold)),
        ("xid", PyVal::Str(&xid)),
        ("day", PyVal::Str(&day_iso)),
        ("sub_header", PyVal::Str(&sub_header)),
        ("body", PyVal::Str(&body)),
        ("month", PyVal::Str(&month)),
        ("document", PyVal::Str(&document)),
        ("month_label", PyVal::Str(&label)),
    ]);
    // Dirty BEFORE anything is written (see the module doc and `writes::append`): a process lost
    // at any later point still leaves its retrieval invalidation behind.
    c.journal.mark_dirty(&document).map_err(failed)?;
    let id = c.journal.enqueue("exchange", &payload).map_err(failed)?;
    let applied = writes::apply_entry(c, &id, || {
        c.store.guarded_write(&document, |current| {
            if let Some(text) = current.filter(|t| cw::has_marker(t, &xid)) {
                return Ok((text.to_string(), false));
            }
            cw::append_to_month_text(current, day, &sub_header, &body)
                .map(|t| (t, true))
                .ok_or_else(|| "invalid day".to_string())
        })?;
        if scaffold {
            writes::seed(c)?;
        }
        Ok(())
    });
    match applied {
        Ok(true) => Ok(xid),
        Ok(false) => Err((true, writes::PENDING_NATIVE.to_string())),
        Err(e) => Err(failed(e)),
    }
}

/// The adapters for one exchange: `diary_core`'s corpus `Store` (the assembler) and `Journal` (the
/// pipeline) over the tenant the caller holds. `now` is the exchange's clock reading (the saved
/// entry's subsection time).
pub struct AnswerStore<'a, 'c> {
    ctx: &'c Ctx<'a>,
    now: NaiveTime,
    undetermined: AtomicBool,
}

impl<'a, 'c> AnswerStore<'a, 'c> {
    pub fn new(ctx: &'c Ctx<'a>, now: NaiveTime) -> Self {
        AnswerStore {
            ctx,
            now,
            undetermined: AtomicBool::new(false),
        }
    }

    /// Did `memory_text` meet a listing it cannot reproduce? The text it returned then is not
    /// Python's: the route must decline (to Python) before calling the model.
    pub fn undetermined(&self) -> bool {
        self.undetermined.load(Ordering::SeqCst)
    }
}

impl ContextStore for AnswerStore<'_, '_> {
    fn day_text(&self, day: NaiveDate, max_chars: i64) -> Result<String, StoreError> {
        Ok(context_day_text(
            self.ctx.reader,
            self.ctx.layout,
            day,
            max_chars,
        ))
    }

    /// Always empty: tenants with INDEX.md enabled are declined (module doc).
    fn standing_sections_text(&self, _max_chars: i64) -> Result<String, StoreError> {
        Ok(String::new())
    }

    fn memory_text(&self) -> String {
        match memory_text(self.ctx.reader) {
            Ok(text) => text,
            Err(Undetermined) => {
                self.undetermined.store(true, Ordering::SeqCst);
                String::new()
            }
        }
    }
}

impl ExchangeJournal for AnswerStore<'_, '_> {
    fn log_exchange(
        &mut self,
        day: &str,
        sub_header: &str,
        me_text: &str,
        claude_text: &str,
    ) -> Result<String, (bool, String)> {
        let Ok(day) = NaiveDate::parse_from_str(day, "%Y-%m-%d") else {
            return Err((false, format!("invalid day {day:?}")));
        };
        log_exchange(self.ctx, day, self.now, sub_header, me_text, claude_text)
    }

    fn index_enabled(&self) -> bool {
        false
    }

    fn standing_sections_text(&mut self, _max_chars: i64) -> Result<String, Failed> {
        Ok(String::new())
    }

    /// Unreachable while `index_enabled` is false (the pipeline gates on it).
    fn update_standing_sections(
        &mut self,
        _open_questions: &Value,
        _timeline: &Value,
        _today: &str,
    ) -> Result<Option<String>, Failed> {
        Ok(None)
    }
}
