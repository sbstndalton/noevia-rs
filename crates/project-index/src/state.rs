//! The per-file in-process state of rag.cjs: `fileGenerations`, `fileVersions` and `pruneFileState`.
//!
//! Every index or delete of a file bumps its generation, so a run that was superseded stops before it
//! writes more; the version (sha256 of the full text) a re-index was requested at is what a search
//! keeps chunks of, and `None` (Node's `null`) after a delete filters every chunk of the file out.
//! The queue that chains runs of one file is the wiring's (one lock per [`FileKey`]); this is the
//! bookkeeping that queue consults.

use std::collections::HashMap;

/// `fileKey`: account (or none for the shared index), project, file name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileKey {
    pub user: Option<String>,
    pub project: String,
    pub file: String,
}

impl FileKey {
    pub fn new(project: &str, file: &str, user: Option<&str>) -> Self {
        // `userId || null`: an empty account id is the shared index.
        Self { user: user.filter(|u| !u.is_empty()).map(str::to_string), project: project.into(), file: file.into() }
    }
}

/// How the last index run of a file ended, as `pruneFileState` reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// `{ ok: true, … }`.
    Ok,
    /// The run threw (`{ ok: false, failed: true }`).
    Failed,
    /// `{ ok: false, reason: 'rag-unavailable' }`.
    Unavailable,
    /// No index run was queued before (a tail append's queue resolves to `null`).
    None,
}

/// A generation as `fileGenerations.get(key)` returns it (None = no entry, JavaScript's undefined).
pub type Generation = Option<u64>;

#[derive(Debug, Default)]
pub struct FileState {
    generations: HashMap<FileKey, u64>,
    versions: HashMap<FileKey, Option<String>>,
}

impl FileState {
    pub fn new() -> Self {
        Self::default()
    }

    fn bump(&mut self, key: &FileKey) -> u64 {
        let g = self.generations.get(key).copied().unwrap_or(0) + 1;
        self.generations.insert(key.clone(), g);
        g
    }

    /// `indexProjectFile`'s bookkeeping: bumps the generation and records the wanted version.
    /// The run is current while [`FileState::current`] of the returned generation holds.
    pub fn request_index(&mut self, key: &FileKey, version: &str) -> u64 {
        let g = self.bump(key);
        self.versions.insert(key.clone(), Some(version.to_string()));
        g
    }

    /// The generation a tail append takes (it supersedes nothing).
    pub fn generation(&self, key: &FileKey) -> Generation {
        self.generations.get(key).copied()
    }

    /// `fileGenerations.get(key) === generation`.
    pub fn current(&self, key: &FileKey, generation: Generation) -> bool {
        self.generations.get(key).copied() == generation
    }

    /// `fileVersions` entry: None = no entry, Some(None) = null (deleted), Some(Some(v)) = wanted version.
    pub fn wanted(&self, key: &FileKey) -> Option<Option<&str>> {
        self.versions.get(key).map(|v| v.as_deref())
    }

    /// `isCurrentVersion`: whether a chunk cut from `version` may be returned by a search.
    pub fn is_current_version(&self, key: &FileKey, version: Option<&str>) -> bool {
        match self.versions.get(key) {
            None => true,
            Some(None) => false,
            Some(Some(want)) => version == Some(want.as_str()),
        }
    }

    /// `wantedVersions(projectId, userId)`: every file of a project with a wanted version (null included),
    /// as a search sends them to the front.
    pub fn wanted_versions(&self, project: &str, user: Option<&str>) -> Vec<(String, Option<String>)> {
        let user = user.filter(|u| !u.is_empty());
        let mut out: Vec<_> = self
            .versions
            .iter()
            .filter(|(k, _)| k.project == project && k.user.as_deref() == user)
            .map(|(k, v)| (k.file.clone(), v.clone()))
            .collect();
        out.sort();
        out
    }

    /// `deleteProjectFile`'s first step: supersede any run and filter every chunk of the file.
    pub fn request_delete(&mut self, key: &FileKey) {
        self.bump(key);
        self.versions.insert(key.clone(), None);
    }

