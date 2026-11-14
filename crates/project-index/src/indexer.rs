//! The process-level indexer: rag.cjs's `indexProjectFile`, `appendFileTail` and `deleteProjectFile`
//! with their per-file queue, over [`crate::state`] and [`crate::index`].
//!
//! Runs of one file are serialized (one lock per [`FileKey`]); every index or delete bumps the file's
//! generation, so a run that was superseded while it waited or embedded stops before writing more.
//! When a file's queue drains, its state is pruned (`pruneFileState`) with the outcome of the last
//! index run, as Node's queue resolves. A delete is not queued (as in Node): it supersedes and drops
//! at once. Everything the account and project lifecycle decide comes from the [`Host`].

use crate::chunk::{hash, units};
use crate::index::{
    self, Embed, Index, IndexOutcome, TailChunk, TailGates, TailOutcome, TailRequest,
};
use crate::slots::Target;
use crate::state::{FileKey, FileState, Outcome};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

/// What the indexer asks the server.
pub trait Host: Send + Sync {
    /// `indexDir(userId)/<projectId>.db`: the account's own data directory, else the shared one.
    fn index_path(&self, project: &str, user: Option<&str>) -> PathBuf;
    /// `userActive(userId)`: a deleted or disabled account never gets an index written.
    fn user_active(&self, user: &str) -> bool;
    /// `lifecycleGuard.generation(userId)`: the front's project-deletion counter.
    fn lifecycle_generation(&self, user: Option<&str>) -> u64;
    /// `lifecycleGuard.deletedSince(projectId, userId, seen)`: the front deleted this project.
    fn deleted_since(&self, project: &str, user: Option<&str>, seen: u64) -> bool;
    /// The active embedding target.
    fn active_target(&self) -> Target;
    /// `embeddingOff(target)`: the admin turned the embedding service off.
    fn embedding_off(&self, target: &Target) -> bool;
    /// `foregroundBusy()`: a chat or search is in flight.
    fn foreground_busy(&self) -> bool;
    /// `ragAvailable()`: the SQLite vector extension loaded.
    fn rag_available(&self) -> bool {
        true
    }
}

struct Lane {
    lock: Arc<Mutex<()>>,
    pending: usize,
    last: Outcome,
}

/// One per process, like rag.cjs's module state.
pub struct Indexer<H: Host> {
    host: H,
    state: Mutex<FileState>,
    lanes: Mutex<HashMap<FileKey, Lane>>,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn outcome_of(out: &IndexOutcome) -> Outcome {
    match (out.ok, out.reason.as_deref()) {
        (true, _) => Outcome::Ok,
        (false, Some("failed")) => Outcome::Failed,
        (false, _) => Outcome::Unavailable,
    }
}

impl<H: Host> Indexer<H> {
    pub fn new(host: H) -> Self {
        Self {
            host,
            state: Mutex::new(FileState::new()),
            lanes: Mutex::new(HashMap::new()),
        }
    }

    pub fn host(&self) -> &H {
        &self.host
    }

    /// `openIndex` with its checks: the account, the front's tombstone before and after the open.
    fn open(&self, project: &str, user: Option<&str>) -> Option<Index> {
        if user.is_some_and(|u| !self.host.user_active(u))
            || self.host.deleted_since(project, user, 0)
            || !self.host.rag_available()
        {
            return None;
        }
        let deleted = || self.host.deleted_since(project, user, 0);
        // Node logs and falls back to direct injection on any open error.
        index::open_index(
            &self.host.index_path(project, user),
            &self.host.active_target(),
            &deleted,
        )
        .ok()
        .flatten()
    }

    /// Joins the file's queue; returns its lock and whether a run was already queued.
    fn enter(&self, key: &FileKey) -> (Arc<Mutex<()>>, bool) {
        let mut lanes = locked(&self.lanes);
        let lane = lanes.entry(key.clone()).or_insert_with(|| Lane {
            lock: Arc::new(Mutex::new(())),
            pending: 0,
            last: Outcome::None,
        });
        let prior = lane.pending > 0;
        lane.pending += 1;
        (lane.lock.clone(), prior)
    }

    /// Leaves the queue; the last one out prunes the file's state. An index run records its outcome;
    /// a tail append prunes only when it was queued behind another run (as Node's queue does).
    fn leave(&self, key: &FileKey, outcome: Option<Outcome>, prune: bool) {
        let mut lanes = locked(&self.lanes);
        let Some(lane) = lanes.get_mut(key) else {
            return;
        };
        lane.pending = lane.pending.saturating_sub(1);
        if let Some(o) = outcome {
            lane.last = o;
        }
        if lane.pending == 0 {
            let last = lane.last;
            lanes.remove(key);
            if prune {
                locked(&self.state).drained(key, last, self.host.rag_available());
            }
        }
    }

