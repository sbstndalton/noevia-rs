//! What a project turn adds to the system message before its skills (chat.cjs ~617-647 at the
//! pinned core), in chat.cjs's order:
//!
//! 1. `You are working inside the user's project <quoteName(name)>.` (not for a free chat's
//!    attachments shadow project, `cowork-chat-context-*`, #1441);
//! 2. `Project goal: ...`; 3. `Project instructions (follow closely):\n...`;
//! 4. the memory part, with the project's memories ([`crate::memory::part`]);
//! 5. the shared Code tasks part ([`crate::shared::for_chat`]) when the project shares into Chat;
//! 6. `Relevant knowledge-file excerpts for this message:\n<filesBlock>` (a shadow project's block
//!    goes in front of the user's message instead, [`attach_files_block`], except on `compactOnly`);
//! 7. the not-ready attachments line ([`crate::sources::not_ready`], never on `compactOnly`);
//! 8. the waiting documents line ([`crate::sources::waiting_notice`]);
//! 9. the coverage note ([`crate::sources::coverage_note`]).
//!
//! After these come the project's skills (index, auto-loaded, pinned: [`ProjectTurn::skills`]), then
//! the stored-only line ([`crate::sources::stored_only`], which a turn without a project never has),
//! then the frame and brain blocks ([`crate::Turn::trailing`]).

use crate::sources::{self, string_of, NotReady, Partial};
use crate::{js, names, shared, PartKind, Thrown};
use js_json::JValue;

/// The id prefix of a free chat's attachments shadow project (#1441).
pub const CHAT_CONTEXT_PREFIX: &str = "cowork-chat-context-";

/// What a project turn is prepared from, beyond [`crate::Turn`].
///
/// Preconditions (the rest is a later slice): the project has no image assets (`project.assets`:
/// the vision notes come after routing), no instruction skill reaches the turn except through
/// [`ProjectTurn::skills`] and the history holds no revoked skill text (the skill-history scrub),
/// and no Diary space.
pub struct ProjectTurn<'a> {
    /// `getProject(projectId)` after `instruction-skills.cjs` `snapshot`: the stored record, each
    /// file's `content` the text Node's accessor reads.
    pub project: &'a JValue,
    /// `codeTasksFor(project) || []` (`[]` when it throws); read only when the project shares
    /// into Chat.
    pub code_tasks: &'a [JValue],
    /// `rag.filesContext(...)`'s result (track R4); `None` or empty: no block.
    pub files_block: Option<&'a str>,
    /// `sourceJobs.settle(...).notReady`; ignored on `compactOnly`, where Node does not wait.
    pub not_ready: &'a [NotReady],
    /// `partialSources(readable, injected)` of what retrieval placed (#1513).
    pub partial_reads: &'a [Partial],
    /// The skill index, the auto-loaded skill block and the pinned skill block, in that order (a
    /// later slice computes them; empty for none).
    pub skills: &'a [(PartKind, String)],
}

/// chat.cjs asks retrieval only for `Array.isArray(project.files) && project.files.length`; without
/// files `filesBlock` stays null and `partialReads` empty, whatever a caller passes (P2a review).
pub fn retrieval_ran(project: &JValue) -> bool {
    matches!(project.get("files"), JValue::Arr(f) if !f.is_empty())
}

/// `String(project.id).startsWith('cowork-chat-context-')`.
pub fn is_chat_attachments(project: &JValue) -> Result<bool, Thrown> {
    Ok(string_of(project.get("id"))?.starts_with(CHAT_CONTEXT_PREFIX))
}

