//! Managed-local execution of the original four Diary journal intent kinds.
//! The owner must hold the live account/lifecycle fence for an entire replay pass.
use crate::{
    diary::{Corpus, WriteCondition},
    journal::{Entry, Journal},
    Control, Fault, Result,
};
use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashSet,
    sync::Mutex,
    time::{Duration, Instant},
};
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Layout {
    pub root: String,
    pub monthly_prefix: String,
    pub entries_prefix: String,
    pub month_file_template: String,
    pub index_file: String,
    pub daily: bool,
}
impl Layout {
    fn join(&self, parts: &[&str]) -> Result<String> {
        let path = std::iter::once(self.root.as_str())
            .chain(parts.iter().copied())
            .map(|s| s.trim_matches('/'))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("/");
        diary_workspace_rules::rel_path(&path, false).map_err(|_| Fault::Invalid)
    }
    fn document(&self, day: NaiveDate) -> Result<String> {
        if self.daily {
            self.join(&[
                &self.entries_prefix,
                &day.year().to_string(),
                &day.format("%B").to_string(),
                &format!("{} {}, {}.md", day.format("%B"), day.day(), day.year()),
            ])
        } else {
            let file = self
                .month_file_template
                .replace("{month:02d}", "{month02}")
                .replace("{year}", &day.year().to_string())
                .replace("{month02}", &format!("{:02}", day.month()))
                .replace("{month}", &day.month().to_string())
                .replace("{month_name}", &day.format("%B").to_string());
            if file.contains(['{', '}']) {
                return Err(Fault::Invalid);
            }
            self.join(&[&self.monthly_prefix, &file])
        }
    }
}
pub struct Replay {
    layout: Layout,
    until: Mutex<Option<Instant>>,
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key).and_then(Value::as_str).ok_or(Fault::Invalid)
}
fn date(v: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(v, "%Y-%m-%d").map_err(|_| Fault::Invalid)
}
impl Replay {
    pub fn new(layout: Layout) -> Result<Self> {
        layout.document(date("2026-01-01")?)?;
        layout.join(&[&layout.monthly_prefix, &layout.index_file])?;
        Ok(Self {
            layout,
            until: Mutex::new(None),
        })
    }
    fn write(
        &self,
        corpus: &Corpus,
        journal: &Journal,
        path: &str,
        control: &Control,
        mutate: impl Fn(Option<&str>) -> Result<Option<String>>,
    ) -> Result<()> {
        journal.mark_dirty(path)?;
        for _ in 0..5 {
            control.check()?;
            let current = corpus.read(path, control)?;
            let Some(next) = mutate(current.as_ref().map(|v| v.content.as_str()))? else {
                return Ok(());
            };
            if current.as_ref().is_some_and(|v| v.content == next) {
                return Ok(());
            }
            match corpus.put(
                path,
                &next,
                WriteCondition {
                    if_match: current.as_ref().map(|v| v.version.as_str()),
                    if_none_match: current.is_none(),
                },
                chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
                control,
                || control.check(),
            ) {
                Ok(_) => return Ok(()),
                Err(Fault::Conflict) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(Fault::Conflict)
    }
    fn apply(
        &self,
        corpus: &Corpus,
        journal: &Journal,
        entry: &Entry,
        control: &Control,
    ) -> Result<()> {
        let p = &entry.payload;
        match entry.kind.as_str() {
            "exchange" => {
                let day = date(text(p, "day")?)?;
                let xid = text(p, "xid")?;
                let header = text(p, "sub_header")?;
                let body = text(p, "body")?;
                let path = self.layout.document(day)?;
                self.write(corpus, journal, &path, control, |old| {
                    if old.is_some_and(|s| diary_corpus_write::has_marker(s, xid)) {
                        return Ok(None);
                    }
                    diary_corpus_write::append_to_month_text(old, day, header, body)
                        .map(Some)
                        .ok_or(Fault::Invalid)
                })?;
                if p.get("scaffold") == Some(&Value::Bool(true)) {
                    for (folder, seed) in [
                        (
                            &self.layout.entries_prefix[..],
                            "Diary entries are saved in the configured daily or monthly layout.\n",
                        ),
                        (
                            "AI Memory",
                            "Keep durable diary memory and context as Markdown files here.\n",
                        ),
                        (
                            "Raw Sources",
                            "Keep original reference material for your diary here.\n",
                        ),
                    ] {
                        self.write(
                            corpus,
                            journal,
                            &self.layout.join(&[folder, "README.md"])?,
                            control,
                            |old| Ok(old.is_none().then(|| seed.to_owned())),
                        )?;
                    }
                }
                Ok(())
            }
            "exchange_edit" => {
                let xid = text(p, "xid")?;
                let me = text(p, "new_me")?;
                let assistant = text(p, "new_claude")?;
                let path = self
                    .find(corpus, xid, p.get("month").and_then(Value::as_str), control)?
                    .ok_or(Fault::Missing)?;
                self.write(corpus, journal, &path, control, |old| {
                    diary_corpus_write::replace_exchange_text(
                        old.ok_or(Fault::Missing)?,
                        xid,
                        me,
                        assistant,
                    )
                    .map(Some)
                    .ok_or(Fault::Invalid)
                })
            }
            "index_month" | "index_update" => {
                let index = self
                    .layout
                    .join(&[&self.layout.monthly_prefix, &self.layout.index_file])?;
                self.write(corpus, journal, &index, control, |old| {
                    let mut idx =
                        diary_index_transform::parse_index(old).map_err(|_| Fault::Capacity)?;
                    let changed = if entry.kind == "index_month" {
                        diary_index_transform::register_month(
                            &mut idx,
                            text(p, "label")?,
                            text(p, "month")?,
                        )
                        .map_err(|_| Fault::Capacity)?
                    } else {
                        diary_index_transform::apply_index_edits(
                            &mut idx,
                            &edits(p.get("open_questions"))?,
                            &edits(p.get("timeline"))?,
                            p.get("today")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .unwrap_or(&chrono::Local::now().format("%Y-%m-%d").to_string()),
                        )
                        .map_err(|_| Fault::Capacity)?
                    };
                    if changed {
                        Ok(Some(idx.render().map_err(|_| Fault::Capacity)?))
                    } else {
                        Ok(None)
                    }
                })
            }
            _ => Err(Fault::Invalid),
        }
    }
    fn find(
        &self,
        corpus: &Corpus,
        xid: &str,
        month: Option<&str>,
        control: &Control,
    ) -> Result<Option<String>> {
        // Restrict edits to real layout documents, not arbitrary files containing copied markers.
        let root = self.layout.join(&[if self.layout.daily {
            &self.layout.entries_prefix
        } else {
            &self.layout.monthly_prefix
        }]);
        let root = match root {
            Ok(p) => p,
            Err(_) if self.layout.root.is_empty() && self.layout.monthly_prefix.is_empty() => {
                String::new()
            }
            Err(e) => return Err(e),
        };
        let mut folders = vec![root];
        let mut candidates = Vec::new();
        let mut scanned = 0;
        while let Some(folder) = folders.pop() {
            control.check()?;
            for item in corpus.list_files(&folder, control)? {
                scanned += 1;
                if scanned > 10000 {
                    return Err(Fault::Capacity);
                }
                if item.is_dir {
                    if self.layout.daily {
                        folders.push(item.path)
                    }
                    continue;
                }
                let Some(file) = corpus.read(&item.path, control)? else {
                    continue;
                };
                if !diary_corpus_write::has_marker(&file.content, xid) {
                    continue;
                }
                for section in diary_corpus_parse::parse_diary(&file.content) {
                    let Some(day) = section.date.as_deref().and_then(|v| date(v).ok()) else {
                        continue;
                    };
                    if self.layout.document(day)? == item.path
                        && month.is_none_or(|m| day.format("%Y-%m").to_string() == m)
                    {
                        candidates.push((day, item.path.clone()));
                        break;
                    }
                }
            }
        }
        candidates.sort();
        Ok(candidates.pop().map(|(_, path)| path))
    }
    pub fn pending(
        &self,
        corpus: &Corpus,
        journal: &Journal,
        limit: u32,
        force: bool,
        control: &Control,
    ) -> Result<usize> {
        let mut until = self.until.lock().map_err(|_| Fault::Unavailable)?;
        if !force && until.is_some_and(|v| Instant::now() < v) {
            return Ok(0);
        }
        let mut retired = HashSet::new();
        let mut applied = 0;
        for entry in journal.unapplied(limit)? {
            control.check()?;
            if retired.contains(&entry.id) {
                continue;
            }
            match self.apply(corpus, journal, &entry, control) {
                Ok(()) => {
                    journal.mark_applied(&entry.id)?;
                    applied += 1
                }
                Err(e)
                    if matches!(e, Fault::Invalid | Fault::Missing)
                        || (entry.attempts >= 19
                            && matches!(e, Fault::Conflict | Fault::Capacity)) =>
                {
                    journal.quarantine(&entry.id, &format!("quarantined: {e}"))?;
                    if let Some(xid) = entry
                        .payload
                        .get("xid")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    {
                        for dependent in
                            journal.unapplied_after_with_xid(journal.rowid_of(&entry.id)?, xid)?
                        {
                            journal.quarantine(
                                &dependent.id,
                                &format!("quarantined: depends on quarantined {}", entry.id),
                            )?;
                            retired.insert(dependent.id);
                        }
                    }
                }
                Err(e) => {
                    journal.mark_failed(&entry.id, &e.to_string())?;
                    *until = Instant::now().checked_add(Duration::from_secs(30));
                    break;
                }
            }
        }
        Ok(applied)
    }
}
fn edits(v: Option<&Value>) -> Result<Vec<diary_index_transform::EditOp>> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    v.as_array()
        .ok_or(Fault::Invalid)?
        .iter()
        .map(|v| {
            Ok(diary_index_transform::EditOp {
                action: v
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                text: text(v, "text")?.to_owned(),
                replacement: v
                    .get("replacement")
                    .map(|v| v.as_str().map(|s| Some(s.to_owned())).ok_or(Fault::Invalid))
                    .transpose()?,
                date: v.get("date").and_then(Value::as_str).map(str::to_owned),
            })
        })
        .collect()
}