    fn current(
        &self,
        key: &FileKey,
        generation: Option<u64>,
        project: &str,
        user: Option<&str>,
        seen: u64,
    ) -> bool {
        locked(&self.state).current(key, generation)
            && user.is_none_or(|u| self.host.user_active(u))
            && !self.host.deleted_since(project, user, seen)
    }

    /// `indexProjectFile`: index (or re-index) one file; waits for any earlier run of the same file.
    pub fn index_file(
        &self,
        project: &str,
        file: &str,
        text: &str,
        user: Option<&str>,
        embed: &mut dyn Embed,
    ) -> IndexOutcome {
        let user = user.filter(|u| !u.is_empty());
        let key = FileKey::new(project, file, user);
        let seen = self.host.lifecycle_generation(user);
        let version = hash(text);
        let generation = locked(&self.state).request_index(&key, &version);
        let (lane, _) = self.enter(&key);
        let out = {
            let _turn = locked(&lane);
            let mut is_current = || self.current(&key, Some(generation), project, user, seen);
            let mut open = || Ok(self.open(project, user));
            index::index_file_now(
                &mut open,
                file,
                &units(text),
                &version,
                &mut is_current,
                embed,
            )
        };
        self.leave(&key, Some(outcome_of(&out)), true);
        out
    }

    /// `appendFileTail`: appends a long document's pages past its head, after any queued run of the file.
    #[allow(clippy::too_many_arguments)]
    pub fn append_tail(
        &self,
        project: &str,
        file: &str,
        head_text: &str,
        tail: &[TailChunk],
        user: Option<&str>,
        window: (usize, Option<i64>, Option<i64>, bool),
        embed: &mut dyn Embed,
    ) -> Result<TailOutcome, String> {
        let user = user.filter(|u| !u.is_empty());
        let key = FileKey::new(project, file, user);
        let seen = self.host.lifecycle_generation(user);
        let generation = locked(&self.state).generation(&key);
        let (lane, prior) = self.enter(&key);
        let out = {
            let _turn = locked(&lane);
            let wanted: Option<Option<String>> = locked(&self.state)
                .wanted(&key)
                .map(|w| w.map(str::to_string));
            let path = self.host.index_path(project, user);
            let mut is_current = || self.current(&key, generation, project, user, seen);
            let busy = || self.host.foreground_busy();
            let off = self.host.embedding_off(&self.host.active_target());
            let mut gates = TailGates {
                is_current: &mut is_current,
                busy: &busy,
                index_exists: path.exists(),
                wanted: wanted.as_ref().map(|w| w.as_deref()),
                embedding_off: off,
            };
            let (limit, from_page, to_page, final_window) = window;
            let req = TailRequest {
                file,
                head_text,
                tail,
                limit,
                from_page,
                to_page,
                final_window,
            };
            let mut open = || Ok(self.open(project, user));
            index::append_tail_now(&mut open, &req, &mut gates, embed)
        };
        self.leave(&key, None, prior);
        out
    }

    /// `deleteProjectFile`: supersedes any run of the file and drops its rows (its text only with `keep_media`).
    pub fn delete_file(
        &self,
        project: &str,
        file: &str,
        user: Option<&str>,
        keep_media: bool,
    ) -> Result<(), String> {
        let user = user.filter(|u| !u.is_empty());
        let key = FileKey::new(project, file, user);
        locked(&self.state).request_delete(&key);
        let mut open = || Ok(self.open(project, user));
        let dropped = index::delete_file(&mut open, file, keep_media)?;
        let queued = locked(&self.lanes).contains_key(&key);
        locked(&self.state).after_delete(&key, dropped, self.host.rag_available(), queued);
        Ok(())
    }

    /// `wantedVersions`: this process's wanted versions of one project's files, for a search.
    pub fn wanted_versions(
        &self,
        project: &str,
        user: Option<&str>,
    ) -> Vec<(String, Option<String>)> {
        locked(&self.state).wanted_versions(project, user)
    }

    /// `isCurrentVersion` for a search hit.
    pub fn is_current_version(
        &self,
        project: &str,
        file: &str,
        user: Option<&str>,
        version: Option<&str>,
    ) -> bool {
        locked(&self.state).is_current_version(&FileKey::new(project, file, user), version)
    }

    /// `trackedFileState`: (queues, generations, versions).
    pub fn tracked(&self) -> (usize, usize, usize) {
        let (g, v) = locked(&self.state).tracked();
        (locked(&self.lanes).len(), g, v)
    }
}
