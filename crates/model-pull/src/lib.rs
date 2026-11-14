//! The model pull and the MTP artifact check noevia-core still answers, ported as pure functions
//! over a [`Host`] (track R11), from `routes/models.cjs`, `models.cjs` `deriveUserModelName`,
//! `model-manager.cjs`, `llamacpp-manager.cjs` (`pull`, `headers`), `llamacpp-variants.cjs` and
//! `mtp-artifact.cjs`:
//!
//! - `POST /api/models/pull`: the models write prelude (a member gets 403 `administrator
//!   required`), the body read by http.cjs `readBody` (1 MiB; larger is 413) and `JSON.parse`d
//!   (400 `invalid JSON`), 400 `checkpoint required`, 404 `model management is disabled`, the
//!   model name (`body.modelName` or [`manager::derive_user_model_name`]), the manager's request
//!   ([`manager::lemonade_pull_request`], or for llama.cpp: the maintenance gate entered, the
//!   checkpoint checked, the download tracker told, [`manager::llamacpp_pull_request`]) and the
//!   answer: 200 `{ jobId, modelName }` or 502 `{ error }`.
//! - `GET /api/models/mtp-artifact?repo=&variant=` (any signed-in user; no prelude for a GET):
//!   400 `Invalid repository`, the manager's variants ([`manager::variants`]; 502 `Could not
//!   resolve selected files`, 404 `Variant no longer available`), then
//!   [`artifact::Artifacts::check`] over the variant's files, answered with 200 whatever it says.
//!
//! An exception core lets out of the handler is answered as index.cjs does ([`manager::Thrown`]).
//!
//! # Network egress
//!
//! - The model manager, at its configured base: `GET <base>/api/v1/pull/variants?checkpoint=` and
//!   `POST <base>/api/v1/pull` (lemonade), `POST <base>/models` (llama.cpp router).
//! - `https://huggingface.co` only, for llama.cpp's variants (`/api/models/<repo>?blobs=true`,
//!   no redirects) and for the artifact check (`/api/models/<repo>` and
//!   `/<repo>/resolve/<revision>/<file>`, redirects followed, so also Hugging Face's file CDN the
//!   resolve redirects to). No credentials go to Hugging Face. The URLs are core's strings, before
//!   WHATWG URL parsing: a repository `a/..` passes the route's test, so the variants request
//!   goes to `https://huggingface.co/api/models/a/..?blobs=true`, which a URL parser makes
//!   `/api/models/?blobs=true`.
//!
//! # Named differences
//!
//! - `body-unsupported`: a pull body Node's `JSON.parse` reads but the port cannot hold (a number
//!   outside the `f64` range, nesting deeper than 128, text in U+10F800..U+10FFFF) is not answered
//!   here: [`Outcome::Forward`], and the caller passes the request to core.
//! - `meta-not-json`: a revision reply that is not JSON gives the reason
//!   [`artifact::META_NOT_JSON`] where Node gives V8's `SyntaxError` text (an empty or
//!   white-space reply keeps Node's `Unexpected end of JSON input`).
//! - `meta-unsupported`: a revision reply Node parses but the port cannot hold reads as one that
//!   is not JSON.
//! - `meta-error-at-cap`: a revision reply whose stream fails right after exactly 4 MiB is read by
//!   Node as complete (its look past the cap swallows the error); the host reports the failure
//!   ([`artifact::Fetched::body`] is `Err` before `max_bytes`), and the reason is its message.
//!
//! # Kept as core does (recorded; worth a look upstream)
//!
//! - The artifact check runs for any signed-in user (a GET passes the models prelude), and each one
//!   may fetch up to four 16 MiB prefixes from Hugging Face; only two run at once.
//! - The variants request (llama.cpp) refuses redirects while the artifact check follows them.
//! - A llama.cpp pull whose request throws (no answer, the deadline) leaves the download tracker at
//!   `requested`: only a refused answer marks it `rejected`.
//! - Refreshing an expired cache entry still drops the oldest entry when 100 are kept, and expired
//!   entries are never purged otherwise.
//! - A revision `sha` that is an array of one hex string passes (`String([sha])`), and is echoed
//!   as an array; `?variant` absent matches a variant named `null`.
//! - The `Content-Range` test reads the header as `Headers` normalizes it (outer white space dropped).

pub mod artifact;
pub mod gguf;
pub mod js;
pub mod manager;

use artifact::{Artifacts, Fetcher, Start};
use js_json::{wtf16, JValue};
use manager::{Config, Kind, Reply, Request, Thrown};

/// http.cjs `readBody`'s limit.
pub const BODY_LIMIT: usize = 1024 * 1024;

