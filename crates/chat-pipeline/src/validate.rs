//! What a turn refuses before anything is read or written (core chat.cjs `handleChatTurn` and the
//! first checks of `handleChatInner`): the Diary extras gates and their recovery preparation, a
//! missing message and a compaction outside an ordinary chat. All answer JSON `{error}`.
use crate::text::{as_str, get, set, spread, string};
use crate::Thrown;
use js_json::{wtf16, JValue};

/// core diary-extras.cjs `PROJECT_ID`.
pub const DIARY_EXTRAS_PROJECT_ID: &str = "cowork-diary-extras";

/// What the account and the workspace say about the Diary extras.
#[derive(Clone, Copy, Debug)]
pub struct Account {
    /// `authn` is present and `authService.diaryEnabled(user)`.
    pub diary_enabled: bool,
    /// `getProject(diaryExtras.PROJECT_ID)` is a project.
    pub extras_project: bool,
}

/// The outcome of the checks.
#[derive(Debug, Clone, PartialEq)]
pub enum Early {
    /// Answer `status` with `{error}` and stop.
    Refuse(u16, &'static str),
    /// A Diary extras recovery: start its preparation record (core diary-jobs.cjs `start`) before the turn.
    StartPreparation,
    /// Go on with this body (the Diary extras turn is rebound to its project and chat).
    Continue(JValue),
}

fn truthy(v: &JValue) -> bool {
    v.truthy()
}

/// `handleChatTurn`'s check of a Diary extras recovery (`spaceId: 'diary-extras'` with a `recoveryId`).
/// `None` when the body is not one.
pub fn recovery(body: &JValue, a: Account) -> Option<Early> {
    if as_str(get(body, "spaceId")) != Some("diary-extras") || !truthy(get(body, "recoveryId")) {
        return None;
    }
    if get(body, "extrasEnabled") != &JValue::Bool(true) || !a.diary_enabled || !a.extras_project {
        return Some(Early::Refuse(400, "Diary extras are not enabled."));
    }
    if !as_str(get(body, "message")).is_some_and(|m| !m.is_empty()) {
        return Some(Early::Refuse(400, "message required"));
    }
    Some(Early::StartPreparation)
}

/// `handleChatInner`'s first checks, in Node's order.
pub fn inner(body: &JValue, a: Account) -> Result<Early, Thrown> {
    if !matches!(body, JValue::Obj(_)) {
        // Node reads `body.compactOnly` (or `body.extrasEnabled`) of a non-object here.
        return Err(Thrown);
    }
    let space = get(body, "spaceId").clone();
    let message = get(body, "message").clone();
    let mut body = body.clone();
    if as_str(&space) == Some("diary-extras") {
        if get(&body, "extrasEnabled") != &JValue::Bool(true) {
            return Ok(Early::Refuse(400, "Extra attachments and tools are off"));
        }
        if !a.diary_enabled {
            return Ok(Early::Refuse(404, "Diary add-on is disabled"));
        }
        if !a.extras_project {
            return Ok(Early::Refuse(400, "Enable diary extras first"));
        }
        let session = get(&body, "sessionId");
        let session = if truthy(session) {
            string(session)?
        } else {
            String::new()
        };
        let mut next = spread(&body);
        set(
            &mut next,
            "projectId",
            JValue::Str(DIARY_EXTRAS_PROJECT_ID.into()),
        );
        set(
            &mut next,
            "chatId",
            JValue::Str(format!("diary-extra-{}", wtf16::slice(&session, 80))),
        );
        body = next;
    }
    let compact = truthy(get(&body, "compactOnly"));
    if (!truthy(&message) && !compact) || as_str(&message).is_none() {
        return Ok(Early::Refuse(400, "message required"));
    }
    if compact && (as_str(&space) == Some("diary") || !truthy(get(&body, "chatId"))) {
        return Ok(Early::Refuse(400, "Choose an ordinary chat to compact"));
    }
    Ok(Early::Continue(body))
}

/// The whole entry: the recovery check, then (when it does not answer) the inner checks.
pub fn check(body: &JValue, a: Account) -> Result<Early, Thrown> {
    match recovery(body, a) {
        Some(e) => Ok(e),
        None => inner(body, a),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_diary_extras_turn_is_rebound_to_its_project_and_a_chat_of_its_session() {
        let body = wtf16::parse(r#"{"spaceId":"diary-extras","extrasEnabled":true,"message":"hi","sessionId":"s-1","projectId":"other"}"#).unwrap();
        let a = Account {
            diary_enabled: true,
            extras_project: true,
        };
        let Early::Continue(next) = inner(&body, a).unwrap() else {
            panic!()
        };
        assert_eq!(
            wtf16::stringify(&next).unwrap(),
            r#"{"spaceId":"diary-extras","extrasEnabled":true,"message":"hi","sessionId":"s-1","projectId":"cowork-diary-extras","chatId":"diary-extra-s-1"}"#
        );
        let long = wtf16::parse(&format!(
            r#"{{"spaceId":"diary-extras","extrasEnabled":true,"message":"hi","sessionId":"{}"}}"#,
            "x".repeat(90)
        ))
        .unwrap();
        let Early::Continue(next) = inner(&long, a).unwrap() else {
            panic!()
        };
        assert_eq!(
            wtf16::len(next.get("chatId").as_str().unwrap()),
            "diary-extra-".len() + 80
        );
    }
}
