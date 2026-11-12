//! The coworker conversation's one tool, `space_page_save` (sbstndalton/noevia#2446, slice 4b).
//! All of its logic is here; the front only authenticates the caller and passes the ids through.
//!
//! Grants, from the conversation outward: the chat and project must be one coworker's own
//! ([`Store::resolve_conversation`]), and the Space must be one of that coworker's `spaces`
//! (default `defaultSpace`). Nothing the model says names an account, a coworker or a project.
//!
//! The same draft twice is one page. A draft is the sha256 of the coworker, the Space, the title
//! and the content; `page-saves.json` in the account's directory maps it to the page it made.
//! A page is written first and the ledger second, so a crash between them leaves a page the
//! ledger does not know: the repeat then finds it by its row (same title, same content version,
//! same coworker) and records it. A page the person deleted is not returned: the draft saves
//! again.

use crate::{
    clean_line, is_id, version_of, Error, PageRow, Specialist, Store, MAX_PAGE_BYTES,
    MAX_TITLE_CHARS,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const LEDGER: &str = "page-saves.json";
/// Drafts remembered; the oldest go first.
pub const MAX_LEDGER: usize = 2000;

/// What the model asked for. Any other field is ignored here, never read as a grant.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavePage {
    #[serde(default)]
    pub space_id: Option<String>,
    pub title: String,
    pub content: String,
}

/// `{pageId, spaceId, title, link}` and whether this call made the page.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Saved {
    pub page_id: String,
    pub space_id: String,
    pub title: String,
    pub link: String,
    pub created: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    saves: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    key: String,
    space_id: String,
    page_id: String,
}

/// sha256 hex of the coworker, Space, title and content, each length-prefixed.
fn draft_key(specialist: &str, space: &str, title: &str, content: &str) -> String {
    let mut h = Sha256::new();
    for part in [specialist, space, title, content] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn saved(row: &PageRow, space: &str, created: bool) -> Saved {
    Saved {
        page_id: row.id.clone(),
        space_id: space.to_owned(),
        title: row.title.clone(),
        link: format!("/spaces/{space}/{}", row.id),
        created,
    }
}

/// The Space this call writes to, or why it may not.
fn destination<'a>(who: &'a Specialist, asked: Option<&'a str>) -> Result<&'a str, Error> {
    let space = match asked.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => who.default_space.as_deref().ok_or_else(|| {
            Error::Invalid(
                "This coworker has no default Space, so spaceId is required; none was saved."
                    .into(),
            )
        })?,
    };
    if !is_id(space) || !who.spaces.iter().any(|s| s == space) {
        return Err(Error::Invalid(
            "That Space is not one this coworker may save to; nothing was saved.".into(),
        ));
    }
    Ok(space)
}

impl Store {
    /// Saves a page for the coworker whose conversation is `chat_id` in `project_id`.
    /// [`Error::NotFound`] when the two ids are not one coworker's own (including another
    /// account's); [`Error::Invalid`] (text for the model) for a Space it may not use.
    pub fn space_page_save(
        &self,
        project_id: &str,
        chat_id: &str,
        req: &SavePage,
    ) -> Result<Saved, Error> {
        let title = clean_line("title", &req.title, MAX_TITLE_CHARS)?;
        if req.content.trim().is_empty() {
            return Err(Error::Invalid("content must not be empty".into()));
        }
        if req.content.len() > MAX_PAGE_BYTES {
            return Err(Error::PageTooLarge);
        }
        self.locked(|| {
            let who = self
                .resolve_conversation_held(project_id, chat_id)?
                .ok_or(Error::NotFound)?;
            let space = destination(&who, req.space_id.as_deref())?;
            // A Space that was granted and since deleted is a refusal, not a 404 for the tool.
            let known: crate::SpaceList = self.read(self.dir(), "spaces.json")?;
            if !known.spaces.iter().any(|s| s.id == space) {
                return Err(Error::Invalid(
                    "That Space no longer exists; nothing was saved.".into(),
                ));
            }
            let key = draft_key(&who.id, space, &title, &req.content);
            let version = version_of(&req.content);
            let index = self.page_index(space)?;
            let mut ledger: Ledger = self.read(self.dir(), LEDGER)?;
            let live = |id: &str| index.pages.iter().find(|p| p.id == id);
            if let Some(row) = ledger
                .saves
                .iter()
                .find(|e| e.key == key && e.space_id == space)
                .and_then(|e| live(&e.page_id))
            {
                return Ok(saved(row, space, false));
            }
            // The page of a crash between the page and the ledger.
            if let Some(row) = index
                .pages
                .iter()
                .find(|p| p.saved_by == who.id && p.title == title && p.version == version)
            {
                let (page, row) = (row.id.clone(), row.clone());
                remember(self, &mut ledger, key, space, &page)?;
                return Ok(saved(&row, space, false));
            }
            let put = crate::PagePut {
                title: Some(title.clone()),
                content: req.content.clone(),
                version: None,
            };
            let page = crate::new_id();
            let written = self.put_page_held(space, &page, put, Some(title), &who.id)?;
            remember(self, &mut ledger, key, space, &written.page.id)?;
            Ok(saved(&written.page, space, true))
        })
    }

    /// [`Store::resolve_conversation`] with the account's lock already held.
    pub(crate) fn resolve_conversation_held(
        &self,
        project_id: &str,
        chat_id: &str,
    ) -> Result<Option<Specialist>, Error> {
        if !project_id.starts_with(server_projects::store::SPECIALIST_PREFIX)
            || !chat_id.starts_with(crate::backing::CHAT_PREFIX)
        {
            return Ok(None);
        }
        let list: crate::SpecialistList = self.read(self.dir(), crate::lists::SPECIALISTS)?;
        Ok(list.specialists.into_iter().find(|s| {
            s.project_id.as_deref() == Some(project_id) && s.chat_id.as_deref() == Some(chat_id)
        }))
    }
}

fn remember(
    store: &Store,
    ledger: &mut Ledger,
    key: String,
    space: &str,
    page: &str,
) -> Result<(), Error> {
    ledger
        .saves
        .retain(|e| !(e.key == key && e.space_id == space));
    ledger.saves.push(Entry {
        key,
        space_id: space.to_owned(),
        page_id: page.to_owned(),
    });
    if ledger.saves.len() > MAX_LEDGER {
        let extra = ledger.saves.len() - MAX_LEDGER;
        ledger.saves.drain(..extra);
    }
    store.write(store.dir(), LEDGER, &*ledger)
}