/// What the two routes need of the deployment, besides the public fetches ([`Fetcher`]).
pub trait Host: Fetcher {
    /// `service.invalidateModelReads()`: drops the shared model reads (a write under
    /// `/api/models/` does, before and after it is handled).
    fn invalidate_model_reads(&mut self);
    /// One request to the model manager (or, for llama.cpp's variants, to Hugging Face), as
    /// http.cjs `fetchJson` sends it; `Err` for what `fetchJson` throws (no answer, the deadline,
    /// a redirect) or the llama.cpp manager's coverage loss.
    fn send(&mut self, request: &Request) -> Result<Reply, Thrown>;
    /// llama.cpp only: the maintenance gate's `enter()` (`Err`: it is closed, core's 503 with the
    /// gate's reason). Each `Ok` is matched by one [`Host::leave`].
    fn enter(&mut self) -> Result<(), Thrown>;
    fn leave(&mut self);
    /// llama.cpp only: the download tracker records a requested pull (and follows the router's
    /// `/models/sse` events).
    fn downloads_requested(&mut self, checkpoint: &JValue);
    /// llama.cpp only: the router refused the pull.
    fn downloads_rejected(&mut self, checkpoint: &JValue);
}

/// What the routes answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// `json(res, status, body)`.
    Answer { status: u16, body: JValue },
    /// Not these routes (another method): the next handler's.
    Pass,
    /// [`Outcome`] of a body the port cannot hold (`body-unsupported`): core answers it.
    Forward,
    /// The same artifact check is running ([`artifact::Start::Join`]): its answer, with 200.
    Join(artifact::Key),
}

fn answer(status: u16, body: JValue) -> Outcome {
    Outcome::Answer { status, body }
}

fn error(status: u16, message: &str) -> Outcome {
    answer(status, JValue::obj([("error", JValue::from(message))]))
}

fn thrown(t: &Thrown) -> Outcome {
    let (status, body) = t.answer();
    answer(status, body)
}

/// A request to one of the two routes.
#[derive(Debug, Clone, Copy)]
pub struct Call<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// The request target's query, after the first `?` (empty without one).
    pub query: &'a str,
    /// `authn.user.role === 'admin'`.
    pub admin: bool,
    /// The body; `None` when it is larger than [`BODY_LIMIT`].
    pub body: Option<&'a [u8]>,
}

fn writes(method: &str) -> bool {
    !matches!(method, "" | "GET" | "HEAD")
}

