//! `hf.repo_detail(repo_id)`: every page of a repository's tree listing, as files, with the
//! reasons it can fail told apart (the two routes answer them differently).
//!
//! The reading, paging, projection and bounds are those of the draft-head lookup
//! (`draft_heads`, noevia#1973, #1985, #1995): at most 40 pages, each under the `hub_page`
//! limits; a `next` link to another origin is not followed; the entries are projected as they
//! arrive and the lookup fails once the projection passes the `model-files` caps; the final
//! parse runs in a blocking task that holds the lookup permit.
use super::net::{self, BodyError};
use crate::draft_heads::{link_header, next_link, origin_of, Exceeded, Projected, PROJECTED_NODES};
use crate::hub_page::{page_limits, PAGE_CAP};
use model_files::json::{Limits, Value};
use std::sync::Arc;

/// `hf._TREE_MAX_PAGES`.
const TREE_MAX_PAGES: usize = 40;

/// Why a listing could not be had.
#[derive(Debug, PartialEq, Eq)]
pub enum TreeError {
    /// The id is not one the Hub lists (this side sends no request for it).
    Invalid,
    /// `raise_for_status()`: a status other than 2xx.
    Status(u16),
    /// An `httpx.HTTPError`, with its message.
    Transport(String),
    /// The body is not JSON (`json.loads` raised: not an `HTTPError`, so Python's 500).
    BadBody,
    /// A page past a bound of this side, or a `next` link to another origin.
    Limit,
    /// `model-files` refused the listing (`ModelFilesError`, Python's 502).
    Files,
}

/// The page of a body, or why not.
pub(super) enum Page {
    Syntax,
    Limit,
}

/// `json.loads(body)` of one page under the `hub_page` bounds, with a syntax error told apart
/// from a bound. A UTF-8 page is parsed where it lies; the raw bytes of any other encoding are
/// dropped once decoded (noevia#1995).
pub(super) fn parse_page(body: Vec<u8>) -> Result<Value, Page> {
    use crate::prompts::Error as Decode;
    let decoded = match crate::prompts::decode_json_bytes(&body, PAGE_CAP) {
        Ok(std::borrow::Cow::Borrowed(_)) => None,
        Ok(std::borrow::Cow::Owned(text)) => Some(text),
        Err(Decode::Body) => return Err(Page::Syntax),
        Err(_) => return Err(Page::Limit),
    };
    let parsed = match decoded {
        Some(mut text) => {
            drop(body);
            text.shrink_to_fit();
            model_files::json::parse_with_limits(&text, page_limits())
        }
        None => {
            let text =
                crate::prompts::decode_json_bytes(&body, PAGE_CAP).map_err(|_| Page::Limit)?;
            model_files::json::parse_with_limits(&text, page_limits())
        }
    };
    parsed.map_err(|error| match error {
        model_files::Error::Json(_) => Page::Syntax,
        _ => Page::Limit,
    })
}

/// Lib tests: runs in the final parse's blocking task, before the parse.
pub type ParseHook = Arc<dyn Fn() + Send + Sync>;

/// The listing of `repo` as files, in `files.sort(key=(not .gguf, path.lower()))` order.
pub async fn list(
    client: &reqwest::Client,
    hf_api: &str,
    header: Option<&reqwest::header::HeaderValue>,
    repo: &str,
    lookup: &Arc<tokio::sync::OwnedSemaphorePermit>,
    parse_hook: Option<ParseHook>,
) -> Result<Vec<model_files::HfFile>, TreeError> {
    if !crate::check_updates::hub_id(repo) {
        return Err(TreeError::Invalid);
    }
    let origin = origin_of(hf_api).ok_or(TreeError::Limit)?;
    let mut url = format!("{hf_api}/models/{repo}/tree/main?recursive=true&expand=true");
    let mut projected = Projected::default();
    for _ in 0..TREE_MAX_PAGES {
        let mut request = client.get(&url);
        if let Some(value) = header {
            request = request.header(reqwest::header::AUTHORIZATION, value.clone());
        }
        let response = request
            .send()
            .await
            .map_err(|e| TreeError::Transport(net::describe(&e)))?;
        if !response.status().is_success() {
            return Err(TreeError::Status(response.status().as_u16()));
        }
        let link = link_header(response.headers());
        let body = match net::read_body(response, PAGE_CAP).await {
            Ok(body) => body,
            Err(BodyError::Transport(message)) => return Err(TreeError::Transport(message)),
            Err(BodyError::Cap) => return Err(TreeError::Limit),
        };
        let page = match parse_page(body) {
            Ok(page) => page,
            Err(Page::Syntax) => return Err(TreeError::BadBody),
            Err(Page::Limit) => return Err(TreeError::Limit),
        };
        // `if not isinstance(page, list): break`.
        let Value::Arr(items) = page else {
            break;
        };
        for item in &items {
            match projected.try_push(item) {
                Ok(()) => {}
                // `len(payload) > MODEL_FILES_STDIN_CAP` and more than `MAX_ENTRIES`: the
                // binary refuses, Python answers 502.
                Err(Exceeded::Bytes | Exceeded::Entries) => return Err(TreeError::Files),
                Err(Exceeded::Nodes) => return Err(TreeError::Limit),
            }
        }
        drop(items);
        let Some(next) = next_link(&link) else {
            break;
        };
        if origin_of(&next).as_ref() != Some(&origin) {
            return Err(TreeError::Limit);
        }
        url = next;
    }
    let payload = projected.finish();
    let lookup = lookup.clone();
    let mut files = crate::docker::spawn_blocking_admitted(move || {
        let _lookup = lookup;
        if let Some(hook) = parse_hook {
            hook();
        }
        files_of(payload)
    })
    .await
    .map_err(|_| TreeError::Files)??;
    files.sort_by_cached_key(|f| {
        let lower = f.path.to_lowercase();
        (!lower.ends_with(".gguf"), lower)
    });
    Ok(files)
}

fn files_of(payload: String) -> Result<Vec<model_files::HfFile>, TreeError> {
    let limits = Limits {
        nodes: PROJECTED_NODES,
        ..Limits::default()
    };
    let value =
        model_files::json::parse_with_limits(&payload, limits).map_err(|_| TreeError::Files)?;
    drop(payload);
    model_files::files_from_tree(&value).map_err(|_| TreeError::Files)
}