/// The parts of [`ProjectTurn`] from the project line through the coverage note (1-9 above), with
/// `memory` (the memory part, already holding the project's memories) in its place. `compact`:
/// the turn is `compactOnly`.
pub fn parts(
    turn: &ProjectTurn<'_>,
    memory: Option<String>,
    compact: bool,
) -> Result<Vec<(PartKind, String)>, Thrown> {
    let project = turn.project;
    let attachments = is_chat_attachments(project)?;
    let mut out = Vec::new();
    let name = project.get("name");
    if name.truthy() && !attachments {
        out.push((
            PartKind::ProjectName,
            format!(
                "You are working inside the user's project {}.",
                names::quote_name(&string_of(name)?)
            ),
        ));
    }
    let goal = project.get("goal");
    if goal.truthy() {
        out.push((
            PartKind::ProjectGoal,
            format!("Project goal: {}", string_of(goal)?),
        ));
    }
    let instructions = project.get("instructions");
    if instructions.truthy() {
        out.push((
            PartKind::ProjectInstructions,
            format!(
                "Project instructions (follow closely):\n{}",
                string_of(instructions)?
            ),
        ));
    }
    if let Some(m) = memory {
        out.push((PartKind::Memory, m));
    }
    if shared::read(project).0 {
        if let Some(p) = shared::for_chat(project, turn.code_tasks)? {
            out.push((PartKind::SharedCode, p));
        }
    }
    let ran = retrieval_ran(project);
    if let Some(block) = turn.files_block.filter(|b| ran && !b.is_empty()) {
        if !attachments || compact {
            out.push((
                PartKind::Knowledge,
                format!("Relevant knowledge-file excerpts for this message:\n{block}"),
            ));
        }
    }
    if !compact {
        if let Some(p) = sources::not_ready(turn.not_ready) {
            out.push((PartKind::NotReady, p));
        }
    }
    if let Some(p) = sources::waiting_notice(project.get("files"))? {
        out.push((PartKind::Waiting, p));
    }
    if let Some(p) = sources::coverage_note(if ran { turn.partial_reads } else { &[] }) {
        out.push((PartKind::Coverage, p));
    }
    Ok(out)
}

/// A shadow project's knowledge block, put right before the newest user message (which the
/// replay placed last): when the last message is the user's with string content, the block and a
/// blank line go in front of its trailing `message`, so after a leading applied-write note the
/// replay folded in (in front of the whole content when it does not end with `message`, which the
/// replay never makes). Nothing else changes.
pub fn attach_files_block(history: &mut [JValue], block: &str, message: &str) {
    let Some(JValue::Obj(items)) = history.last_mut() else {
        return;
    };
    let is_user = items
        .iter()
        .any(|(k, v)| k == "role" && v.as_str() == Some("user"));
    let Some((_, JValue::Str(content))) = items.iter_mut().find(|(k, _)| k == "content") else {
        return;
    };
    if !is_user {
        return;
    }
    // `last.content.endsWith(message)` and the cut, over UTF-16 units as in JS.
    let units = js::to_units(content);
    let tail = js::to_units(message);
    let at = if units.ends_with(&tail) {
        units.len() - tail.len()
    } else {
        0
    };
    let (head, rest) = units.split_at(at);
    *content = format!(
        "{}{block}\n\n{}",
        js::from_units(head),
        js::from_units(rest)
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn a_block_for_a_project_without_files_is_not_used() {
        for files in ["[]", "null", "{}"] {
            let project = js_json::wtf16::parse(&format!(
                r#"{{"id":"qa-project","name":"QA","files":{files}}}"#
            ))
            .unwrap();
            let partial = [Partial {
                name: "qa-a.txt".into(),
                length: 2,
                read: 1,
            }];
            let turn = ProjectTurn {
                project: &project,
                code_tasks: &[],
                files_block: Some("QA block"),
                not_ready: &[],
                partial_reads: &partial,
                skills: &[],
            };
            let out = parts(&turn, None, false).unwrap();
            assert!(
                out.iter()
                    .all(|(k, _)| !matches!(k, PartKind::Knowledge | PartKind::Coverage)),
                "{files}: {out:?}"
            );
        }
    }

    fn wire(entries: &[(&str, &str)]) -> Vec<JValue> {
        entries
            .iter()
            .map(|(r, c)| js::message(r, (*c).to_string()))
            .collect()
    }

    #[test]
    fn the_block_goes_before_the_trailing_message_or_the_whole_content() {
        let mut h = wire(&[("user", "QA note\n\nQA question")]);
        attach_files_block(&mut h, "QA block", "QA question");
        assert_eq!(
            h[0].get("content").as_str(),
            Some("QA note\n\nQA block\n\nQA question")
        );
        // Content that does not end with the message (the replay never makes one): in front.
        let mut h = wire(&[("user", "QA other")]);
        attach_files_block(&mut h, "QA block", "QA question");
        assert_eq!(h[0].get("content").as_str(), Some("QA block\n\nQA other"));
    }

    #[test]
    fn nothing_changes_without_a_last_user_message() {
        let mut empty: Vec<JValue> = Vec::new();
        attach_files_block(&mut empty, "QA block", "QA");
        assert!(empty.is_empty());
        let mut h = wire(&[("user", "QA"), ("assistant", "QA")]);
        let before = h.clone();
        attach_files_block(&mut h, "QA block", "QA");
        assert_eq!(h, before);
    }
}