/// `/^\/api\/(models|model-manager|auto-roles)(\/|$)/`.
fn shared_reads(path: &str) -> bool {
    ["models", "model-manager", "auto-roles"].iter().any(|seg| {
        path.strip_prefix("/api/")
            .and_then(|rest| rest.strip_prefix(seg))
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// One request, as `modelRoutes(req, res, ctx)` handles it: the prelude, the two routes, and the
/// wrapper's invalidation after a write.
pub fn handle(
    config: &Config,
    artifacts: &mut Artifacts,
    call: Call<'_>,
    host: &mut impl Host,
) -> Outcome {
    let write = writes(call.method) && shared_reads(call.path);
    if write {
        host.invalidate_model_reads();
    }
    let out = if call.path.starts_with("/api/models/") && writes(call.method) && !call.admin {
        error(403, "administrator required")
    } else if call.path == "/api/models/mtp-artifact" && call.method == "GET" {
        match mtp_selection(config, call.query, host) {
            Ok((repo, files)) => match artifacts.start(&repo, &files, host.now_ms()) {
                Start::Done(body) => answer(200, body),
                Start::Join(key) => Outcome::Join(key),
                Start::Run(key) => {
                    let ran = artifact::run(&key, host);
                    answer(200, artifacts.finish(&key, ran, host.now_ms()))
                }
            },
            Err(out) => out,
        }
    } else if call.path == "/api/models/pull" && call.method == "POST" {
        pull(config, call.body, host)
    } else {
        Outcome::Pass
    };
    if write {
        host.invalidate_model_reads();
    }
    out
}

/// The mtp-artifact route up to `check`: the repository and the variant's files, or the answer.
pub fn mtp_selection(
    config: &Config,
    query: &str,
    host: &mut impl Host,
) -> Result<(String, JValue), Outcome> {
    let repo = js::search_param(query, "repo").unwrap_or_default();
    if !js::is_repo(&repo) {
        return Err(error(400, "Invalid repository"));
    }
    let result = manager::variants(config, &repo, &mut |r| host.send(r)).map_err(|t| thrown(&t))?;
    if !result.ok {
        return Err(error(502, "Could not resolve selected files"));
    }
    // `(result.body?.variants || []).find(v => v.name === url.searchParams.get('variant'))`
    let list = result.body.get("variants");
    let items: &[JValue] = match list {
        v if !v.truthy() => &[],
        JValue::Arr(items) => items,
        _ => {
            return Err(thrown(&Thrown::internal(
                "result.body?.variants.find is not a function",
            )))
        }
    };
    let wanted = js::search_param(query, "variant");
    let mut variant = None;
    for v in items {
        match v {
            JValue::Null => {
                return Err(thrown(&Thrown::internal(
                    "Cannot read properties of null (reading 'name')",
                )))
            }
            JValue::Obj(_) => {
                let hit = match (v.get("name"), &wanted) {
                    (JValue::Null, None) => true,
                    (JValue::Str(name), Some(w)) => name == w,
                    _ => false,
                };
                if hit {
                    variant = Some(v);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(variant) = variant else {
        return Err(error(404, "Variant no longer available"));
    };
    let files = match variant.get("files") {
        f if f.truthy() => f.clone(),
        _ => JValue::Arr(vec![variant.get("primary_file").clone()]),
    };
    Ok((repo, files))
}

/// `POST /api/models/pull` for an administrator (the prelude is [`handle`]'s).
pub fn pull(config: &Config, body: Option<&[u8]>, host: &mut impl Host) -> Outcome {
    let Some(bytes) = body.filter(|b| b.len() <= BODY_LIMIT) else {
        return error(413, "Request exceeds size limit");
    };
    let body = match wtf16::parse(&String::from_utf8_lossy(bytes)) {
        Ok(v) => v,
        Err(wtf16::Refusal::Unsupported) => return Outcome::Forward,
        Err(wtf16::Refusal::Invalid) => return error(400, "invalid JSON"),
    };
    if body == JValue::Null {
        return thrown(&Thrown::internal(
            "Cannot read properties of null (reading 'checkpoint')",
        ));
    }
    let checkpoint = body.get("checkpoint");
    if !checkpoint.truthy() {
        return error(400, "checkpoint required");
    }
    if config.kind == Kind::None {
        return error(404, "model management is disabled");
    }
    let model_name = match body.get("modelName") {
        name if name.truthy() => name.clone(),
        _ => match manager::derive_user_model_name(checkpoint) {
            Ok(name) => JValue::Str(name),
            Err(t) => return thrown(&t),
        },
    };
    let recipe = match body.get("recipe") {
        r if r.truthy() => r.clone(),
        _ => JValue::from("llamacpp"),
    };
    let reply = match config.kind {
        Kind::Llamacpp => llamacpp_pull(config, checkpoint, host),
        _ => host.send(&manager::lemonade_pull_request(
            config,
            &model_name,
            checkpoint,
            &recipe,
        )),
    };
    let r = match reply {
        Ok(r) => r,
        Err(t) => return thrown(&t),
    };
    if r.ok {
        let first = [r.body.get("id"), r.body.get("job_id")]
            .into_iter()
            .find(|v| v.truthy())
            .cloned()
            .unwrap_or_else(|| JValue::from("pull"));
        let name = match r.body.get("modelName") {
            n if n.truthy() => n.clone(),
            _ => model_name,
        };
        answer(200, JValue::obj([("jobId", first), ("modelName", name)]))
    } else {
        let err = match r.body.get("error") {
            e if e.truthy() => e.clone(),
            _ => JValue::Str(format!(
                "pull failed: {}",
                js_json::number_to_string(f64::from(r.status))
            )),
        };
        answer(502, JValue::obj([("error", err)]))
    }
}

/// llamacpp-manager.cjs `pull({ checkpoint })` under `mutate` (the maintenance gate entered for
/// the whole call). A pull the router accepts answers with the checkpoint as its job id and model
/// name.
fn llamacpp_pull(
    config: &Config,
    checkpoint: &JValue,
    host: &mut impl Host,
) -> Result<Reply, Thrown> {
    host.enter()?;
    let out = llamacpp_pull_entered(config, checkpoint, host);
    host.leave();
    out
}

fn llamacpp_pull_entered(
    config: &Config,
    checkpoint: &JValue,
    host: &mut impl Host,
) -> Result<Reply, Thrown> {
    // `/…/.test(checkpoint || '')` reads `String(checkpoint)`.
    let text = js::to_string(checkpoint).ok_or_else(|| Thrown::internal(js::NO_PRIMITIVE))?;
    if !manager::is_llamacpp_checkpoint(&text) {
        return Ok(Reply {
            ok: false,
            status: 400,
            body: JValue::obj([("error", JValue::from(manager::LLAMACPP_CHECKPOINT))]),
        });
    }
    host.downloads_requested(checkpoint);
    let reply = host.send(&manager::llamacpp_pull_request(config, checkpoint))?;
    if !reply.ok {
        host.downloads_rejected(checkpoint);
        return Ok(reply);
    }
    Ok(Reply {
        body: JValue::obj([
            ("id", checkpoint.clone()),
            ("modelName", checkpoint.clone()),
        ]),
        ..reply
    })
}
