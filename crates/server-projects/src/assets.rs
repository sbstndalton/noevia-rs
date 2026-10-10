//! A project's images: core routes/projects.cjs `POST /api/projects/{id}/assets` and
//! `GET`/`DELETE /api/projects/{id}/assets/{assetId}`, answer for answer, including what Node does
//! with unusual JSON (a `null` body or a non-array `assets` throws a TypeError there: a 500 here).
//!
//! Stricter than Node, never looser: the image count and the project's existence are checked
//! again under the projects.json lock (Node's single thread made that unnecessary), and the image
//! file is removed again when that check fails; a stored `mime` that is not a string is a 500
//! (Node would send `String(mime)`).

use crate::js;
use crate::store::{self, StoreError};
use crate::{Body, Ctx, Reply};
use js_json::JValue;
use std::path::Path;

/// IMAGE_MIME.
pub const IMAGE_MIME: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];
/// IMAGE_UPLOAD_CAP.
pub const IMAGE_UPLOAD_CAP: usize = 8 * 1024 * 1024;
/// MAX_PROJECT_IMAGES.
pub const MAX_PROJECT_IMAGES: usize = 12;
/// `Math.ceil(IMAGE_UPLOAD_CAP / 3) * 4 + 512 * 1024`: the body limit of the upload.
pub const BODY_LIMIT: usize = IMAGE_UPLOAD_CAP.div_ceil(3) * 4 + 512 * 1024;

fn store_failure(e: &StoreError) -> Reply {
    match e {
        StoreError::Busy => Reply::error(503, "The project store is busy. Try again shortly."),
        _ => Reply::internal(),
    }
}

/// `String(v || '')`; `None` where JS throws.
fn string_or_empty(v: &JValue) -> Option<String> {
    if v.truthy() {
        js_json::to_js_string(v).ok()
    } else {
        Some(String::new())
    }
}

/// `(list || [])` followed by `.find((a) => a.id === id)`: `Err` where JS throws (a truthy
/// non-array has no `find`; a `null`/`undefined` element before the match has no `id`).
fn find_in(list: &JValue, id: &str) -> Result<Option<JValue>, ()> {
    let items = match list {
        JValue::Arr(items) => items,
        v if !v.truthy() => return Ok(None),
        _ => return Err(()),
    };
    for a in items {
        if a.is_nullish() {
            return Err(());
        }
        if a.get("id").as_str() == Some(id) {
            return Ok(Some(a.clone()));
        }
    }
    Ok(None)
}

/// The project id and the image id of a request path; `None` where `decodeURIComponent` throws
/// (Node: a URIError, so a 500).
fn ids(raw_project: &str, raw_asset: &str) -> Option<(String, String)> {
    let id = js::decode_uri_component(raw_project)?;
    let asset = js::id_chars(&js::decode_uri_component(raw_asset)?);
    Some((id, asset))
}

