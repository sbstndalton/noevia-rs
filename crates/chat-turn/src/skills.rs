//! A native turn's instruction skills (#272, #546): chat.cjs's `revokedSkills`, `noteSkillRead`, the round-0
//! scrub and their texts, for a project turn core handed off with its skills (the prepared frame's `skills`).
//! What core keeps (the project as stored now, the account's skill ledger) is asked through the host, which
//! calls core's skills bridges; the turn keeps what chat.cjs keeps in its closure: the loaded skills, the
//! revocation once found (it stays for the rest of the turn), when it last checked, and the names already taken
//! out of the history.
//!
//! Named differences, each the executor's own (Node asks no bridge, so it has no such failure):
//! - a revocation check the bridge gives no answer to counts as every loaded skill revoked (fail closed);
//! - a read the bridge gives no answer to counts as reading every candidate: they are tracked (so a later
//!   change revokes them, the stricter direction) but not recorded in the ledger;
//! - a round-0 scrub the bridge gives no answer to ends the turn with index.cjs's failure text, as a handler
//!   that throws there does, rather than sending a history that may still carry a revoked skill.

use std::sync::Mutex;

use js_json::JValue;

use crate::host::{Host, Skill};
use crate::js;
use crate::prepared::Prepared;

/// The skill state of one turn.
#[derive(Debug)]
pub struct Skills {
    project_id: Option<String>,
    chat_id: Option<String>,
    /// The frame carried skills: the round-0 scrub is asked (an older core sends none).
    scrubs: bool,
    enabled: Vec<Skill>,
    image_warning: String,
    coverage_warning: String,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    loaded: Vec<Skill>,
    revoked: Option<Vec<(String, String)>>,
    checked_at: f64,
    in_history: Vec<String>,
}

