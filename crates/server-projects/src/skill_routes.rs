//! A project's instruction skills over HTTP (sbstndalton/noevia#2191 round 9, capability
//! `rust-project-config`), core routes/projects.cjs:
//!
//! - `GET  /api/projects/{id}/instruction-skills`            the Sources list ([`list`])
//! - `PUT  /api/projects/{id}/instruction-skills`            enable or disable one ([`select`])
//! - `GET  /api/projects/{id}/instruction-skills/manifests`  the portable manifests ([`manifests`])
//! - `GET  /api/projects/{id}/instruction-skills/manifests/{skillId}/content` ([`content`])
//! - `POST /api/projects/{id}/skills/install`                one published SKILL.md ([`install`])
//!
//! Every route starts with core's `getProject` (the self-heal and the reconcile, saved when they
//! change something, [`crate::originals::project`]); the project is the front's to write, and
//! chat reads it back through the project store like any other change. A removed account is 410,
//! a busy store 503 `PROJECTS_BUSY`.
//!
//! The install copies one text, fetched by the caller (a fixed host, public https, no redirect, an
//! 8 s deadline and a 32 KiB cap, bins/noevia-server plugin_directory_routes.rs); nothing in it
//! names a path: the project file's name is `<skill>/SKILL.md` for a validated skill name, and the
//! text lands in the account's own `project-sources` folder under the usual content-addressed name.
//! Scripts and assets are never fetched.

use crate::instruction_skills::{self as skills, View};
use crate::{chats, js, lifecycle, originals, source_limits, store, Ctx, Reply};
use js_json::{wtf16, JValue};
use std::path::Path;

/// http.cjs readJson's default limit.
pub const BODY_LIMIT: usize = 1024 * 1024;
/// The install's text cap (a project skill is at most 32 KiB).
pub const SKILL_CAP: usize = 32 * 1024;
/// The install's refusals before and after the fetch.
pub const UNKNOWN_SKILL: &str = "Unknown skill";
const SAVE_FAILED: &str = "could not save the project; nothing was changed";

fn json(status: u16, value: &JValue) -> Reply {
    Reply::json(status, wtf16::stringify(value).unwrap_or_default())
}

fn refusal(r: &skills::Refusal) -> Reply {
    Reply::error(r.status, &r.message)
}

/// `decodeURIComponent` of the project segment: a bad escape is a URIError, a 500.
fn decode(raw: &str) -> Result<String, Reply> {
    js::decode_uri_component(raw).ok_or_else(Reply::internal)
}

/// `getProject(id)` with `Err` for the answer to send (`missing` is the 404 text this route uses).
fn project_of(
    ctx: Ctx<'_>,
    id: &str,
    missing: &str,
) -> Result<(std::path::PathBuf, JValue), Reply> {
    match originals::project(ctx, id)? {
        (dir, Some(project)) => Ok((dir, project)),
        (_, None) => Err(Reply::error(404, missing)),
    }
}

/// GET /api/projects/{id}/instruction-skills.
pub fn list(ctx: Ctx<'_>, raw: &str) -> Reply {
    let id = match decode(raw) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let (dir, project) = match project_of(ctx, &id, "No such project") {
        Ok(v) => v,
        Err(r) => return r,
    };
    reply_list(&dir, &project)
}

fn reply_list(dir: &Path, project: &JValue) -> Reply {
    let view = View {
        project,
        dir,
        known: &[],
    };
    match skills::list_for_client(view) {
        Ok(rows) => json(200, &JValue::obj([("skills", rows)])),
        Err(_) => Reply::internal(),
    }
}