    /// `deleteProjectFile`'s last step. `dropped`: the rows went; otherwise the open failed, and
    /// `rag_available` says whether that was a transient error (the null version must stay) or no
    /// index can exist at all. `queued`: a run of this file is still in its queue.
    pub fn after_delete(&mut self, key: &FileKey, dropped: bool, rag_available: bool, queued: bool) {
        if dropped {
            if self.versions.get(key) == Some(&None) {
                self.versions.remove(key);
            }
        } else if !rag_available {
            self.versions.remove(key);
        } else {
            return;
        }
        if !queued {
            self.generations.remove(key);
        }
    }

    /// `pruneFileState`: the file's queue drained after a run that ended with `outcome`.
    pub fn drained(&mut self, key: &FileKey, outcome: Outcome, rag_available: bool) {
        self.generations.remove(key);
        if self.versions.get(key) == Some(&None) {
            return;
        }
        if outcome == Outcome::Failed {
            return;
        }
        if outcome == Outcome::Unavailable && rag_available {
            return;
        }
        self.versions.remove(key);
    }

    /// Number of files with any state (#796: the maps must not keep a key for every file ever indexed).
    pub fn tracked(&self) -> (usize, usize) {
        (self.generations.len(), self.versions.len())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn key() -> FileKey {
        FileKey::new("p", "a.md", Some("u"))
    }

    #[test]
    fn a_newer_index_supersedes_the_older_run() {
        let mut s = FileState::new();
        let g1 = s.request_index(&key(), "v1");
        let g2 = s.request_index(&key(), "v2");
        assert!(!s.current(&key(), Some(g1)));
        assert!(s.current(&key(), Some(g2)));
        assert!(s.is_current_version(&key(), Some("v2")));
        assert!(!s.is_current_version(&key(), Some("v1")));
        s.drained(&key(), Outcome::Ok, true);
        assert_eq!(s.tracked(), (0, 0));
        assert!(s.is_current_version(&key(), Some("v1")), "nothing known: every version is current");
    }

    #[test]
    fn a_failed_run_keeps_the_wanted_version() {
        let mut s = FileState::new();
        s.request_index(&key(), "v1");
        s.drained(&key(), Outcome::Failed, true);
        assert_eq!(s.wanted(&key()), Some(Some("v1")));
        s.request_index(&key(), "v2");
        s.drained(&key(), Outcome::Unavailable, true);
        assert_eq!(s.wanted(&key()), Some(Some("v2")), "a transient open error keeps filtering");
        s.drained(&key(), Outcome::Unavailable, false);
        assert_eq!(s.wanted(&key()), None, "no deps: no index to protect");
    }

    #[test]
    fn a_delete_that_could_not_drop_keeps_filtering() {
        let mut s = FileState::new();
        s.request_delete(&key());
        s.after_delete(&key(), false, true, false);
        assert!(!s.is_current_version(&key(), Some("v1")));
        s.drained(&key(), Outcome::Ok, true);
        assert_eq!(s.wanted(&key()), Some(None), "pruning keeps a null version");
        s.request_delete(&key());
        s.after_delete(&key(), true, true, false);
        assert_eq!(s.tracked(), (0, 0));
    }

    #[test]
    fn a_delete_with_a_queued_run_keeps_its_generation() {
        let mut s = FileState::new();
        let g = s.request_index(&key(), "v1");
        s.request_delete(&key());
        s.after_delete(&key(), true, true, true);
        assert!(!s.current(&key(), Some(g)));
        assert_eq!(s.tracked(), (1, 0));
    }

    #[test]
    fn wanted_versions_are_per_project_and_account() {
        let mut s = FileState::new();
        s.request_index(&FileKey::new("p", "b", Some("u")), "vb");
        s.request_delete(&FileKey::new("p", "a", Some("u")));
        s.request_index(&FileKey::new("p", "c", None), "vc");
        s.request_index(&FileKey::new("q", "d", Some("u")), "vd");
        assert_eq!(s.wanted_versions("p", Some("u")), vec![("a".into(), None), ("b".into(), Some("vb".into()))]);
        assert_eq!(s.wanted_versions("p", Some("")), vec![("c".into(), Some("vc".into()))]);
    }
}