fn write_asset_file(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write as _;
    let mut f = options.open(dir.join(name))?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// POST /api/projects/{id}/assets. `raw_project` is the path segment as sent; `body` the request
/// body read up to [`BODY_LIMIT`], `over` when it was longer.
pub fn post(ctx: Ctx<'_>, raw_project: &str, body: &[u8], over: bool) -> Reply {
    let Some(id) = js::decode_uri_component(raw_project) else {
        return Reply::internal();
    };
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(e) => return store_failure(&e),
    };
    let projects = match store::read(&dir) {
        Ok(p) => p,
        Err(e) => return store_failure(&e),
    };
    let Some(project) = store::find(&projects, &id) else {
        return Reply::error(404, "no such project");
    };
    if over {
        return Reply::error(413, "That image is larger than the 8 MB limit.");
    }
    let Ok(body) = js_json::parse(&String::from_utf8_lossy(body)) else {
        return Reply::error(400, "invalid JSON");
    };
    if matches!(body, JValue::Null) {
        return Reply::internal(); // body.name on null
    }
    let Some(name) = string_or_empty(body.get("name")) else {
        return Reply::internal();
    };
    let name = js_json::js_slice(&name, 200);
    let Some(mime) = string_or_empty(body.get("mime")) else {
        return Reply::internal();
    };
    let mime = mime.to_lowercase();
    if !IMAGE_MIME.contains(&mime.as_str()) {
        let shown = if mime.is_empty() {
            "that file"
        } else {
            mime.as_str()
        };
        return Reply::error(
            400,
            &format!("{shown} is not a supported image (png, jpeg, webp or gif)."),
        );
    }
    let Some(data) = string_or_empty(body.get("dataBase64")) else {
        return Reply::internal();
    };
    let bytes = js::base64_loose(&data);
    if bytes.is_empty() {
        return Reply::error(400, "image data was empty");
    }
    if bytes.len() > IMAGE_UPLOAD_CAP {
        // Math.round(bytes.length / 1024)
        let kb = (bytes.len() as f64 / 1024.0 + 0.5).floor();
        return Reply::error(
            413,
            &format!(
                "That image is {} KB, over the 8 MB limit.",
                js_json::number_to_string(kb)
            ),
        );
    }
    let too_many = || {
        Reply::error(
            400,
            &format!("A project holds at most {MAX_PROJECT_IMAGES} images."),
        )
    };
    let count = |p: &JValue| match p.get("assets") {
        JValue::Arr(a) => a.len(),
        _ => 0,
    };
    if count(project) >= MAX_PROJECT_IMAGES {
        return too_many();
    }
    let asset_id = format!(
        "img-{}-{}",
        js::base36(u64::try_from(ctx.now_ms).unwrap_or(0)),
        js::random36(6)
    );
    let assets_dir = store::asset_dir(&dir, &id);
    if write_asset_file(&assets_dir, &asset_id, &bytes).is_err() {
        return Reply::internal();
    }
    let entry = JValue::obj([
        ("id", JValue::Str(asset_id.clone())),
        ("name", JValue::Str(name)),
        ("mime", JValue::Str(mime)),
        ("bytes", JValue::Num(bytes.len() as f64)),
    ]);
    let saved = store::update(&dir, ctx.switch, |projects| {
        let Some(project) = store::find_mut(projects, &id) else {
            return (Err(Reply::error(404, "no such project")), false);
        };
        let mut assets = match project.get("assets") {
            JValue::Arr(a) => a.clone(),
            _ => Vec::new(),
        };
        if assets.len() >= MAX_PROJECT_IMAGES {
            return (Err(too_many()), false);
        }
        assets.push(entry.clone());
        store::set(project, "assets", JValue::Arr(assets));
        (Ok(()), true)
    });
    let failed = match saved {
        Ok(Ok(())) => None,
        Ok(Err(reply)) => Some(reply),
        Err(e) => Some(store_failure(&e)),
    };
    if let Some(reply) = failed {
        let _ = std::fs::remove_file(assets_dir.join(&asset_id));
        return reply;
    }
    let out = JValue::obj([("asset", entry)]);
    Reply::json(200, js_json::stringify(&out).unwrap_or_default())
}

/// GET /api/projects/{id}/assets/{assetId}: the image bytes, or Node's 404s.
pub fn get(ctx: Ctx<'_>, raw_project: &str, raw_asset: &str) -> Reply {
    let Some((id, asset_id)) = ids(raw_project, raw_asset) else {
        return Reply::internal();
    };
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(e) => return store_failure(&e),
    };
    let projects = match store::read(&dir) {
        Ok(p) => p,
        Err(e) => return store_failure(&e),
    };
    let Some(project) = store::find(&projects, &id) else {
        return Reply::error(404, "no such project");
    };
    // A retired image (replaced, but still named by a chat transcript) stays readable (#218).
    let asset = match find_in(project.get("assets"), &asset_id) {
        Ok(Some(a)) => Some(a),
        Ok(None) => match find_in(project.get("retiredAssets"), &asset_id) {
            Ok(a) => a,
            Err(()) => return Reply::internal(),
        },
        Err(()) => return Reply::internal(),
    };
    let Some(asset) = asset else {
        return Reply::error(404, "no such image");
    };
    let Ok(bytes) = std::fs::read(store::asset_dir(&dir, &id).join(&asset_id)) else {
        return Reply::error(404, "image data is missing");
    };
    let Some(mime) = asset.get("mime").as_str().map(str::to_string) else {
        return Reply::internal();
    };
    Reply {
        status: 200,
        body: Body::Bytes {
            headers: vec![
                ("content-type", mime),
                ("content-length", bytes.len().to_string()),
                ("cache-control", "private, max-age=86400".into()),
                (
                    "content-security-policy",
                    "default-src 'none'; sandbox".into(),
                ),
                ("x-content-type-options", "nosniff".into()),
            ],
            bytes,
        },
    }
}

