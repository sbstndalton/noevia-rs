//! `specialists.json` and `spaces.json`: the two lists, each `{revision, <items>}`.

use crate::{
    clean_line, is_id, new_id, now_ms, Error, Store, MAX_BRIEF_CHARS, MAX_GLYPH_CHARS,
    MAX_NAME_CHARS, MAX_SPACES, MAX_SPACE_NAME_CHARS, MAX_SPECIALISTS, MAX_TINT_CHARS,
};
use serde::{Deserialize, Deserializer, Serialize};

pub(crate) const SPECIALISTS: &str = "specialists.json";
const SPACES: &str = "spaces.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Specialist {
    pub id: String,
    pub name: String,
    pub glyph: String,
    /// A design-token name, or "" for the default tint.
    pub tint: String,
    /// The coworker's instructions.
    pub brief: String,
    /// The backing project and conversation (see [`crate::backing`]): `null` only for a coworker
    /// made before they existed, until the next read or patch gives it both.
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub chat_id: Option<String>,
    /// The Spaces this coworker may save pages to.
    #[serde(default)]
    pub spaces: Vec<String>,
    #[serde(default)]
    pub default_space: Option<String>,
    /// The toolboxes the coworker's conversation may use: ids of the account's permitted
    /// toolboxes, mirrored into the backing project's tool settings.
    #[serde(default)]
    pub toolboxes: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SpecialistList {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub specialists: Vec<Specialist>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Space {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SpaceList {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub spaces: Vec<Space>,
}

/// `POST /api/specialists`. Unknown fields are refused, so `projectId` and `chatId` can only be set
/// by the server.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewSpecialist {
    pub name: String,
    #[serde(default)]
    pub glyph: Option<String>,
    #[serde(default)]
    pub tint: Option<String>,
    #[serde(default)]
    pub brief: Option<String>,
    #[serde(default)]
    pub spaces: Option<Vec<String>>,
    #[serde(default)]
    pub default_space: Option<String>,
    #[serde(default)]
    pub toolboxes: Option<Vec<String>>,
}

/// `PATCH /api/specialists/{id}`: the revision the client loaded, and the fields to change.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SpecialistPatch {
    pub revision: u64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub glyph: Option<String>,
    #[serde(default)]
    pub tint: Option<String>,
    #[serde(default)]
    pub brief: Option<String>,
    #[serde(default)]
    pub spaces: Option<Vec<String>>,
    /// `Some(None)` clears it.
    #[serde(default, deserialize_with = "some")]
    pub default_space: Option<Option<String>>,
    #[serde(default)]
    pub toolboxes: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewSpace {
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SpacePatch {
    pub revision: u64,
    #[serde(default)]
    pub name: Option<String>,
}

fn some<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

fn glyph(s: &str) -> Result<String, Error> {
    let t = s.trim();
    if t.chars().count() > MAX_GLYPH_CHARS || t.chars().any(char::is_control) {
        return Err(Error::Invalid(format!(
            "glyph must be at most {MAX_GLYPH_CHARS} characters"
        )));
    }
    Ok(t.to_owned())
}

fn tint(s: &str) -> Result<String, Error> {
    let ok = s.len() <= MAX_TINT_CHARS
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok {
        return Err(Error::Invalid(
            "tint must be a design-token name (lowercase letters, digits, hyphens)".into(),
        ));
    }
    Ok(s.to_owned())
}

fn brief(s: &str) -> Result<String, Error> {
    if s.chars().count() > MAX_BRIEF_CHARS {
        return Err(Error::Invalid(format!(
            "brief must be at most {MAX_BRIEF_CHARS} characters"
        )));
    }
    Ok(s.to_owned())
}

fn stale_revision(current: u64) -> Error {
    Error::Stale {
        revision: Some(current),
        version: None,
    }
}

impl Store {
    /// The coworkers. With a backing, a read first repairs whatever a crash or an older version
    /// left ([`crate::backing`]).
    pub fn list_specialists(&self) -> Result<SpecialistList, Error> {
        self.repair_if_needed()?;
        self.locked(|| self.read(self.dir(), SPECIALISTS))
    }

    pub fn get_specialist(&self, id: &str) -> Result<(u64, Specialist), Error> {
        if !is_id(id) {
            return Err(Error::NotFound);
        }
        let list = self.list_specialists()?;
        let found = list.specialists.into_iter().find(|s| s.id == id);
        found.map(|s| (list.revision, s)).ok_or(Error::NotFound)
    }

    /// The Spaces a specialist may be granted: each must be an id of this account's Spaces.
    fn granted(&self, spaces: &[String]) -> Result<Vec<String>, Error> {
        let known: SpaceList = self.read(self.dir(), SPACES)?;
        let mut out: Vec<String> = Vec::new();
        for id in spaces {
            if !is_id(id) || !known.spaces.iter().any(|s| &s.id == id) {
                return Err(Error::Invalid("spaces must be ids of your Spaces".into()));
            }
            if !out.contains(id) {
                out.push(id.clone());
            }
        }
        Ok(out)
    }

    pub fn create_specialist(&self, new: NewSpecialist) -> Result<(u64, Specialist), Error> {
        let name = clean_line("name", &new.name, MAX_NAME_CHARS)?;
        let glyph_v = glyph(new.glyph.as_deref().unwrap_or(""))?;
        let tint_v = tint(new.tint.as_deref().unwrap_or(""))?;
        let brief_v = brief(new.brief.as_deref().unwrap_or(""))?;
        self.locked(|| {
            let mut list: SpecialistList = self.read(self.dir(), SPECIALISTS)?;
            if list.specialists.len() >= MAX_SPECIALISTS {
                return Err(Error::Limit {
                    code: "specialist_limit",
                    message: format!("At most {MAX_SPECIALISTS} coworkers"),
                });
            }
            let spaces = self.granted(new.spaces.as_deref().unwrap_or(&[]))?;
            if new
                .default_space
                .as_ref()
                .is_some_and(|d| !spaces.contains(d))
            {
                return Err(Error::Invalid("defaultSpace must be one of spaces".into()));
            }
            let boxes = self.toolbox_ids(new.toolboxes.as_deref().unwrap_or(&[]))?;
            let now = now_ms();
            let sid = new_id();
            let (project_id, chat_id) = self.backing_ids(&sid);
            let made = Specialist {
                id: sid,
                name,
                glyph: glyph_v,
                tint: tint_v,
                brief: brief_v,
                project_id,
                chat_id,
                spaces,
                default_space: new.default_space,
                toolboxes: boxes,
                created_at: now,
                updated_at: now,
            };
            list.specialists.push(made.clone());
            list.revision += 1;
            // The record first, then the project (see `backing`): a crash between leaves a record
            // whose project the next read makes. A failed project write takes the record back.
            self.write(self.dir(), SPECIALISTS, &list)?;
            if let Err(e) = self.mirror(&made) {
                list.specialists.pop();
                list.revision += 1;
                let _ = self.write(self.dir(), SPECIALISTS, &list);
                return Err(e);
            }
            Ok((list.revision, made))
        })
    }

    pub fn update_specialist(
        &self,
        id: &str,
        patch: SpecialistPatch,
    ) -> Result<(u64, Specialist), Error> {
        if !is_id(id) {
            return Err(Error::NotFound);
        }
        let name = patch
            .name
            .as_deref()
            .map(|n| clean_line("name", n, MAX_NAME_CHARS))
            .transpose()?;
        let glyph_v = patch.glyph.as_deref().map(glyph).transpose()?;
        let tint_v = patch.tint.as_deref().map(tint).transpose()?;
        let brief_v = patch.brief.as_deref().map(brief).transpose()?;
        self.locked(|| {
            let mut list: SpecialistList = self.read(self.dir(), SPECIALISTS)?;
            let at = list
                .specialists
                .iter()
                .position(|s| s.id == id)
                .ok_or(Error::NotFound)?;
            if patch.revision != list.revision {
                return Err(stale_revision(list.revision));
            }
            let spaces = patch
                .spaces
                .as_deref()
                .map(|s| self.granted(s))
                .transpose()?;
            let boxes = patch
                .toolboxes
                .as_deref()
                .map(|b| self.toolbox_ids(b))
                .transpose()?;
            let item = list.specialists.get_mut(at).ok_or(Error::NotFound)?;
            let mut next = item.clone();
            if let Some(v) = name {
                next.name = v;
            }
            if let Some(v) = glyph_v {
                next.glyph = v;
            }
            if let Some(v) = tint_v {
                next.tint = v;
            }
            if let Some(v) = brief_v {
                next.brief = v;
            }
            if let Some(v) = spaces {
                next.spaces = v;
            }
            if let Some(v) = boxes {
                next.toolboxes = v;
            }
            if next.project_id.is_none() {
                (next.project_id, next.chat_id) = self.backing_ids(&next.id);
            }
            if let Some(v) = patch.default_space {
                next.default_space = v;
            }
            if next
                .default_space
                .as_ref()
                .is_some_and(|d| !next.spaces.contains(d))
            {
                return Err(Error::Invalid("defaultSpace must be one of spaces".into()));
            }
            next.updated_at = now_ms();
            *item = next.clone();
            list.revision += 1;
            // Record first, then the same brief and toolboxes into the project, in this one lock.
            // A mirror that fails is repaired by the next read (the record is the truth).
            self.write(self.dir(), SPECIALISTS, &list)?;
            self.mirror(&next)?;
            Ok((list.revision, next))
        })
    }

    /// Returns the new revision. The record goes first (that is the delete); the chat (tombstone
    /// first) and then the project follow, and a crash between leaves an orphan that the next
    /// read removes (see [`crate::backing`]).
    pub fn delete_specialist(&self, id: &str, revision: u64) -> Result<u64, Error> {
        if !is_id(id) {
            return Err(Error::NotFound);
        }
        self.locked(|| {
            let mut list: SpecialistList = self.read(self.dir(), SPECIALISTS)?;
            let at = list
                .specialists
                .iter()
                .position(|s| s.id == id)
                .ok_or(Error::NotFound)?;
            if revision != list.revision {
                return Err(stale_revision(list.revision));
            }
            let gone = list.specialists.remove(at);
            list.revision += 1;
            self.write(self.dir(), SPECIALISTS, &list)?;
            self.unback(&gone);
            Ok(list.revision)
        })
    }

    pub fn list_spaces(&self) -> Result<SpaceList, Error> {
        self.locked(|| self.read(self.dir(), SPACES))
    }

    pub fn get_space(&self, id: &str) -> Result<(u64, Space), Error> {
        if !is_id(id) {
            return Err(Error::NotFound);
        }
        let list = self.list_spaces()?;
        let found = list.spaces.into_iter().find(|s| s.id == id);
        found.map(|s| (list.revision, s)).ok_or(Error::NotFound)
    }

    pub fn create_space(&self, new: NewSpace) -> Result<(u64, Space), Error> {
        let name = clean_line("name", &new.name, MAX_SPACE_NAME_CHARS)?;
        self.locked(|| {
            let mut list: SpaceList = self.read(self.dir(), SPACES)?;
            if list.spaces.len() >= MAX_SPACES {
                return Err(Error::Limit {
                    code: "space_limit",
                    message: format!("At most {MAX_SPACES} Spaces"),
                });
            }
            let now = now_ms();
            let made = Space {
                id: new_id(),
                name,
                created_at: now,
                updated_at: now,
            };
            list.spaces.push(made.clone());
            list.revision += 1;
            self.write(self.dir(), SPACES, &list)?;
            Ok((list.revision, made))
        })
    }

    pub fn update_space(&self, id: &str, patch: SpacePatch) -> Result<(u64, Space), Error> {
        if !is_id(id) {
            return Err(Error::NotFound);
        }
        let name = patch
            .name
            .as_deref()
            .map(|n| clean_line("name", n, MAX_SPACE_NAME_CHARS))
            .transpose()?;
        self.locked(|| {
            let mut list: SpaceList = self.read(self.dir(), SPACES)?;
            let item = list
                .spaces
                .iter_mut()
                .find(|s| s.id == id)
                .ok_or(Error::NotFound)?;
            if patch.revision != list.revision {
                return Err(stale_revision(list.revision));
            }
            if let Some(v) = name {
                item.name = v;
            }
            item.updated_at = now_ms();
            let out = item.clone();
            list.revision += 1;
            self.write(self.dir(), SPACES, &list)?;
            Ok((list.revision, out))
        })
    }

    /// Deletes a Space. One with pages needs `with_pages`. The list is written first (a crash
    /// leaves an orphaned directory, never a listed Space without its pages), then the directory
    /// goes and every specialist loses the grant (both attempted, the first error returned). Returns the new revision.
    pub fn delete_space(&self, id: &str, revision: u64, with_pages: bool) -> Result<u64, Error> {
        if !is_id(id) {
            return Err(Error::NotFound);
        }
        self.locked(|| {
            let mut list: SpaceList = self.read(self.dir(), SPACES)?;
            let at = list
                .spaces
                .iter()
                .position(|s| s.id == id)
                .ok_or(Error::NotFound)?;
            if revision != list.revision {
                return Err(stale_revision(list.revision));
            }
            let dir = self.space_dir(id);
            if !with_pages && !self.page_index(id)?.pages.is_empty() {
                return Err(Error::SpaceNotEmpty);
            }
            list.spaces.remove(at);
            list.revision += 1;
            self.write(self.dir(), SPACES, &list)?;
            // Both clean-up steps run whatever the other does; the first failure is the answer. The
            // Space is already unlisted, so a retry could not reach it again.
            let removed = match std::fs::remove_dir_all(&dir) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(Error::Io(e)),
                _ => Ok(()),
            };
            let ungranted = self.revoke_grants(id);
            removed.and(ungranted).map(|()| list.revision)
        })
    }

    /// Removes `space` from every specialist's grants and default.
    fn revoke_grants(&self, space: &str) -> Result<(), Error> {
        let mut granted: SpecialistList = self.read(self.dir(), SPECIALISTS)?;
        let mut changed = false;
        for s in &mut granted.specialists {
            if s.spaces.iter().any(|x| x == space) {
                s.spaces.retain(|x| x != space);
                if s.default_space.as_deref() == Some(space) {
                    s.default_space = None;
                }
                s.updated_at = now_ms();
                changed = true;
            }
        }
        if changed {
            granted.revision += 1;
            self.write(self.dir(), SPECIALISTS, &granted)?;
        }
        Ok(())
    }
}
