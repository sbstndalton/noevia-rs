//! A Space's pages: `spaces/<spaceId>/index.json` (the rows) and `spaces/<spaceId>/<pageId>.md`.
//!
//! The page file is the truth for content and so for `version` (its sha256); the index row is what
//! lists show. A write puts the file down first and the index second, so a crash between them
//! leaves a row with a stale version that the next read corrects, never a row without a file. A
//! delete is the other way round (index first), leaving at worst an orphaned file nothing lists.

use crate::{
    clean_line, is_id, new_id, now_ms, version_of, Error, Store, DEFAULT_LIMIT, MAX_LIMIT,
    MAX_PAGES, MAX_PAGE_BYTES, MAX_QUERY_CHARS, MAX_TITLE_CHARS, SAVED_BY_YOU,
    SEARCH_BYTE_BUDGET,
};
use serde::{Deserialize, Serialize};
use server_store::json::{remove_owned_page, write_owned_page};
use server_store::RustAuth;

const INDEX: &str = "index.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageRow {
    pub id: String,
    pub title: String,
    /// sha256 hex of the content.
    pub version: String,
    pub bytes: u64,
    pub created_at: i64,
    pub updated_at: i64,
    /// `"you"` or the id of the specialist that saved it.
    pub saved_by: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct PageIndex {
    #[serde(default)]
    pub pages: Vec<PageRow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PageList {
    pub pages: Vec<PageRow>,
    /// More rows matched than are returned, or a search stopped at its read budget.
    pub truncated: bool,
    /// Pages in the Space, whatever matched.
    pub total: usize,
}

/// `PUT /api/spaces/{id}/pages/{pageId}`. A missing `version` is `null`: it only creates.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PagePut {
    #[serde(default)]
    pub title: Option<String>,
    pub content: String,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PageWrite {
    pub page: PageRow,
    /// False for a write of what the page already had.
    pub changed: bool,
    pub created: bool,
}

fn saved_by_ok(s: &str) -> Result<(), Error> {
    if s == SAVED_BY_YOU || is_id(s) {
        Ok(())
    } else {
        Err(Error::Invalid("savedBy must be \"you\" or a coworker id".into()))
    }
}

fn too_large(content: &str) -> Result<(), Error> {
    if content.len() > MAX_PAGE_BYTES {
        return Err(Error::PageTooLarge);
    }
    Ok(())
}

fn file_name(page: &str) -> String {
    format!("{page}.md")
}

impl Store {
    pub(crate) fn page_index(&self, space: &str) -> Result<PageIndex, Error> {
        self.read(&self.space_dir(space), INDEX)
    }

    fn require_space(&self, space: &str) -> Result<(), Error> {
        if !is_id(space) {
            return Err(Error::NotFound);
        }
        let known: crate::SpaceList = self.read(self.dir(), "spaces.json")?;
        known
            .spaces
            .iter()
            .any(|s| s.id == space)
            .then_some(())
            .ok_or(Error::NotFound)
    }

    fn read_content(&self, space: &str, page: &str) -> Result<String, Error> {
        std::fs::read_to_string(self.space_dir(space).join(file_name(page)))
            .map_err(|_| Error::Corrupt(file_name(page)))
    }

    fn write_index(&self, space: &str, index: &PageIndex) -> Result<(), Error> {
        self.write(&self.space_dir(space), INDEX, index)
    }

    /// Pages of a Space, newest first. `q` is a case-folded substring of title or content.
    /// A search reads at most the Space's pages, at most [`SEARCH_BYTE_BUDGET`] of content, and
    /// stops one match past `limit`.
    pub fn list_pages(
        &self,
        space: &str,
        q: Option<&str>,
        limit: Option<usize>,
        offset: usize,
    ) -> Result<PageList, Error> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT);
        if limit == 0 || limit > MAX_LIMIT {
            return Err(Error::Invalid(format!("limit must be 1 to {MAX_LIMIT}")));
        }
        let needle = match q.map(str::trim).filter(|q| !q.is_empty()) {
            Some(q) if q.chars().count() > MAX_QUERY_CHARS => {
                return Err(Error::Invalid(format!(
                    "q must be at most {MAX_QUERY_CHARS} characters"
                )))
            }
            Some(q) => Some(q.to_lowercase()),
            None => None,
        };
        self.locked(|| {
            self.require_space(space)?;
            let mut rows = self.page_index(space)?.pages;
            rows.sort_by(|a, b| {
                b.updated_at
                    .cmp(&a.updated_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
            let total = rows.len();
            let (mut pages, mut truncated) = (Vec::new(), false);
            let (mut skipped, mut scanned) = (0usize, 0u64);
            for row in rows {
                if let Some(needle) = &needle {
                    let mut hit = row.title.to_lowercase().contains(needle.as_str());
                    if !hit {
                        scanned = scanned.saturating_add(row.bytes);
                        if scanned > SEARCH_BYTE_BUDGET {
                            truncated = true;
                            break;
                        }
                        hit = self
                            .read_content(space, &row.id)
                            .is_ok_and(|c| c.to_lowercase().contains(needle.as_str()));
                    }
                    if !hit {
                        continue;
                    }
                }
                if skipped < offset {
                    skipped += 1;
                    continue;
                }
                if pages.len() == limit {
                    truncated = true;
                    break;
                }
                pages.push(row);
            }
            Ok(PageList {
                pages,
                truncated,
                total,
            })
        })
    }

    /// A page and its content; the version is the content's, whatever the index says.
    pub fn get_page(&self, space: &str, page: &str) -> Result<(PageRow, String), Error> {
        if !is_id(space) || !is_id(page) {
            return Err(Error::NotFound);
        }
        self.locked(|| {
            self.require_space(space)?;
            let mut row = self
                .page_index(space)?
                .pages
                .into_iter()
                .find(|p| p.id == page)
                .ok_or(Error::NotFound)?;
            let content = self.read_content(space, page)?;
            row.version = version_of(&content);
            row.bytes = content.len() as u64;
            Ok((row, content))
        })
    }

    /// A new page with a new id.
    pub fn create_page(
        &self,
        space: &str,
        title: &str,
        content: &str,
        saved_by: &str,
    ) -> Result<PageWrite, Error> {
        let put = PagePut {
            title: Some(title.to_owned()),
            content: content.to_owned(),
            version: None,
        };
        self.put_page(space, &new_id(), put, saved_by)
    }

    /// The revision rule for one page; see the crate docs.
    pub fn put_page(
        &self,
        space: &str,
        page: &str,
        put: PagePut,
        saved_by: &str,
    ) -> Result<PageWrite, Error> {
        if !is_id(space) || !is_id(page) {
            return Err(Error::NotFound);
        }
        saved_by_ok(saved_by)?;
        too_large(&put.content)?;
        let title = put
            .title
            .as_deref()
            .map(|t| clean_line("title", t, MAX_TITLE_CHARS))
            .transpose()?;
        self.locked(|| {
            self.require_space(space)?;
            let mut index = self.page_index(space)?;
            let at = index.pages.iter().position(|p| p.id == page);
            let new_version = version_of(&put.content);
            let dir = self.space_dir(space);
            let Some(at) = at else {
                if put.version.is_some() {
                    return Err(Error::NotFound);
                }
                let Some(title) = title else {
                    return Err(Error::Invalid("title is required for a new page".into()));
                };
                if index.pages.len() >= MAX_PAGES {
                    return Err(Error::Limit {
                        code: "page_limit",
                        message: format!("At most {MAX_PAGES} pages in a Space"),
                    });
                }
                let now = now_ms();
                let row = PageRow {
                    id: page.to_owned(),
                    title,
                    version: new_version,
                    bytes: put.content.len() as u64,
                    created_at: now,
                    updated_at: now,
                    saved_by: saved_by.to_owned(),
                };
                write_owned_page(&dir, &file_name(page), &put.content, RustAuth::owned())?;
                index.pages.push(row.clone());
                self.write_index(space, &index)?;
                return Ok(PageWrite {
                    page: row,
                    changed: true,
                    created: true,
                });
            };
            let current = self.read_content(space, page)?;
            let current_version = version_of(&current);
            let mut row = index.pages.get(at).cloned().ok_or(Error::NotFound)?;
            row.version = current_version.clone();
            row.bytes = current.len() as u64;
            let Some(base) = put.version.as_deref() else {
                return Err(Error::Stale {
                    revision: None,
                    version: Some(current_version),
                });
            };
            let same_title = title.as_ref().is_none_or(|t| *t == row.title);
            if new_version == current_version && same_title {
                return Ok(PageWrite {
                    page: row,
                    changed: false,
                    created: false,
                });
            }
            if base != current_version {
                return Err(Error::Stale {
                    revision: None,
                    version: Some(current_version),
                });
            }
            if let Some(t) = title {
                row.title = t;
            }
            row.version = new_version;
            row.bytes = put.content.len() as u64;
            row.updated_at = now_ms();
            row.saved_by = saved_by.to_owned();
            if let Some(slot) = index.pages.get_mut(at) {
                *slot = row.clone();
            }
            write_owned_page(&dir, &file_name(page), &put.content, RustAuth::owned())?;
            self.write_index(space, &index)?;
            Ok(PageWrite {
                page: row,
                changed: true,
                created: false,
            })
        })
    }

    pub fn delete_page(&self, space: &str, page: &str) -> Result<(), Error> {
        if !is_id(space) || !is_id(page) {
            return Err(Error::NotFound);
        }
        self.locked(|| {
            self.require_space(space)?;
            let mut index = self.page_index(space)?;
            let at = index
                .pages
                .iter()
                .position(|p| p.id == page)
                .ok_or(Error::NotFound)?;
            index.pages.remove(at);
            self.write_index(space, &index)?;
            remove_owned_page(
                &self.space_dir(space),
                &file_name(page),
                RustAuth::owned(),
            )?;
            Ok(())
        })
    }
}