/// `JSON.stringify(name)` of each, joined by ", ".
fn quoted(names: &[&str]) -> String {
    names
        .iter()
        .map(|n| js::stringify(&js::s(*n)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// chat.cjs `historyNote(names)`.
pub fn history_note(names: &[String]) -> String {
    let one = names.len() == 1;
    let list: Vec<&str> = names.iter().map(String::as_str).collect();
    format!(
        "Earlier replies in this chat used {} {}, which {} now disabled or changed. {} instructions were left out of this request.",
        if one { "Skill" } else { "Skills" },
        quoted(&list),
        if one { "is" } else { "are" },
        if one { "Its" } else { "Their" }
    )
}

impl Skills {
    pub fn new(p: &Prepared) -> Skills {
        let frame = p.skills.clone().unwrap_or_default();
        Skills {
            project_id: p.project_id.clone(),
            chat_id: p.chat_id.clone(),
            scrubs: p.skills.is_some() && p.project_id.is_some(),
            enabled: frame.enabled,
            image_warning: frame.image_warning,
            coverage_warning: frame.coverage_warning,
            state: Mutex::new(State {
                loaded: frame.loaded,
                in_history: frame.revoked_in_history,
                ..State::default()
            }),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> Option<T> {
        self.state.lock().ok().map(|mut s| f(&mut s))
    }

    /// A loaded skill was found revoked earlier in the turn.
    pub fn is_revoked(&self) -> bool {
        self.with(|s| s.revoked.is_some()).unwrap_or(true)
    }

    /// chat.cjs `revokedSkills()`: the revocation found so far, else, with skills loaded, a check against the
    /// project as stored now (the check's time is kept for the mid-stream check).
    pub async fn revoked<H: Host>(&self, host: &H) -> bool {
        let Some(project) = self.project_id.as_deref() else {
            return self.is_revoked();
        };
        let loaded = self.with(|s| {
            if s.revoked.is_some() {
                return None;
            }
            if s.loaded.is_empty() {
                return Some(Vec::new());
            }
            s.checked_at = host.now_ms();
            Some(s.loaded.clone())
        });
        let loaded = match loaded {
            Some(Some(l)) if !l.is_empty() => l,
            Some(Some(_)) => return false,
            _ => return true,
        };
        let gone = host.skills_revoked(project, &loaded).await.unwrap_or_else(|| {
            loaded
                .iter()
                .map(|k| {
                    let name = if k.name.is_empty() { &k.file } else { &k.name };
                    (k.file.clone(), name.clone())
                })
                .collect()
        });
        if gone.is_empty() {
            return false;
        }
        self.with(|s| s.revoked = Some(gone));
        true
    }

    /// The stream's check is due: skills are loaded and the last check is at least a second old (chat.cjs
    /// `loadedSkills.size && Date.now() - skillCheckedAt >= 1000`).
    pub fn due(&self, now: f64) -> bool {
        self.with(|s| !s.loaded.is_empty() && now - s.checked_at >= 1000.0)
            .unwrap_or(false)
    }

    /// chat.cjs `revocationText()`.
    pub fn text(&self) -> String {
        let names = self
            .with(|s| {
                s.revoked
                    .as_ref()
                    .map(|g| g.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let list: Vec<&str> = names.iter().map(String::as_str).collect();
        format!(
            "Skill {} was disabled or changed during this reply, so no further steps were run.",
            quoted(&list)
        )
    }

    /// chat.cjs `refuseForRevokedSkill(name)`: audited, and the call's result.
    pub fn refuse<H: Host>(&self, host: &H, name: &str) -> String {
        host.audit(
            "tool.denied",
            &js::obj([("tool", js::s(name)), ("reason", js::s("skill-revoked"))]),
        );
        format!("ERROR: {} {name} was not run. Do not retry it.", self.text())
    }

    /// The result of a call streamed in a round the revocation cut off (chat.cjs, at most 300 UTF-16 units).
    pub fn not_run(&self, name: &str) -> String {
        let name = if name.is_empty() { "This tool" } else { name };
        js::slice(&format!("ERROR: {} {name} was not run.", self.text()), 300)
    }

    /// chat.cjs `noteSkillRead(args, result)` after a successful call: the enabled skills not loaded yet that
    /// it read are loaded too (core records them).
    pub async fn note_read<H: Host>(&self, host: &H, args: &str, result: &str) {
        let Some(project) = self.project_id.as_deref() else {
            return;
        };
        let candidates: Vec<Skill> = self
            .with(|s| {
                self.enabled
                    .iter()
                    .filter(|k| !s.loaded.iter().any(|l| l.file == k.file))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if candidates.is_empty() {
            return;
        }
        let read = host
            .skills_read(project, self.chat_id.as_deref(), &candidates, args, result)
            .await
            .unwrap_or(candidates);
        self.with(|s| {
            for k in read {
                if !s.loaded.iter().any(|l| l.file == k.file) {
                    s.loaded.push(k);
                }
            }
        });
    }

    /// The round-0 scrub (chat.cjs before its first round): whether the messages changed, and the warning to
    /// send when it took out a skill not named before. `Err` when the bridge gave no answer.
    pub async fn scrub<H: Host>(
        &self,
        host: &H,
        messages: &mut [JValue],
    ) -> Result<(bool, Option<String>), ()> {
        let Some(project) = self.project_id.as_deref().filter(|_| self.scrubs) else {
            return Ok((false, None));
        };
        let view: Vec<JValue> = messages
            .iter()
            .map(|m| match (m.get("role").as_str(), m.get("content")) {
                (Some(role), JValue::Str(content)) if role != "user" && role != "system" => {
                    js::obj([("role", js::s(role)), ("content", js::s(content.clone()))])
                }
                _ => JValue::Null,
            })
            .collect();
        let scrub = host
            .skills_scrub(project, self.chat_id.as_deref(), &view)
            .await
            .ok_or(())?;
        if scrub.removed.is_empty() {
            return Ok((false, None));
        }
        for (i, content) in scrub.changed {
            if let Some(m) = messages.get_mut(i) {
                js::set(m, "content", js::s(content));
            }
        }
        let warning = self
            .with(|s| {
                let fresh: Vec<String> = scrub
                    .removed
                    .iter()
                    .filter(|n| !s.in_history.contains(n))
                    .cloned()
                    .collect();
                if fresh.is_empty() {
                    return None;
                }
                s.in_history.extend(fresh);
                let note = history_note(&s.in_history);
                let parts = [self.image_warning.as_str(), note.as_str(), self.coverage_warning.as_str()];
                Some(
                    parts
                        .iter()
                        .filter(|p| !p.is_empty())
                        .copied()
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            })
            .flatten();
        Ok((true, warning))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn the_history_note_counts_its_skills() {
        assert_eq!(
            history_note(&["QA Skill".into()]),
            "Earlier replies in this chat used Skill \"QA Skill\", which is now disabled or changed. Its instructions were left out of this request."
        );
        assert_eq!(
            history_note(&["A".into(), "B\"".into()]),
            "Earlier replies in this chat used Skills \"A\", \"B\\\"\", which are now disabled or changed. Their instructions were left out of this request."
        );
    }
}
