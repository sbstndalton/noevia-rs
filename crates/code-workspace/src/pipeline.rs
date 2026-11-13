//! code-workspace.cjs's reads for the Code pipeline (#705) and its verified merge: what a released
//! task changed (`change`), what the Planner may see before any work (`projectSnapshot`), another
//! round on the same branch (`reclaim`), the fast-forward of the base branch to a reviewed head
//! (`mergePreflight`, `mergeVerified`), what the verifier copies (`verifyCheckout`) and the branch
//! tip (`branchTip`). Everything is read from the SOURCE repository, never a tree a harness held,
//! with every diff driver, textconv and external diff off.

use crate::git::{GitError, RunOptions};
use crate::{args, branch_name_ok, hostile, join, realpath, s, with, Error, JValue, Workspaces};
use std::path::Path;

/// `execFileSync`'s buffer for a diff read for review (code-workspace.cjs `CHANGE_MAX_BUFFER`).
pub const CHANGE_MAX_BUFFER: usize = 16 * 1024 * 1024;

fn coded(code: &'static str, message: &str) -> Error {
    Error {
        status: None,
        code: Some(code),
        message: message.to_string(),
    }
}

/// `/^[0-9a-f]{7,64}$/`.
fn short_sha(v: &JValue) -> bool {
    v.as_str().is_some_and(|t| {
        (7..=64).contains(&t.len()) && t.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// `/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/`.
fn full_sha(t: &str) -> bool {
    (t.len() == 40 || t.len() == 64) && t.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `text.split(/^(?=diff --git )/m)`: a piece starts at the text's start and at every line
/// start (after `\n`, `\r`, U+2028 or U+2029) that begins `diff --git `.
fn split_patches(text: &str) -> Vec<&str> {
    let mut starts = vec![0];
    let mut prev: Option<char> = None;
    for (i, c) in text.char_indices() {
        if i > 0
            && matches!(prev, Some('\n' | '\r' | '\u{2028}' | '\u{2029}'))
            && text.get(i..).is_some_and(|t| t.starts_with("diff --git "))
        {
            starts.push(i);
        }
        prev = Some(c);
    }
    starts.push(text.len());
    starts
        .windows(2)
        .filter_map(|w| match w {
            [a, b] => text.get(*a..*b),
            _ => None,
        })
        .collect()
}

/// `/^readme(\.(md|markdown|txt|rst))?$/i`.
fn is_readme(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    match lower.strip_prefix("readme") {
        Some("") => true,
        Some(ext) => matches!(ext, ".md" | ".markdown" | ".txt" | ".rst"),
        None => false,
    }
}

/// `{ ok: false, code, reason }`.
fn no(code: &str, reason: &str) -> JValue {
    JValue::obj([
        ("ok", JValue::Bool(false)),
        ("code", s(code)),
        ("reason", s(reason)),
    ])
}

struct Checked {
    record: JValue,
    base: String,
}

impl Workspaces {
    fn source_git(
        &self,
        repo: &str,
        list: &[&str],
        options: RunOptions,
    ) -> Result<String, GitError> {
        let mut env = self.env();
        env.max_buffer = options.max_buffer;
        env.raw = options.raw;
        self.git.run(&hostile(list), Some(Path::new(repo)), &env)
    }

    /// `git diff base..head` of a cleanly released task, read from the source repository:
    /// `{ baseSha, headSha, files: [{ path, patch }], truncated }`, or a coded refusal.
    pub fn change(&self, task_id: &str, max_files: usize) -> Result<JValue, Error> {
        let _held = self.guard();
        let record = self
            .read(task_id)?
            .ok_or_else(|| coded("no_workspace", "The task holds no workspace record."))?;
        if record.get("status").as_str() != Some("released") {
            return Err(coded(
                "not_released",
                "The workspace was not released cleanly, so its change was not read.",
            ));
        }
        if !short_sha(record.get("baseSha")) {
            return Err(coded(
                "no_base",
                "The repository had no base commit to compare against.",
            ));
        }
        if !short_sha(record.get("headSha")) {
            return Err(coded("no_head", "The task branch has no commit to review."));
        }
        let base = record.get("baseSha").as_str().unwrap_or("").to_string();
        let head = record.get("headSha").as_str().unwrap_or("").to_string();
        if base == head {
            return Err(coded(
                "no_change",
                "The task made no commits, so there is nothing to review.",
            ));
        }
        let repo = record.get("repo").as_str().unwrap_or("").to_string();
        let diff = |extra: &[&str]| -> Result<String, Error> {
            let mut list = vec![
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--no-renames",
            ];
            list.extend_from_slice(extra);
            let options = RunOptions {
                max_buffer: Some(self.change_max_buffer),
                raw: true,
                ..RunOptions::default()
            };
            self.source_git(&repo, &list, options).map_err(|e| {
                if e.too_large {
                    coded("too_large", "The change is too large to review.")
                } else {
                    coded("unreadable", "The change could not be read.")
                }
            })
        };
        let names_text = diff(&["--name-only", "-z", &base, &head, "--"])?;
        let names: Vec<&str> = names_text.split('\0').filter(|n| !n.is_empty()).collect();
        let text = diff(&["--unified=3", &base, &head, "--"])?;
        let chunks: Vec<&str> = split_patches(&text)
            .into_iter()
            .filter(|c| c.starts_with("diff --git "))
            .collect();
        if chunks.len() != names.len() {
            return Err(coded("unreadable", "The change could not be read."));
        }
        let files: Vec<JValue> = chunks
            .iter()
            .zip(names.iter())
            .take(max_files)
            .map(|(chunk, name)| {
                JValue::obj([
                    ("path", s(name)),
                    ("patch", s(chunk.strip_suffix('\n').unwrap_or(chunk))),
                ])
            })
            .collect();
        Ok(JValue::obj([
            ("baseSha", s(&base)),
            ("headSha", s(&head)),
            ("files", JValue::Arr(files)),
            ("truncated", JValue::Bool(names.len() > max_files)),
        ]))
    }

    /// Takes a cleanly released task's workspace again on the same branch, for the next round.
    pub fn reclaim(&self, task_id: &str, pinned: &[JValue]) -> Result<JValue, Error> {
        let _held = self.guard();
        Self::check_id(task_id)?;
        let record = self
            .read(task_id)?
            .ok_or_else(|| Error::new(Some(404), "This task holds no workspace to take again"))?;
        let status = record.get("status");
        if status.as_str() != Some("released") {
            return Err(Error::new(
                Some(409),
                format!(
                    "This task's workspace is {}, so it cannot be taken again",
                    crate::text_of(status)
                ),
            ));
        }
        let repo = record.get("repo").as_str().unwrap_or("").to_string();
        let branch = record.get("branch").as_str().unwrap_or("").to_string();
        if let Some(held) = self.list().into_iter().find(|r| {
            r.get("taskId").as_str() != Some(task_id)
                && r.get("status").as_str() != Some("released")
                && r.get("repo").as_str() == Some(repo.as_str())
                && r.get("branch").as_str() == Some(branch.as_str())
        }) {
            return Err(Error::new(
                Some(409),
                format!(
                    "Another task already writes {branch} in this repository ({})",
                    crate::text_of(held.get("status"))
                ),
            ));
        }
        let tree = join(&self.trees, task_id);
        let home = match record.get("home") {
            v if v.truthy() => crate::text_of(v),
            _ => join(&self.trees, &format!(".harness-home/{task_id}")),
        };
        self.make_dirs(&home).map_err(|e| crate::os(&e))?;
        if record.get("mode").as_str() == Some("clone") {
            let start = match self.branch_head(&record) {
                JValue::Str(t) => JValue::Str(t),
                _ => record.get("baseSha").clone(),
            };
            if !short_sha(&start) {
                return Err(Error::new(
                    Some(409),
                    "The task branch has no commit to continue from",
                ));
            }
            let start = start.as_str().unwrap_or("").to_string();
            self.run(&args(&["clone", "--quiet", "--shared", &repo, &tree]), None)
                .map_err(crate::raw)?;
            self.run(
                &hostile(&["checkout", "--quiet", "-B", &branch, &start]),
                Some(&tree),
            )
            .map_err(crate::raw)?;
        } else {
            self.run(&args(&["worktree", "add", &tree, &branch]), Some(&repo))
                .map_err(crate::raw)?;
        }
        // As in Node, a failure is undone the way this store makes trees (its own mode).
        let pins = self.prepare_tree(&repo, &tree, &home, pinned)?;
        let path = realpath(&tree).ok_or_else(|| {
            Error::new(
                None,
                format!("ENOENT: no such file or directory, realpath '{tree}'"),
            )
        })?;
        let kept = match &record {
            JValue::Obj(items) => JValue::Obj(
                items
                    .iter()
                    .filter(|(k, _)| !matches!(k.as_str(), "releasedAt" | "headSha" | "error"))
                    .cloned()
                    .collect(),
            ),
            other => other.clone(),
        };
        let rounds = match record.get("rounds").as_num() {
            Some(n) if n.fract() == 0.0 && n.is_finite() => n,
            _ => 1.0,
        } + 1.0;
        let at = JValue::Num((self.now)());
        self.write(with(
            &kept,
            vec![
                ("path", s(&path)),
                ("home", s(&home)),
                ("status", s("held")),
                ("pinned", JValue::Arr(pins.iter().map(|p| s(p)).collect())),
                ("epoch", s(&self.epoch)),
                ("claimedAt", at),
                ("rounds", JValue::Num(rounds)),
            ],
        ))
    }

    /// The tracked file names and the README at the task's base commit, from the source repository.
    pub fn project_snapshot(
        &self,
        task_id: &str,
        max_files: usize,
        max_readme_bytes: usize,
    ) -> Result<JValue, Error> {
        let _held = self.guard();
        let empty = || {
            JValue::obj([
                ("files", JValue::Arr(Vec::new())),
                ("truncated", JValue::Bool(false)),
                ("readme", JValue::Null),
            ])
        };
        let Some(record) = self.read(task_id)? else {
            return Ok(empty());
        };
        if !short_sha(record.get("baseSha")) {
            return Ok(empty());
        }
        let repo = record.get("repo").as_str().unwrap_or("").to_string();
        let base = record.get("baseSha").as_str().unwrap_or("").to_string();
        let listing = self
            .source_git(
                &repo,
                &["ls-tree", "-r", "--name-only", "-z", &base],
                RunOptions {
                    raw: true,
                    max_buffer: Some(CHANGE_MAX_BUFFER),
                    ..RunOptions::default()
                },
            )
            .unwrap_or_default();
        let names: Vec<&str> = listing.split('\0').filter(|n| !n.is_empty()).collect();
        let readme = names
            .iter()
            .find(|n| is_readme(n))
            .map_or(JValue::Null, |top| {
                let blob = format!("{base}:{top}");
                match self.source_git(
                    &repo,
                    &["cat-file", "blob", &blob],
                    RunOptions {
                        raw: true,
                        ..RunOptions::default()
                    },
                ) {
                    Ok(text) => {
                        let text = if text.len() > max_readme_bytes {
                            let cut = String::from_utf8_lossy(
                                text.as_bytes().get(..max_readme_bytes).unwrap_or(&[]),
                            )
                            .into_owned();
                            cut.strip_suffix('\u{FFFD}')
                                .map(str::to_string)
                                .unwrap_or(cut)
                        } else {
                            text
                        };
                        JValue::obj([("path", s(top)), ("text", JValue::Str(text))])
                    }
                    Err(_) => JValue::Null,
                }
            });
        Ok(JValue::obj([
            (
                "files",
                JValue::Arr(names.iter().take(max_files).map(|n| s(n)).collect()),
            ),
            ("truncated", JValue::Bool(names.len() > max_files)),
            ("readme", readme),
        ]))
    }

    fn tip(&self, repo: &str, branch: &str) -> Option<String> {
        let rev = format!("refs/heads/{branch}^{{commit}}");
        self.source_git(
            repo,
            &["rev-parse", "--verify", "--quiet", &rev],
            RunOptions::default(),
        )
        .ok()
        .filter(|t| !t.is_empty())
    }

    fn merge_checks(&self, task_id: &str, reviewed: &str) -> Result<Checked, JValue> {
        let record = match self.read(task_id) {
            Err(_) => return Err(no("invalid", "Invalid task id.")),
            Ok(None) => return Err(no("no_workspace", "The task holds no workspace record.")),
            Ok(Some(r)) => r,
        };
        let status = record.get("status");
        if status.as_str() != Some("released") {
            return Err(no(
                "not_released",
                &format!(
                    "The workspace is {}, so nothing is merged.",
                    crate::text_of(status)
                ),
            ));
        }
        if !full_sha(reviewed) {
            return Err(no("bad_sha", "A merge needs the full reviewed commit id."));
        }
        let base_sha = record.get("baseSha").as_str().unwrap_or("").to_string();
        if !full_sha(&base_sha) {
            return Err(no("no_base", "The task recorded no base commit."));
        }
        let Some(base) = record
            .get("baseBranch")
            .as_str()
            .filter(|b| branch_name_ok(b))
            .map(str::to_string)
        else {
            return Err(no(
                "no_base_branch",
                "The task did not fork from a named branch, so there is nothing to fast-forward.",
            ));
        };
        let branch = record.get("branch").as_str().unwrap_or("").to_string();
        if base == branch {
            return Err(no("no_base_branch", "The task branch is its own base."));
        }
        let repo = record.get("repo").as_str().unwrap_or("").to_string();
        if self.tip(&repo, &branch).as_deref() != Some(reviewed) {
            return Err(no(
                "head_moved",
                "The task branch is no longer at the reviewed commit, so it was not merged.",
            ));
        }
        if self.tip(&repo, &base).as_deref() != Some(base_sha.as_str()) {
            return Err(no(
                "base_moved",
                &format!("{base} has moved since the task forked from it, so the change was not merged. Review it again against the new base."),
            ));
        }
        if self
            .source_git(
                &repo,
                &["merge-base", "--is-ancestor", &base_sha, reviewed],
                RunOptions::default(),
            )
            .is_err()
        {
            return Err(no(
                "not_fast_forward",
                "The reviewed head does not build on the base, so it cannot be fast-forwarded.",
            ));
        }
        let listing = self
            .source_git(
                &repo,
                &["worktree", "list", "--porcelain"],
                RunOptions::default(),
            )
            .map_err(|_| {
                no(
                    "unreadable",
                    "The repository could not say where its branches are checked out.",
                )
            })?;
        let mut at: Option<&str> = None;
        let mut checked_out: Vec<&str> = Vec::new();
        let wanted = format!("branch refs/heads/{base}");
        for line in listing.split('\n') {
            if let Some(path) = line.strip_prefix("worktree ") {
                at = Some(path);
            } else if line == wanted {
                if let Some(path) = at {
                    checked_out.push(path);
                }
            }
        }
        if let Some(first) = checked_out.first() {
            return Err(no(
                "checked_out",
                &format!(
                    "{base} is checked out in {first}, so noevia did not move it: that would rewrite files in a working tree. To merge it yourself, run there: git merge --ff-only {reviewed} (the reviewed commit on {branch})."
                ),
            ));
        }
        Ok(Checked { record, base })
    }

    /// Every check of [`Workspaces::merge_verified`], nothing moved: `{ ok, baseBranch }` or
    /// `{ ok: false, code, reason }`.
    pub fn merge_preflight(&self, task_id: &str, reviewed: &str) -> JValue {
        let _held = self.guard();
        match self.merge_checks(task_id, reviewed) {
            Ok(c) => JValue::obj([("ok", JValue::Bool(true)), ("baseBranch", s(&c.base))]),
            Err(refusal) => refusal,
        }
    }

    /// Fast-forwards the task's base branch to the reviewed head with a compare-and-swap, only
    /// when every check holds. Never throws: `{ ok, baseBranch, from, to, recordFailed? }` or
    /// `{ ok: false, code, reason }`.
    pub fn merge_verified(&self, task_id: &str, reviewed: &str) -> JValue {
        let _held = self.guard();
        let Checked { record, base } = match self.merge_checks(task_id, reviewed) {
            Ok(c) => c,
            Err(refusal) => return refusal,
        };
        let repo = record.get("repo").as_str().unwrap_or("").to_string();
        let base_sha = record.get("baseSha").as_str().unwrap_or("").to_string();
        let message = format!(
            "noevia: fast-forward to reviewed task {}",
            crate::text_of(record.get("taskId"))
        );
        let target = format!("refs/heads/{base}");
        if self
            .source_git(
                &repo,
                &["update-ref", "-m", &message, &target, reviewed, &base_sha],
                RunOptions::default(),
            )
            .is_err()
        {
            let code = if self.tip(&repo, &base).as_deref() == Some(base_sha.as_str()) {
                "refused"
            } else {
                "base_moved"
            };
            return no(
                code,
                &format!("git refused to fast-forward {base}; nothing was merged."),
            );
        }
        if self.tip(&repo, &base).as_deref() != Some(reviewed) {
            return no(
                "refused",
                &format!("{base} did not end at the reviewed commit."),
            );
        }
        let merged = JValue::obj([
            ("ok", JValue::Bool(true)),
            ("baseBranch", s(&base)),
            ("from", s(&base_sha)),
            ("to", s(reviewed)),
        ]);
        let at = JValue::Num((self.now)());
        let note = JValue::obj([
            ("baseBranch", s(&base)),
            ("from", s(&base_sha)),
            ("to", s(reviewed)),
            ("at", at),
        ]);
        if self.write(with(&record, vec![("merged", note)])).is_err() {
            return with(&merged, vec![("recordFailed", JValue::Bool(true))]);
        }
        merged
    }

    /// What the verifier copies: `{ source, headSha, branch }` for a cleanly released task whose
    /// branch tip in the source is exactly `sha`, or a coded refusal.
    pub fn verify_checkout(&self, task_id: &str, sha: &str) -> Result<JValue, Error> {
        let _held = self.guard();
        let record = self
            .read(task_id)?
            .ok_or_else(|| coded("no_workspace", "The task holds no workspace record."))?;
        if !full_sha(sha) {
            return Err(coded("bad_sha", "Verification needs a full commit id."));
        }
        let status = record.get("status");
        if status.as_str() == Some("held") {
            return Err(coded(
                "held",
                "An agent still holds this workspace; release it before verifying.",
            ));
        }
        if status.as_str() != Some("released") {
            return Err(coded(
                "not_verifiable",
                &format!(
                    "The workspace is {}, so its branch is not verified.",
                    crate::text_of(status)
                ),
            ));
        }
        if self.branch_head(&record).as_str() != Some(sha) {
            return Err(coded(
                "stale_sha",
                "That commit is not the current head of the task branch.",
            ));
        }
        Ok(JValue::obj([
            ("source", record.get("repo").clone()),
            ("headSha", s(sha)),
            ("branch", record.get("branch").clone()),
        ]))
    }

    /// The task branch's tip in the source repository now, or `None`.
    pub fn branch_tip(&self, task_id: &str) -> Option<String> {
        let _held = self.guard();
        let record = self.read(task_id).ok().flatten()?;
        self.branch_head(&record).as_str().map(str::to_string)
    }
}