/// DELETE /api/projects/{id}/assets/{assetId}.
pub fn delete(ctx: Ctx<'_>, raw_project: &str, raw_asset: &str) -> Reply {
    let Some((id, asset_id)) = ids(raw_project, raw_asset) else {
        return Reply::internal();
    };
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(e) => return store_failure(&e),
    };
    let projects = match store::read(&dir) {
        Ok(p) => p,
        Err(e) => return store_failure(&e),
    };
    let Some(project) = store::find(&projects, &id) else {
        return Reply::error(404, "no such project");
    };
    match find_in(project.get("assets"), &asset_id) {
        Ok(Some(_)) => {}
        Ok(None) => return Reply::error(404, "no such image"),
        Err(()) => return Reply::internal(),
    }
    let saved = store::update(&dir, ctx.switch, |projects| {
        let Some(project) = store::find_mut(projects, &id) else {
            return (Err(Reply::error(404, "no such project")), false);
        };
        // (project.assets || []).filter((a) => a.id !== assetId)
        let kept = match project.get("assets") {
            JValue::Arr(items) => {
                if items.iter().any(JValue::is_nullish) {
                    return (Err(Reply::internal()), false);
                }
                items
                    .iter()
                    .filter(|a| a.get("id").as_str() != Some(asset_id.as_str()))
                    .cloned()
                    .collect()
            }
            v if !v.truthy() => Vec::new(),
            _ => return (Err(Reply::internal()), false),
        };
        store::set(project, "assets", JValue::Arr(kept));
        (Ok(()), true)
    });
    match saved {
        Ok(Ok(())) => {}
        Ok(Err(reply)) => return reply,
        Err(e) => return store_failure(&e),
    }
    let _ = std::fs::remove_file(store::asset_dir(&dir, &id).join(&asset_id)); // already gone: fine
    Reply::json(200, "{\"ok\":true}".into())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;
    use server_store::RustProjects;

    const USER: &str = "11111111-2222-4333-8444-555555555555";
    const OTHER: &str = "99999999-2222-4333-8444-555555555555";
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";

    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new(projects: &str) -> Self {
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join("users").join(USER);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("projects.json"), projects).unwrap();
            Fixture { root }
        }
        fn ctx(&self) -> Ctx<'_> {
            self.ctx_as(USER)
        }
        fn ctx_as<'a>(&'a self, user: &'a str) -> Ctx<'a> {
            Ctx {
                data_dir: self.root.path(),
                user_id: user,
                switch: RustProjects::from_env_value(Some("1")).unwrap(),
                now_ms: 1_791_600_933_849,
            }
        }
        fn user(&self) -> std::path::PathBuf {
            self.root.path().join("users").join(USER)
        }
        fn projects(&self) -> serde_json::Value {
            serde_json::from_str(
                &std::fs::read_to_string(self.user().join("projects.json")).unwrap(),
            )
            .unwrap()
        }
    }

    fn json(r: &Reply) -> serde_json::Value {
        match &r.body {
            Body::Json(t) => serde_json::from_str(t).unwrap(),
            Body::Bytes { .. } => panic!("not json"),
        }
    }

    fn post_json(f: &Fixture, project: &str, body: &str) -> Reply {
        post(f.ctx(), project, body.as_bytes(), false)
    }

    const ONE: &str = "{\n  \"projects\": [\n    {\n      \"id\": \"proj-1\",\n      \"name\": \"Synthetic\",\n      \"chats\": []\n    }\n  ]\n}";

    #[test]
    fn upload_read_delete_round_trip() {
        let f = Fixture::new(ONE);
        let body =
            format!("{{\"name\":\"pixel.png\",\"mime\":\"IMAGE/PNG\",\"dataBase64\":\"{PNG}\"}}");
        let r = post_json(&f, "proj-1", &body);
        assert_eq!(r.status, 200, "{r:?}");
        let asset = json(&r)["asset"].clone();
        let id = asset["id"].as_str().unwrap().to_string();
        assert!(
            id.starts_with("img-mv1sxryx-") && id.len() == "img-mv1sxryx-".len() + 6,
            "{id}"
        );
        assert_eq!(asset["mime"], "image/png");
        assert_eq!(asset["bytes"], 70);
        // Key order of the reply is Node's.
        let Body::Json(text) = &r.body else { panic!() };
        assert!(text.starts_with("{\"asset\":{\"id\":"), "{text}");
        assert!(
            text.ends_with(",\"name\":\"pixel.png\",\"mime\":\"image/png\",\"bytes\":70}}"),
            "{text}"
        );
        let file = f.user().join("project-assets/proj-1").join(&id);
        assert_eq!(std::fs::read(&file).unwrap().len(), 70);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(f.user().join("project-assets"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        assert_eq!(f.projects()["projects"][0]["assets"][0]["id"], id.as_str());
        // Node's file: the new key appended, everything else as it was.
        let text = std::fs::read_to_string(f.user().join("projects.json")).unwrap();
        assert!(text.starts_with("{\n  \"projects\": [\n    {\n      \"id\": \"proj-1\",\n      \"name\": \"Synthetic\",\n      \"chats\": [],\n      \"assets\": [\n        {\n          \"id\": \"img-"), "{text}");

        let g = get(f.ctx(), "proj-1", &id);
        assert_eq!(g.status, 200);
        let Body::Bytes { headers, bytes } = &g.body else {
            panic!()
        };
        assert_eq!(bytes.len(), 70);
        assert!(headers.contains(&("content-type", "image/png".into())));
        assert!(headers.contains(&(
            "content-security-policy",
            "default-src 'none'; sandbox".into()
        )));

        let d = delete(f.ctx(), "proj-1", &id);
        assert_eq!(
            (d.status, d.body.clone()),
            (200, Body::Json("{\"ok\":true}".into()))
        );
        assert!(!file.exists());
        assert_eq!(f.projects()["projects"][0]["assets"], serde_json::json!([]));
        assert_eq!(
            get(f.ctx(), "proj-1", &id),
            Reply::error(404, "no such image")
        );
        assert_eq!(
            delete(f.ctx(), "proj-1", &id),
            Reply::error(404, "no such image")
        );
    }

    #[test]
    fn nodes_refusals() {
        let f = Fixture::new(ONE);
        let png =
            |extra: &str| format!("{{\"mime\":\"image/png\",\"dataBase64\":\"{PNG}\"{extra}}}");
        assert_eq!(
            post_json(&f, "nope", &png("")),
            Reply::error(404, "no such project")
        );
        assert_eq!(
            post(f.ctx(), "proj-1", b"{", false),
            Reply::error(400, "invalid JSON")
        );
        assert_eq!(
            post(f.ctx(), "proj-1", b"", false),
            Reply::error(400, "invalid JSON")
        );
        assert_eq!(
            post(f.ctx(), "proj-1", b"x", true),
            Reply::error(413, "That image is larger than the 8 MB limit.")
        );
        assert_eq!(post(f.ctx(), "proj-1", b"null", false), Reply::internal());
        assert_eq!(
            post(f.ctx(), "proj-1", b"[]", false),
            Reply::error(
                400,
                "that file is not a supported image (png, jpeg, webp or gif)."
            )
        );
        assert_eq!(
            post_json(
                &f,
                "proj-1",
                "{\"mime\":\"Text/Plain\",\"dataBase64\":\"QUJD\"}"
            ),
            Reply::error(
                400,
                "text/plain is not a supported image (png, jpeg, webp or gif)."
            )
        );
        assert_eq!(
            post_json(
                &f,
                "proj-1",
                "{\"mime\":\"image/png\",\"dataBase64\":\"====\"}"
            ),
            Reply::error(400, "image data was empty")
        );
        assert_eq!(
            post_json(
                &f,
                "proj-1",
                "{\"mime\":\"image/png\",\"dataBase64\":{\"toString\":1}}"
            ),
            Reply::internal()
        );
        assert_eq!(post_json(&f, "%E0%A4%A", &png("")), Reply::internal());
        // Over 8 MiB decoded: Math.round(KB).
        let big = "A".repeat((IMAGE_UPLOAD_CAP + 1536) / 3 * 4);
        let r = post_json(
            &f,
            "proj-1",
            &format!("{{\"mime\":\"image/gif\",\"dataBase64\":\"{big}\"}}"),
        );
        assert_eq!(
            r,
            Reply::error(413, "That image is 8193 KB, over the 8 MB limit.")
        );
        // Nothing was written by any refusal.
        assert!(!f.user().join("project-assets").exists());
        assert_eq!(
            std::fs::read_to_string(f.user().join("projects.json")).unwrap(),
            ONE
        );
        // The name: String(v || '') then 200 UTF-16 units.
        let r = post_json(&f, "proj-1", &png(",\"name\":[\"a\",null,7,{}]"));
        assert_eq!(json(&r)["asset"]["name"], "a,,7,[object Object]");
        let long = "é".repeat(300);
        let r = post_json(&f, "proj-1", &png(&format!(",\"name\":\"{long}\"")));
        assert_eq!(
            json(&r)["asset"]["name"].as_str().unwrap().chars().count(),
            200
        );
        let r = post_json(&f, "proj-1", &png(",\"name\":0"));
        assert_eq!(json(&r)["asset"]["name"], "");
    }

    #[test]
    fn twelve_images_at_most_and_the_limit_holds_under_the_lock() {
        let f = Fixture::new(ONE);
        let body = format!("{{\"mime\":\"image/webp\",\"dataBase64\":\"{PNG}\"}}");
        for _ in 0..MAX_PROJECT_IMAGES {
            assert_eq!(post_json(&f, "proj-1", &body).status, 200);
        }
        assert_eq!(
            post_json(&f, "proj-1", &body),
            Reply::error(400, "A project holds at most 12 images.")
        );
        assert_eq!(
            std::fs::read_dir(f.user().join("project-assets/proj-1"))
                .unwrap()
                .count(),
            MAX_PROJECT_IMAGES
        );
    }

    #[test]
    fn another_users_project_is_not_found_and_ids_cannot_leave_the_directory() {
        let f = Fixture::new(ONE);
        let body = format!("{{\"mime\":\"image/png\",\"dataBase64\":\"{PNG}\"}}");
        let id = json(&post_json(&f, "proj-1", &body))["asset"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        std::fs::create_dir_all(f.root.path().join("users").join(OTHER)).unwrap();
        assert_eq!(
            get(f.ctx_as(OTHER), "proj-1", &id),
            Reply::error(404, "no such project")
        );
        assert_eq!(
            delete(f.ctx_as(OTHER), "proj-1", &id),
            Reply::error(404, "no such project")
        );
        assert_eq!(get(f.ctx_as("../../x"), "proj-1", &id), Reply::internal());
        // A traversal in the image id is reduced to [A-Za-z0-9_-] before it names a file.
        std::fs::write(f.user().join("projects.json.secret"), "x").unwrap();
        assert_eq!(
            get(f.ctx(), "proj-1", "..%2F..%2Fprojects.json.secret"),
            Reply::error(404, "no such image")
        );
        assert_eq!(
            get(f.ctx(), "proj-1", &format!("%2E%2E%2F{id}")).status,
            200,
            "reduced to the plain id"
        );
        assert_eq!(get(f.ctx(), "proj-1", "%ZZ"), Reply::internal());
    }

    #[test]
    fn unusual_stored_data_answers_as_node_does() {
        let mk = |assets: &str| {
            Fixture::new(&format!("{{\"projects\":[{{\"id\":\"p\",\"assets\":{assets},\"retiredAssets\":[{{\"id\":\"old\",\"mime\":\"image/gif\"}}]}}]}}"))
        };
        let f = mk("null");
        assert_eq!(get(f.ctx(), "p", "x"), Reply::error(404, "no such image"));
        // A retired image is readable, not deletable.
        std::fs::create_dir_all(f.user().join("project-assets/p")).unwrap();
        std::fs::write(f.user().join("project-assets/p/old"), "GIF").unwrap();
        assert_eq!(get(f.ctx(), "p", "old").status, 200);
        assert_eq!(
            delete(f.ctx(), "p", "old"),
            Reply::error(404, "no such image")
        );
        let f = mk("{\"a\":1}");
        assert_eq!(get(f.ctx(), "p", "x"), Reply::internal());
        let f = mk("[null, {\"id\":\"x\"}]");
        assert_eq!(get(f.ctx(), "p", "x"), Reply::internal());
        let f = mk("[{\"id\":\"x\"}, null]");
        assert_eq!(
            get(f.ctx(), "p", "x"),
            Reply::error(404, "image data is missing")
        );
        let f = mk("[{\"id\":\"x\",\"mime\":7}]");
        std::fs::create_dir_all(f.user().join("project-assets/p")).unwrap();
        std::fs::write(f.user().join("project-assets/p/x"), "x").unwrap();
        assert_eq!(get(f.ctx(), "p", "x"), Reply::internal());
        let f = Fixture::new("{\"projects\":[");
        assert_eq!(get(f.ctx(), "p", "x"), Reply::internal());
    }

    #[test]
    fn a_held_lock_is_a_503_and_nothing_is_left_behind() {
        let f = Fixture::new(ONE);
        std::fs::write(f.user().join("projects.json.lock"), "1 1\n").unwrap();
        let body = format!("{{\"mime\":\"image/png\",\"dataBase64\":\"{PNG}\"}}");
        let r = post_json(&f, "proj-1", &body);
        assert_eq!(
            r,
            Reply::error(503, "The project store is busy. Try again shortly.")
        );
        assert_eq!(
            std::fs::read_dir(f.user().join("project-assets/proj-1"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            std::fs::read_to_string(f.user().join("projects.json")).unwrap(),
            ONE
        );
    }

    #[test]
    fn body_limit_is_nodes() {
        assert_eq!(BODY_LIMIT, 11_709_100);
    }
}
