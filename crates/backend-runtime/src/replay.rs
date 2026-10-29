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
    #[serde(default = "enabled")]
    pub index_enabled: bool,
}
fn enabled() -> bool {
    true
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
    pub fn settings(&self) -> Value {
        serde_json::json!({"monthly_prefix":self.layout.monthly_prefix,"entries_prefix":self.layout.entries_prefix,"entry_layout":if self.layout.daily {"daily"} else {"monthly"},"month_file_template":self.layout.month_file_template,"index_file":self.layout.index_file,"index_enabled":self.layout.index_enabled})
    }
    pub fn months(&self, corpus: &Corpus, control: &Control) -> Result<Value> {
        let root = [
            &self.layout.root,
            if self.layout.daily {
                &self.layout.entries_prefix
            } else {
                &self.layout.monthly_prefix
            },
        ]
        .into_iter()
        .map(|s| s.trim_matches('/'))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/");
        let mut folders = vec![root];
        let mut files = HashSet::new();
        let mut seen = 0;
        while let Some(folder) = folders.pop() {
            for item in corpus.list_files(&folder, control)? {
                seen += 1;
                if seen > 10000 {
                    return Err(Fault::Capacity);
                }
                if item.is_dir {
                    if self.layout.daily {
                        folders.push(item.path);
                    }
                } else {
                    files.insert(item.path);
                }
            }
        }
        let mut months = Vec::new();
        let mut used = HashSet::new();
        for year in 2000..=2100 {
            for month in 1..=12 {
                control.check()?;
                let first = NaiveDate::from_ymd_opt(year, month, 1).ok_or(Fault::Invalid)?;
                let mut present = false;
                for number in 1..=31 {
                    let Some(day) = first.with_day(number) else {
                        break;
                    };
                    let path = self.layout.document(day)?;
                    if files.contains(&path) && used.insert(path) {
                        present = true;
                    }
                    if !self.layout.daily {
                        break;
                    }
                }
                if present {
                    months.push(serde_json::json!({"id":first.format("%Y-%m").to_string(),"label":first.format("%B %Y").to_string()}));
                }
            }
        }
        // Retain the existing API's source discriminator for frontend compatibility.
        Ok(serde_json::json!({"source":"sidecar","months":months}))
    }
    /// Browse existing layout files, preserving markers for the original editor UI.
    pub fn history(
        &self,
        corpus: &Corpus,
        month: Option<&str>,
        today: NaiveDate,
        control: &Control,
    ) -> Result<Value> {
        use serde_json::json;
        if let Some(month) = month.filter(|s| !s.is_empty()) {
            if month.len() != 7 {
                return Err(Fault::Invalid);
            }
            let first = date(&format!("{month}-01"))?;
            let mut parts = Vec::new();
            for day_number in 1..=31 {
                let Some(day) = first.with_day(day_number) else {
                    break;
                };
                if let Some(file) = corpus.read(&self.layout.document(day)?, control)? {
                    parts.push(if self.layout.daily {
                        file.content.trim().to_owned()
                    } else {
                        file.content
                    });
                }
                if !self.layout.daily {
                    break;
                }
            }
            return Ok(
                json!({"todayLog":parts.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n"),"standing":""}),
            );
        }
        let mut log = String::new();
        if let Some(file) = corpus.read(&self.layout.document(today)?, control)? {
            let expected = diary_corpus_write::canonical_day_header(today).ok_or(Fault::Invalid)?;
            if let Some(day) = diary_corpus_parse::parse_diary(&file.content)
                .into_iter()
                .find(|d| d.header == expected)
            {
                let mut parts = vec![day.header, String::new()];
                for sub in day.subsections {
                    parts.extend([format!("### {}", sub.header), String::new()]);
                    for ex in sub.exchanges {
                        if !ex.me.is_empty() {
                            parts.extend([format!("**Me:** {}", ex.me), String::new()]);
                        }
                        if !ex.claude.is_empty() {
                            parts.extend([format!("**Assistant:** {}", ex.claude), String::new()]);
                        }
                        if let Some(xid) = ex.xid {
                            parts.extend([format!("<!-- xid:{xid} -->"), String::new()]);
                        }
                    }
                }
                log = parts.join("\n").trim().to_owned();
                let count = log.chars().count();
                if count > 12000 {
                    log = format!(
                        "(earlier exchanges today truncated)\n…{}",
                        log.chars().skip(count - 12000).collect::<String>()
                    );
                }
            }
        }
        if !self.layout.index_enabled {
            return Ok(json!({"todayLog":log,"standing":""}));
        }
        let index = corpus.read(
            &self
                .layout
                .join(&[&self.layout.monthly_prefix, &self.layout.index_file])?,
            control,
        )?;
        let mut standing =
            diary_index_transform::parse_index(index.as_ref().map(|f| f.content.as_str()))
                .and_then(|i| i.render())
                .map_err(|_| Fault::Capacity)?;
        if standing.chars().count() > 4000 {
            standing = format!(
                "{}\n…(standing sections truncated)",
                standing.chars().take(4000).collect::<String>()
            );
        }
        Ok(json!({"todayLog":log,"standing":standing}))
    }
    /// Explicit human correction, under the caller's account/lifecycle writer fence.
    /// A queued predecessor participates in optimistic concurrency even before storage changes.
    pub fn edit(
        &self,
        corpus: &Corpus,
        journal: &Journal,
        request: &Value,
        control: &Control,
    ) -> Result<(u16, Value)> {
        use diary_corpus_write::{exchange_hash, exchange_text, replace_exchange_text};
        use serde_json::json;
        let bad = |message: &str| Ok((400, json!({"error":message})));
        let Some(xid) = request.get("xid").and_then(Value::as_str).map(str::trim) else {
            return bad("invalid xid");
        };
        if !(8..=64).contains(&xid.len())
            || !xid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
        {
            return bad("invalid xid");
        }
        let Some(me) = request
            .get("me")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        else {
            return bad("me is required");
        };
        let assistant = match request.get("assistant") {
            None | Some(Value::Null) => "",
            Some(Value::String(s)) => s,
            _ => return bad("invalid assistant"),
        };
        let month = match request.get("month") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.len() == 7 && date(&format!("{s}-01")).is_ok() => {
                Some(s.as_str())
            }
            _ => return bad("month must be YYYY-MM"),
        };
        let base = match request.get("base_hash") {
            None | Some(Value::Null) => None,
            Some(Value::String(s))
                if s.trim().len() == 64 && s.trim().bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                Some(s.trim().to_ascii_lowercase())
            }
            _ => return bad("invalid base_hash"),
        };
        let Some(path) = self.find(corpus, xid, month, control)? else {
            return Ok((
                404,
                json!({"error":"No corpus document contains this exchange"}),
            ));
        };
        let original = corpus.read(&path, control)?.ok_or(Fault::Missing)?.content;
        let Some(edited) = replace_exchange_text(&original, xid, me, assistant) else {
            return Ok((
                404,
                json!({"error":"The entry cannot be edited safely: malformed or missing block"}),
            ));
        };
        let current_hash = exchange_hash(Some(&original), xid);
        let current_text = exchange_text(Some(&original), xid);
        if let Some(base) = base {
            let queued = journal.unapplied_exchange_edit(xid)?;
            let queued_hash = queued.as_ref().and_then(|e| {
                let edited = replace_exchange_text(
                    &original,
                    xid,
                    e.payload.get("new_me")?.as_str()?,
                    e.payload.get("new_claude")?.as_str()?,
                )?;
                exchange_hash(Some(&edited), xid)
            });
            if current_hash.as_deref() != Some(base.as_str())
                || (queued.is_some() && queued_hash.as_deref() != Some(base.as_str()))
            {
                return Ok((
                    409,
                    json!({"error":"Entry changed or an edit is still queued; reload and retry", "conflict":true,"current_hash":current_hash,"current_text":current_text}),
                ));
            }
        }
        let day = diary_corpus_parse::parse_diary(&original)
            .into_iter()
            .filter_map(|s| s.date)
            .filter_map(|v| date(&v).ok())
            .find(|d| self.layout.document(*d).ok().as_deref() == Some(&path))
            .ok_or(Fault::Invalid)?;
        // The original monthly store reports the owning document's first day.
        let day = if self.layout.daily {
            day
        } else {
            day.with_day(1).ok_or(Fault::Invalid)?
        };
        let id = journal.enqueue(
            "exchange_edit",
            &json!({"xid":xid,"new_me":me,"new_claude":assistant,"month":month}),
        )?;
        self.pending(corpus, journal, 10000, false, control)?;
        let stored = corpus.read(&path, control)?;
        if !journal.is_applied(&id)?
            || exchange_hash(stored.as_ref().map(|f| f.content.as_str()), xid)
                != exchange_hash(Some(&edited), xid)
        {
            return Ok((
                503,
                json!({"error":"Correction is queued but could not be saved yet. It will retry when storage is available."}),
            ));
        }
        Ok((
            200,
            json!({"ok":true,"document":path,"day":day.format("%Y-%m-%d").to_string(),"hash":exchange_hash(Some(&edited),xid)}),
        ))
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