/// PUT /api/projects/{id}/instruction-skills. `body` was read up to [`BODY_LIMIT`].
pub fn select(ctx: Ctx<'_>, raw: &str, body: &[u8], over: bool) -> Reply {
    let id = match decode(raw) {
        Ok(id) => id,
        Err(r) => return r,
    };
    // getProject first (its heal is saved even when the body is refused), then the body.
    let (dir, _) = match project_of(ctx, &id, "No such project") {
        Ok(v) => v,
        Err(r) => return r,
    };
    if over {
        return Reply::error(413, "Request exceeds size limit");
    }
    let text = String::from_utf8_lossy(body);
    let parsed = if text.is_empty() {
        JValue::Obj(Vec::new())
    } else {
        match wtf16::parse(&text) {
            Ok(v) => v,
            Err(_) => return chats::invalid_json(),
        }
    };
    let done = store::update(&dir, ctx.switch, |projects| {
        let Ok(Some(at)) = chats::locate(projects, &id) else {
            return (Err(Reply::error(404, "No such project")), false);
        };
        let Some(p) = projects.get_mut(at) else {
            return (Err(Reply::internal()), false);
        };
        let healed = match chats::get_project_effects(p) {
            Ok(effects) => {
                let changed = effects.saved.is_some();
                *p = effects.memory;
                changed
            }
            Err(_) => return (Err(Reply::internal()), false),
        };
        if let Err(r) = skills::set_selection(p, &dir, &[], &parsed, ctx.now_ms) {
            return (Err(refusal(&r)), healed);
        }
        store::set(p, "updatedAt", JValue::Num(ctx.now_ms as f64));
        let project = p.clone();
        if !lifecycle::reconcile_all(projects) {
            return (Err(Reply::internal()), false);
        }
        (Ok(project), true)
    });
    match done {
        Ok(Ok(project)) => {
            // The list is of the project as saved (reconcile ran on the whole list).
            let saved = store::read(&dir)
                .ok()
                .and_then(|all| store::find(&all, &id).cloned())
                .unwrap_or(project);
            reply_list(&dir, &saved)
        }
        Ok(Err(reply)) => reply,
        Err(e) => chats::save_failure(&e, SAVE_FAILED),
    }
}

