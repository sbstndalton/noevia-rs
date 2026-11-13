//! Long mode (#1079, #1093; core llamacpp-full-autotune.cjs lines 17-26 and 1467-1535): a Long
//! tune's `<model>-long` section, added once from the model's own section through the usual
//! writer and backup rules, and picked up by the router before anything is measured.

use crate::deps::{Call, Deps, Fail};
use crate::engine::string_of;
use crate::js;
use crate::runner::Runner;
use js_json::JValue;

/// `longRefusal(reason)`.
pub(crate) fn refusal(reason: &JValue) -> &'static str {
    match reason.as_str() {
        Some("invalid_id") => "This model's name is too long, or not a valid models.ini name, for a long-context profile.",
        Some("ambiguous") => "models.ini has a section the model server would read differently (a duplicate or unusual header), so auto-tune does not add one. Check models.ini, then try again.",
        Some("no_base") => "This model has no section of its own in models.ini, so there is nothing to copy for a long-context profile.",
        Some("no_model") => "The model server does not say which file this model loads, so a long-context profile cannot point at it.",
        Some("bad_path") => "This model's file path cannot be written to models.ini safely, so no long-context profile was added.",
        Some("too_large") => "models.ini would pass its 1 MiB limit with a long-context profile added.",
        _ => "The long-context profile could not be prepared safely; nothing was changed.",
    }
}

/// `longClash(item)`.
pub(crate) fn clash(model: &str) -> String {
    format!("models.ini already has a section named {model} that loads a different file. Rename or remove it, then run a Long tune.")
}

impl<D: Deps> Runner<D> {
    /// The injected `reloadRouter()`, or the plain router re-read.
    async fn router_reload(&self) -> Result<JValue, Fail> {
        match self.deps().reload_router() {
            Some(reload) => reload.await,
            None => {
                self.request(Call::get("/models?reload=1", 120_000), None)
                    .await
            }
        }
    }

    async fn served(&self, base: &JValue, long: &JValue) -> Result<bool, Fail> {
        let (_, pairs) = self.router_pairs().await?;
        Ok(js::items(&pairs)
            .iter()
            .any(|p| p.get("base") == base && p.get("long") == long))
    }

    /// `adoptPendingRevision(j)` (#1093): a file at the revision this job was about to write is its
    /// own write.
    pub(crate) fn adopt_pending_revision(&self) -> Result<(), Fail> {
        let j = self.job();
        if matches!(j.get("_pendingRevision"), JValue::Undefined) {
            return Ok(());
        }
        let (_, revision) = self.deps().presets_snapshot();
        self.with_job(|j| {
            if j.get("_pendingRevision").as_str() == Some(revision.as_str()) {
                let pending = j.get("_pendingRevision").clone();
                js::set(j, "_revision", pending);
            }
            js::del(j, "_pendingRevision");
        });
        self.save()
    }

    /// `finishPendingLongSection(j, item)` (#1093): a section this job wrote that the router has
    /// not listed yet gets one more re-read.
    async fn finish_pending_long_section(&self, mi: usize) -> Result<(), Fail> {
        let item = self.item(mi);
        let _ = self.router_reload().await;
        if self.served(item.get("base"), item.get("model")).await? {
            return self.note(&format!(
                "The long-context profile {} is served; continuing.",
                string_of(item.get("model"))
            ));
        }
        Err(Fail::public(format!(
            "The long-context profile {} is in models.ini, but the model server has not picked it up yet. Check the model server, then Resume.",
            string_of(item.get("model"))
        ))
        .with_fatal())
    }

    /// `ensureLongSection(j, item)`: `item.model` (`<base>-long`) is a section the router serves for
    /// `item.base`'s file, added here once if needed (and taken out again if the router does not
    /// pick it up).
    pub(crate) async fn ensure_long_section(&self, mi: usize) -> Result<(), Fail> {
        self.check()?;
        self.guard_revision()?;
        let item = self.item(mi);
        let (model, base) = (item.get("model").clone(), item.get("base").clone());
        let model_text = string_of(&model);
        let (rows, pairs) = self.router_pairs().await?;
        if self.deps().presets_get(&model_text).get("exists").truthy() {
            if js::items(&pairs)
                .iter()
                .any(|p| p.get("base") == &base && p.get("long") == &model)
            {
                return Ok(());
            }
            if item.get("sectionCreated").truthy() {
                return self.finish_pending_long_section(mi).await;
            }
            return Err(Fail::public(clash(&model_text)).with_fatal());
        }
        let Some(row) = js::items(&rows)
            .iter()
            .find(|r| r.get("id") == &base)
            .cloned()
        else {
            return Err(Fail::public("A queued model is no longer configured.").with_fatal());
        };
        self.unload_all(false).await?;
        let (text, revision) = self.deps().presets_snapshot();
        if self.job().get("_revision").as_str() != Some(revision.as_str()) {
            return Err(Fail::outside_edit());
        }
        let made = self.deps().long_section(&text, &row);
        if !made.get("ok").truthy() {
            return Err(Fail::public(refusal(made.get("reason"))).with_fatal());
        }
        if made.get("id") != &model {
            return Err(Fail::public(refusal(&JValue::from("unavailable"))).with_fatal());
        }
        let made_text = made.get("text").as_str().unwrap_or("").to_string();
        // #1093: the section and the revision it produces are recorded before the write.
        let pending = self.deps().revision_of(&made_text);
        self.with_item(mi, |item| js::set(item, "sectionCreated", true));
        self.with_job(|j| js::set(j, "_pendingRevision", pending.as_str()));
        self.save()?;
        self.deps()
            .presets_commit(
                JValue::obj([
                    ("baseRevision", JValue::from(revision.as_str())),
                    ("text", JValue::from(made_text)),
                ]),
                self.write_options(),
            )
            .await?;
        self.wrote()?;
        self.with_job(|j| {
            let pending = j.get("_pendingRevision").clone();
            js::set(j, "_revision", pending);
            js::del(j, "_pendingRevision");
        });
        self.save()?;
        let served = match self.router_reload().await {
            Ok(r) if r.get("ok").truthy() => self.served(&base, &model).await.unwrap_or(false),
            _ => false,
        };
        if !served {
            // Put the file back as it was, unless it moved on since (then it is the operator's).
            let undo: Result<(), Fail> = async {
                let (_, now) = self.deps().presets_snapshot();
                if self.job().get("_revision").as_str() == Some(now.as_str()) {
                    self.deps()
                        .presets_commit(
                            JValue::obj([
                                ("baseRevision", JValue::from(now)),
                                ("text", JValue::from(text)),
                            ]),
                            JValue::obj([("backup", JValue::Bool(false))]),
                        )
                        .await?;
                    let (_, after) = self.deps().presets_snapshot();
                    self.with_job(|j| js::set(j, "_revision", after));
                    self.with_item(mi, |item| js::set(item, "sectionCreated", false));
                    self.save()?;
                    let _ = self.router_reload().await;
                }
                Ok(())
            }
            .await;
            let _ = undo;
            return Err(Fail::public(format!(
                "The model server did not pick up the long-context profile {model_text}. models.ini was put back; check the model server, then try again."
            ))
            .with_fatal());
        }
        self.note(&format!(
            "Added the long-context profile {model_text} to models.ini, copied from {}'s settings. This tune changes only that profile.",
            string_of(&base)
        ))
    }
}