/// GET /api/projects/{id}/instruction-skills/manifests. `known`: every toolbox the server knows
/// (`None`: core has not published them, 503).
pub fn manifests(ctx: Ctx<'_>, raw: &str, known: Option<&[String]>) -> Reply {
    let id = match decode(raw) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let (dir, project) = match project_of(ctx, &id, "No such project") {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(known) = known else {
        return not_published();
    };
    let view = View {
        project: &project,
        dir: &dir,
        known,
    };
    match skills::manifests(view) {
        Ok(all) => json(
            200,
            &JValue::obj([
                ("schemaVersion", JValue::Num(1.0)),
                (
                    "skills",
                    JValue::Arr(all.iter().map(skills::Manifest::json).collect()),
                ),
            ]),
        ),
        Err(_) => Reply::internal(),
    }
}

fn not_published() -> Reply {
    Reply::error(
        503,
        "noevia-core has not published its toolbox catalogue yet; try again shortly.",
    )
}

/// GET /api/projects/{id}/instruction-skills/manifests/{skillId}/content?version=. `skill` is the
/// segment as the URL spelled it (core does not decode it); `version` the first `version` query.
pub fn content(
    ctx: Ctx<'_>,
    raw: &str,
    skill: &str,
    version: Option<&str>,
    known: Option<&[String]>,
) -> Reply {
    let id = match decode(raw) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let (dir, project) = match project_of(ctx, &id, "No such project") {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(known) = known else {
        return not_published();
    };
    let view = View {
        project: &project,
        dir: &dir,
        known,
    };
    match skills::resolve(view, skill, version) {
        Ok((manifest, content)) => json(
            200,
            &JValue::obj([
                ("manifest", manifest.json()),
                ("content", JValue::Str(content)),
            ]),
        ),
        Err(r) => refusal(&r),
    }
}

/// routes/projects.cjs's first step of an install: the project must exist (`no such project`, the
/// route's own lower-case spelling). Run before the body is read.
pub fn install_check(ctx: Ctx<'_>, raw: &str) -> Result<(), Reply> {
    let id = decode(raw)?;
    project_of(ctx, &id, "no such project").map(|_| ())
}

/// `String(name || '')` of the body's `skill`, which names the published skill and the file: `None`
/// where core's `fetchPublishedSkill` refuses it (`/^[a-z0-9][a-z0-9-]{0,79}$/`).
pub fn skill_name(body: &JValue) -> Option<String> {
    let v = body.get("skill");
    let text = if v.truthy() {
        crate::files::js_string(v)
    } else {
        String::new()
    };
    let mut chars = text.chars();
    let ok = text.len() <= 80
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    ok.then_some(text)
}

/// The URL the skill is fetched from (the fixed raw host, `name` validated by [`skill_name`]).
pub fn skill_path(name: &str) -> String {
    format!("anthropics/skills/main/skills/{name}/SKILL.md")
}

/// The published skill as a project file, and its entry in the answer. `retrieved_at` is
/// `new Date().toISOString()`.
pub fn install(ctx: Ctx<'_>, raw: &str, name: &str, content: &str, retrieved_at: &str) -> Reply {
    let id = match decode(raw) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let file_name = format!("{name}/SKILL.md");
    let digest = skills::hash(content);
    let file = JValue::obj([
        ("name", JValue::Str(file_name.clone())),
        ("content", JValue::Str(content.to_string())),
        (
            "skillOrigin",
            JValue::obj([
                ("kind", JValue::from("published")),
                ("publisher", JValue::from("Anthropic")),
                (
                    "repository",
                    JValue::from("https://github.com/anthropics/skills"),
                ),
                ("sourceRef", JValue::from("main")),
                ("sourcePath", JValue::Str(format!("skills/{name}/SKILL.md"))),
                ("digest", JValue::Str(digest)),
                ("retrievedAt", JValue::Str(retrieved_at.to_string())),
            ]),
        ),
    ]);
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(e) => return chats::store_failure(&e),
    };
    let done = store::update(&dir, ctx.switch, |projects| {
        let Ok(Some(at)) = chats::locate(projects, &id) else {
            return (Err(Reply::error(404, "no such project")), false);
        };
        let Some(p) = projects.get_mut(at) else {
            return (Err(Reply::internal()), false);
        };
        let healed = match chats::get_project_effects(p) {
            Ok(effects) => {
                let changed = effects.saved.is_some();
                *p = effects.memory;
                changed
            }
            Err(_) => return (Err(Reply::internal()), false),
        };
        // A skill the frontmatter check cannot use is refused with its reason.
        let view = View {
            project: p,
            dir: &dir,
            known: &[],
        };
        let inspected = match skills::inspect(view, &file) {
            Ok(v) => v,
            Err(_) => return (Err(Reply::internal()), healed),
        };
        match inspected {
            Some(s) if s.valid => {}
            other => {
                let why = other
                    .map(|s| s.error)
                    .filter(|e| !e.is_empty())
                    .unwrap_or_else(|| "no skill frontmatter".to_string());
                return (
                    Err(Reply::error(
                        422,
                        &format!("This skill cannot be used as a project skill: {why}"),
                    )),
                    healed,
                );
            }
        }
        let files: Vec<JValue> = match p.get("files") {
            JValue::Arr(items) => items.clone(),
            _ => Vec::new(),
        };
        let listed = files
            .iter()
            .any(|f| f.get("name").as_str() == Some(file_name.as_str()));
        if !listed {
            if let Some(full) = source_limits::refusal(&files, &file_name, None) {
                let mut text = String::from("{\"error\":");
                js_json::quote(&full.message, &mut text);
                text.push_str(",\"code\":");
                js_json::quote(full.code, &mut text);
                text.push('}');
                return (Err(Reply::json(409, text)), healed);
            }
        }
        let mut next: Vec<JValue> = files
            .into_iter()
            .filter(|f| f.get("name").as_str() != Some(file_name.as_str()))
            .collect();
        next.push(file.clone());
        store::set(p, "files", JValue::Arr(next));
        // A replaced skill needs review again.
        if let JValue::Obj(fields) = p {
            if let Some((_, JValue::Obj(selection))) =
                fields.iter_mut().find(|(k, _)| k == "instructionSkills")
            {
                selection.retain(|(k, _)| k != &file_name);
            }
        }
        // The text is on disk before the projects.json that points at it.
        if let Err(r) = lifecycle::externalize_reply(&dir, p) {
            return (Err(r), healed);
        }
        store::set(p, "updatedAt", JValue::Num(ctx.now_ms as f64));
        if !lifecycle::reconcile_all(projects) {
            return (Err(Reply::internal()), false);
        }
        let saved = store::find(projects, &id).cloned().unwrap_or(JValue::Null);
        (Ok(saved), true)
    });
    match done {
        Ok(Ok(project)) => {
            let view = View {
                project: &project,
                dir: &dir,
                known: &[],
            };
            match skills::list_without_content(view) {
                Ok(rows) => json(
                    201,
                    &JValue::obj([("file", JValue::Str(file_name)), ("skills", rows)]),
                ),
                Err(_) => Reply::internal(),
            }
        }
        Ok(Err(reply)) => reply,
        Err(e) => chats::save_failure(&e, SAVE_FAILED),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    const USER: &str = "01234567-89ab-4cde-8fab-0123456789ab";
    const GOOD: &str = "---\nname: Fixture\ndescription: A synthetic skill\n---\nBody";

    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new(projects: &str) -> Fixture {
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join("users").join(USER);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("projects.json"), projects).unwrap();
            Fixture { root }
        }

        fn ctx(&self) -> Ctx<'_> {
            Ctx {
                data_dir: self.root.path(),
                user_id: USER,
                switch: server_store::RustProjects::owned(),
                now_ms: 1_700_000_000_000,
            }
        }

        fn stored(&self) -> JValue {
            let text = std::fs::read_to_string(
                self.root
                    .path()
                    .join("users")
                    .join(USER)
                    .join("projects.json"),
            )
            .unwrap();
            wtf16::parse(&text).unwrap()
        }
    }

    fn body(r: &Reply) -> JValue {
        match &r.body {
            crate::Body::Json(text) => wtf16::parse(text).unwrap(),
            crate::Body::Bytes { .. } => panic!("bytes"),
        }
    }

    fn one(files: &str) -> String {
        format!(
            r#"{{"projects":[{{"id":"proj-1","name":"P","files":{files},"instructionSkills":{{}},"toolboxes":["core"]}}]}}"#
        )
    }

    fn skill_file() -> String {
        let mut quoted = String::new();
        js_json::quote(GOOD, &mut quoted);
        format!(r#"[{{"name":"fix/SKILL.md","content":{quoted}}}]"#)
    }

    #[test]
    fn the_list_names_the_skill_in_review_and_a_missing_project_is_core_s_404() {
        let f = Fixture::new(&one(&skill_file()));
        let r = list(f.ctx(), "proj-1");
        assert_eq!(r.status, 200);
        let b = body(&r);
        let JValue::Arr(rows) = b.get("skills") else {
            panic!("rows")
        };
        assert_eq!(rows[0].get("status").as_str(), Some("review"));
        assert_eq!(rows[0].get("file").as_str(), Some("fix/SKILL.md"));
        assert!(rows[0].get("id").as_str().unwrap().starts_with("skill_"));
        assert_eq!(
            rows[0].get("origin").get("kind").as_str(),
            Some("project-file")
        );
        let missing = list(f.ctx(), "nope");
        assert_eq!(
            (
                missing.status,
                body(&missing).get("error").as_str().map(str::to_owned)
            ),
            (404, Some("No such project".to_string()))
        );
        assert_eq!(
            list(f.ctx(), "%E0%A4%A").status,
            500,
            "a bad escape is core's URIError"
        );
    }

    #[test]
    fn put_enables_the_reviewed_hash_and_the_manifest_then_resolves() {
        let f = Fixture::new(&one(&skill_file()));
        let hash = skills::hash(GOOD);
        let ask = |json: &str| select(f.ctx(), "proj-1", json.as_bytes(), false);
        assert_eq!(ask("not json").status, 400);
        assert_eq!(
            ask(r#"{"file":"fix/SKILL.md","enabled":true,"hash":"old"}"#).status,
            409
        );
        let ok = ask(&format!(
            r#"{{"file":"fix/SKILL.md","enabled":true,"hash":"{hash}"}}"#
        ));
        assert_eq!(ok.status, 200);
        assert_eq!(
            body(&ok).get("skills"),
            body(&list(f.ctx(), "proj-1")).get("skills")
        );
        let stored = f.stored();
        let p = &match stored.get("projects") {
            JValue::Arr(a) => a[0].clone(),
            _ => panic!(),
        };
        assert_eq!(
            p.get("instructionSkills")
                .get("fix/SKILL.md")
                .get("enabled"),
            &JValue::Bool(true)
        );
        assert_eq!(p.get("updatedAt"), &JValue::Num(1_700_000_000_000.0));
        // The manifests need core's published toolbox list.
        assert_eq!(manifests(f.ctx(), "proj-1", None).status, 503);
        let known = vec!["core".to_string()];
        let m = manifests(f.ctx(), "proj-1", Some(&known));
        assert_eq!(m.status, 200);
        let mb = body(&m);
        let JValue::Arr(all) = mb.get("skills") else {
            panic!()
        };
        assert_eq!(all[0].get("resolvable"), &JValue::Bool(true));
        let id = all[0].get("id").as_str().unwrap().to_string();
        let c = content(f.ctx(), "proj-1", &id, Some(&hash), Some(&known));
        assert_eq!(c.status, 200);
        assert_eq!(body(&c).get("content").as_str(), Some(GOOD));
        assert_eq!(
            content(f.ctx(), "proj-1", &id, None, Some(&known)).status,
            400
        );
        // A request over the cap is 413, after the project was looked up.
        assert_eq!(select(f.ctx(), "proj-1", b"{}", true).status, 413);
        assert_eq!(select(f.ctx(), "nope", b"{}", false).status, 404);
    }

    #[test]
    fn install_replaces_a_skill_and_needs_review_again() {
        let f = Fixture::new(&one("[]"));
        assert!(install_check(f.ctx(), "proj-1").is_ok());
        assert_eq!(install_check(f.ctx(), "nope").unwrap_err().status, 404);
        let r = install(f.ctx(), "proj-1", "fix", GOOD, "2026-10-30T00:00:00.000Z");
        assert_eq!(r.status, 201, "{:?}", r.body);
        let b = body(&r);
        assert_eq!(b.get("file").as_str(), Some("fix/SKILL.md"));
        let JValue::Arr(rows) = b.get("skills") else {
            panic!()
        };
        assert_eq!(rows[0].get("status").as_str(), Some("review"));
        assert!(matches!(rows[0].get("content"), JValue::Undefined));
        // The text is in its own file; the project file keeps the origin.
        let stored = f.stored();
        let JValue::Arr(projects) = stored.get("projects") else {
            panic!()
        };
        let file = match projects[0].get("files") {
            JValue::Arr(files) => files[0].clone(),
            _ => panic!(),
        };
        assert!(file.get("textRef").as_str().is_some());
        assert!(matches!(file.get("content"), JValue::Undefined));
        assert_eq!(
            file.get("skillOrigin").get("digest").as_str(),
            Some(skills::hash(GOOD).as_str())
        );
        assert_eq!(file.get("skillHeader"), &JValue::Bool(true));
        // Enable it, then install again: the review is gone.
        let hash = skills::hash(GOOD);
        let on = select(
            f.ctx(),
            "proj-1",
            format!(r#"{{"file":"fix/SKILL.md","enabled":true,"hash":"{hash}"}}"#).as_bytes(),
            false,
        );
        assert_eq!(on.status, 200);
        assert_eq!(
            body(&on).get("skills"),
            body(&list(f.ctx(), "proj-1")).get("skills")
        );
        let again = install(f.ctx(), "proj-1", "fix", GOOD, "2026-10-31T00:00:00.000Z");
        assert_eq!(again.status, 201);
        let JValue::Arr(rows) = body(&again).get("skills").clone() else {
            panic!()
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get("status").as_str(),
            Some("review"),
            "a replaced skill needs review again"
        );
    }

    #[test]
    fn install_refuses_what_is_not_a_usable_skill_and_a_full_project() {
        let f = Fixture::new(&one("[]"));
        let bad = install(f.ctx(), "proj-1", "plain", "no front matter", "x");
        assert_eq!(bad.status, 422);
        assert_eq!(
            body(&bad).get("error").as_str(),
            Some("This skill cannot be used as a project skill: Use a closed frontmatter block of at most 2 KiB.")
        );
        assert_eq!(
            f.stored(),
            wtf16::parse(&one("[]")).unwrap(),
            "nothing was saved"
        );
        // The default cap is 1,000 sources.
        let many: Vec<String> = (0..1000)
            .map(|i| format!(r#"{{"name":"f{i}.md","content":"x"}}"#))
            .collect();
        let full = Fixture::new(&one(&format!("[{}]", many.join(","))));
        let r = install(full.ctx(), "proj-1", "fix", GOOD, "x");
        assert_eq!(r.status, 409);
        assert_eq!(body(&r).get("code").as_str(), Some("projectLimit"));
    }

    #[test]
    fn the_skill_name_is_core_s_rule() {
        let name = |v: &str| skill_name(&wtf16::parse(v).unwrap());
        assert_eq!(
            name(r#"{"skill":"frontend-design"}"#).as_deref(),
            Some("frontend-design")
        );
        assert_eq!(
            name(r#"{"skill":["frontend-design"]}"#).as_deref(),
            Some("frontend-design")
        );
        assert_eq!(name(r#"{"skill":5}"#).as_deref(), Some("5"));
        for refused in [
            r#"{"skill":"../etc"}"#,
            r#"{"skill":"a/b"}"#,
            r#"{"skill":""}"#,
            r#"{"skill":"X"}"#,
            r#"{"skill":"a b"}"#,
            r#"{"skill":"-a"}"#,
            r#"{"skill":{}}"#,
            r#"{}"#,
            r#"null"#,
        ] {
            assert_eq!(name(refused), None, "{refused}");
        }
        assert_eq!(name(&format!(r#"{{"skill":"{}"}}"#, "a".repeat(81))), None);
        assert!(name(&format!(r#"{{"skill":"{}"}}"#, "a".repeat(80))).is_some());
    }
}
