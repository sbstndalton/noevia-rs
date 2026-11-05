use super::*;
fn coded(code: &'static str, message: impl Into<String>) -> Error {
    Error::Coded {
        code,
        message: message.into(),
    }
}
fn short_sha(value: &str) -> bool {
    (7..=64).contains(&value.len())
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn no(code: &str, reason: impl Into<String>) -> JValue {
    object(vec![
        ("ok", JValue::Bool(false)),
        ("code", string(code)),
        ("reason", string(reason)),
    ])
}
impl Store {
    fn diff_run(
        &self,
        repo: &Path,
        args: &[&str],
        cancelled: &AtomicBool,
    ) -> Result<String, Error> {
        let args = args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        git::run(
            &args,
            repo,
            &self.environment(),
            16 * 1024 * 1024,
            true,
            cancelled,
        )
        .map_err(|e| match e {
            git::Error::TooLarge => coded("too_large", "The change is too large to review."),
            _ => coded("unreadable", "The change could not be read."),
        })
    }
    pub fn change(
        &self,
        id: &str,
        max_files: usize,
        cancelled: &AtomicBool,
    ) -> Result<JValue, Error> {
        let record = self
            .read(id)?
            .ok_or_else(|| coded("no_workspace", "The task holds no workspace record."))?;
        if record.get("status").as_str() != Some("released") {
            return Err(coded(
                "not_released",
                "The workspace was not released cleanly, so its change was not read.",
            ));
        }
        let base = record
            .get("baseSha")
            .as_str()
            .filter(|s| short_sha(s))
            .ok_or_else(|| {
                coded(
                    "no_base",
                    "The repository had no base commit to compare against.",
                )
            })?;
        let head = record
            .get("headSha")
            .as_str()
            .filter(|s| short_sha(s))
            .ok_or_else(|| coded("no_head", "The task branch has no commit to review."))?;
        if base == head {
            return Err(coded(
                "no_change",
                "The task made no commits, so there is nothing to review.",
            ));
        }
        let repo = Path::new(field(&record, "repo")?);
        let mut flags = git::HOSTILE_OFF.to_vec();
        flags.extend([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-renames",
        ]);
        let mut args = flags.clone();
        args.extend(["--name-only", "-z", base, head, "--"]);
        let names = self.diff_run(repo, &args, cancelled)?;
        let names = names
            .split('\0')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        let mut args = flags;
        args.extend(["--unified=3", base, head, "--"]);
        let text = self.diff_run(repo, &args, cancelled)?;
        let starts = text
            .match_indices("diff --git ")
            .filter(|(i, _)| *i == 0 || text.as_bytes().get(i.saturating_sub(1)) == Some(&b'\n'))
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        if starts.len() != names.len() {
            return Err(coded("unreadable", "The change could not be read."));
        }
        let mut files = Vec::new();
        for (index, (start, name)) in starts.iter().zip(&names).take(max_files).enumerate() {
            let end = starts.get(index + 1).copied().unwrap_or(text.len());
            let patch = text
                .get(*start..end)
                .ok_or_else(|| coded("unreadable", "The change could not be read."))?;
            files.push(object(vec![
                ("path", string(*name)),
                ("patch", string(patch.strip_suffix('\n').unwrap_or(patch))),
            ]));
        }
        Ok(object(vec![
            ("baseSha", string(base)),
            ("headSha", string(head)),
            ("files", JValue::Arr(files)),
            ("truncated", JValue::Bool(names.len() > max_files)),
        ]))
    }
    pub fn project_snapshot(
        &self,
        id: &str,
        max_files: usize,
        max_readme_bytes: usize,
        cancelled: &AtomicBool,
    ) -> Result<JValue, Error> {
        let empty = || {
            object(vec![
                ("files", JValue::Arr(vec![])),
                ("truncated", JValue::Bool(false)),
                ("readme", JValue::Null),
            ])
        };
        let Some(record) = self.read(id)? else {
            return Ok(empty());
        };
        let Some(base) = record.get("baseSha").as_str().filter(|s| short_sha(s)) else {
            return Ok(empty());
        };
        let repo = Path::new(field(&record, "repo")?);
        let mut flags = git::HOSTILE_OFF.to_vec();
        flags.extend(["ls-tree", "-r", "--name-only", "-z", base]);
        let raw = self.diff_run(repo, &flags, cancelled).unwrap_or_default();
        let names = raw
            .split('\0')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        let mut readme = JValue::Null;
        if let Some(name) = names.iter().find(|name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "readme" | "readme.md" | "readme.markdown" | "readme.txt" | "readme.rst"
            )
        }) {
            let mut args = git::HOSTILE_OFF
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<Vec<_>>();
            args.extend(["cat-file".into(), "blob".into(), format!("{base}:{name}")]);
            if let Ok(mut text) = git::run(
                &args,
                repo,
                &self.environment(),
                1024 * 1024,
                true,
                cancelled,
            ) {
                if text.len() > max_readme_bytes {
                    text = String::from_utf8_lossy(
                        text.as_bytes()
                            .get(..max_readme_bytes)
                            .ok_or(Error::Unavailable)?,
                    )
                    .into_owned();
                    if text.ends_with('\u{fffd}') {
                        text.pop();
                    }
                }
                readme = object(vec![("path", string(*name)), ("text", string(text))]);
            }
        }
        Ok(object(vec![
            (
                "files",
                JValue::Arr(names.iter().take(max_files).map(|s| string(*s)).collect()),
            ),
            ("truncated", JValue::Bool(names.len() > max_files)),
            ("readme", readme),
        ]))
    }
    pub fn verify_checkout(
        &self,
        id: &str,
        commit: &str,
        cancelled: &AtomicBool,
    ) -> Result<JValue, Error> {
        let record = self
            .read(id)?
            .ok_or_else(|| coded("no_workspace", "The task holds no workspace record."))?;
        if !full_sha(commit) {
            return Err(coded("bad_sha", "Verification needs a full commit id."));
        }
        if record.get("status").as_str() == Some("held") {
            return Err(coded(
                "held",
                "An agent still holds this workspace; release it before verifying.",
            ));
        }
        if record.get("status").as_str() != Some("released") {
            return Err(coded(
                "not_verifiable",
                format!(
                    "The workspace is {}, so its branch is not verified.",
                    record.get("status").as_str().unwrap_or("undefined")
                ),
            ));
        }
        if self.branch_head(&record, cancelled).as_deref() != Some(commit) {
            return Err(coded(
                "stale_sha",
                "That commit is not the current head of the task branch.",
            ));
        }
        Ok(object(vec![
            ("source", record.get("repo").clone()),
            ("headSha", string(commit)),
            ("branch", record.get("branch").clone()),
        ]))
    }
    fn tip(&self, record: &JValue, branch: &str, cancelled: &AtomicBool) -> Option<String> {
        self.safe(
            Path::new(record.get("repo").as_str()?),
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}^{{commit}}"),
            ],
            cancelled,
        )
        .ok()
        .filter(|s| !s.is_empty())
    }
    fn merge_checks(
        &self,
        id: &str,
        reviewed: &str,
        cancelled: &AtomicBool,
    ) -> Result<(JValue, String), JValue> {
        let record = self
            .read(id)
            .map_err(|_| no("invalid", "Invalid task id."))?
            .ok_or_else(|| no("no_workspace", "The task holds no workspace record."))?;
        if record.get("status").as_str() != Some("released") {
            return Err(no(
                "not_released",
                format!(
                    "The workspace is {}, so nothing is merged.",
                    record.get("status").as_str().unwrap_or("undefined")
                ),
            ));
        }
        if !full_sha(reviewed) {
            return Err(no("bad_sha", "A merge needs the full reviewed commit id."));
        }
        let base_sha = record
            .get("baseSha")
            .as_str()
            .filter(|s| full_sha(s))
            .ok_or_else(|| no("no_base", "The task recorded no base commit."))?;
        let base=record.get("baseBranch").as_str().filter(|s|valid_branch(s)).ok_or_else(||no("no_base_branch","The task did not fork from a named branch, so there is nothing to fast-forward."))?;
        let branch = record.get("branch").as_str().unwrap_or("undefined");
        if base == branch {
            return Err(no("no_base_branch", "The task branch is its own base."));
        }
        if self.tip(&record, branch, cancelled).as_deref() != Some(reviewed) {
            return Err(no(
                "head_moved",
                "The task branch is no longer at the reviewed commit, so it was not merged.",
            ));
        }
        if self.tip(&record, base, cancelled).as_deref() != Some(base_sha) {
            return Err(no("base_moved",format!("{base} has moved since the task forked from it, so the change was not merged. Review it again against the new base.")));
        }
        let repo = Path::new(record.get("repo").as_str().ok_or_else(|| {
            no(
                "unreadable",
                "The repository could not say where its branches are checked out.",
            )
        })?);
        if self
            .safe(
                repo,
                &["merge-base", "--is-ancestor", base_sha, reviewed],
                cancelled,
            )
            .is_err()
        {
            return Err(no(
                "not_fast_forward",
                "The reviewed head does not build on the base, so it cannot be fast-forwarded.",
            ));
        }
        let listing = self
            .safe(repo, &["worktree", "list", "--porcelain"], cancelled)
            .map_err(|_| {
                no(
                    "unreadable",
                    "The repository could not say where its branches are checked out.",
                )
            })?;
        let mut at = None;
        for line in listing.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                at = Some(path);
            } else if line == format!("branch refs/heads/{base}") {
                if let Some(path) = at {
                    return Err(no("checked_out",format!("{base} is checked out in {path}, so noevia did not move it: that would rewrite files in a working tree. To merge it yourself, run there: git merge --ff-only {reviewed} (the reviewed commit on {branch}).")));
                }
            }
        }
        let base = base.to_owned();
        Ok((record, base))
    }
    pub fn merge_preflight(&self, id: &str, reviewed: &str) -> JValue {
        let Ok(_transaction) = TRANSACTION.lock() else {
            return no(
                "unreadable",
                "The repository could not say where its branches are checked out.",
            );
        };
        let cancelled = AtomicBool::new(false);
        match self.merge_checks(id, reviewed, &cancelled) {
            Ok((_, base)) => object(vec![
                ("ok", JValue::Bool(true)),
                ("baseBranch", string(base)),
            ]),
            Err(reply) => reply,
        }
    }
    /// The coordinator treats this as a synchronous commit: cancellation acknowledgement waits
    /// until its CAS result is known, just as the original synchronous Git operation did.
    pub fn merge_verified(&self, id: &str, reviewed: &str, now: i64) -> JValue {
        let Ok(_transaction) = TRANSACTION.lock() else {
            return no(
                "unreadable",
                "The repository could not say where its branches are checked out.",
            );
        };
        let cancelled = AtomicBool::new(false);
        let (mut record, base) = match self.merge_checks(id, reviewed, &cancelled) {
            Ok(value) => value,
            Err(reply) => return reply,
        };
        let Some(repo) = record.get("repo").as_str().map(PathBuf::from) else {
            return no(
                "unreadable",
                "The repository could not say where its branches are checked out.",
            );
        };
        let Some(original) = record.get("baseSha").as_str().map(str::to_owned) else {
            return no("no_base", "The task recorded no base commit.");
        };
        if self
            .safe(
                &repo,
                &[
                    "update-ref",
                    "-m",
                    &format!("noevia: fast-forward to reviewed task {id}"),
                    &format!("refs/heads/{base}"),
                    reviewed,
                    &original,
                ],
                &cancelled,
            )
            .is_err()
        {
            return no(
                if self.tip(&record, &base, &cancelled).as_deref() == Some(original.as_str()) {
                    "refused"
                } else {
                    "base_moved"
                },
                format!("git refused to fast-forward {base}; nothing was merged."),
            );
        }
        if self.tip(&record, &base, &cancelled).as_deref() != Some(reviewed) {
            return no(
                "refused",
                format!("{base} did not end at the reviewed commit."),
            );
        }
        let mut reply = object(vec![
            ("ok", JValue::Bool(true)),
            ("baseBranch", string(base.clone())),
            ("from", string(original.clone())),
            ("to", string(reviewed)),
        ]);
        set(
            &mut record,
            "merged",
            object(vec![
                ("baseBranch", string(base)),
                ("from", string(original)),
                ("to", string(reviewed)),
                ("at", JValue::Num(now as f64)),
            ]),
        );
        if self.write(&record).is_err() {
            set(&mut reply, "recordFailed", JValue::Bool(true));
        }
        reply
    }
}

impl Store {
    pub fn reclaim(
        &self,
        id: &str,
        pinned: &[String],
        now: i64,
        cancelled: &AtomicBool,
    ) -> Result<JValue, Error> {
        use std::os::unix::fs::PermissionsExt;
        let _transaction = TRANSACTION.lock().map_err(|_| Error::Unavailable)?;
        self.file(id)?;
        let mut record = self
            .read(id)?
            .ok_or_else(|| refused(404, "This task holds no workspace to take again"))?;
        if record.get("status").as_str() != Some("released") {
            return Err(refused(
                409,
                format!(
                    "This task's workspace is {}, so it cannot be taken again",
                    record.get("status").as_str().unwrap_or("undefined")
                ),
            ));
        }
        let repo = PathBuf::from(field(&record, "repo")?);
        let branch = field(&record, "branch")?.to_owned();
        for held in self.list()? {
            if held.get("taskId").as_str() != Some(id)
                && held.get("status").as_str() != Some("released")
                && held.get("repo") == record.get("repo")
                && held.get("branch") == record.get("branch")
            {
                return Err(refused(
                    409,
                    format!(
                        "Another task already writes {branch} in this repository ({})",
                        held.get("status").as_str().unwrap_or("undefined")
                    ),
                ));
            }
        }
        let tree = self.trees.join(id);
        let home = record
            .get("home")
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.trees.join(".harness-home").join(id));
        if fs::symlink_metadata(&tree).is_ok() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "workspace path already exists",
            )));
        }
        mkdir(
            &self.trees,
            if self.trees == self.root {
                0o700
            } else {
                0o755
            },
        )?;
        let parent = home.parent().ok_or(Error::Unavailable)?;
        mkdir(parent, 0o755)?;
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o755));
        mkdir(&home, 0o700)?;
        let clone = record.get("mode").as_str() == Some("clone");
        let mut undo = Undo {
            store: self,
            repo: &repo,
            tree: &tree,
            home: &home,
            clone,
            armed: true,
        };
        if clone {
            let start = self
                .branch_head(&record, cancelled)
                .or_else(|| record.get("baseSha").as_str().map(str::to_owned))
                .filter(|s| short_sha(s))
                .ok_or_else(|| refused(409, "The task branch has no commit to continue from"))?;
            self.run(
                &std::env::current_dir()?,
                &[
                    "clone",
                    "--quiet",
                    "--shared",
                    &repo.to_string_lossy(),
                    &tree.to_string_lossy(),
                ],
                cancelled,
            )?;
            self.safe(
                &tree,
                &["checkout", "--quiet", "-B", &branch, &start],
                cancelled,
            )?;
        } else {
            self.run(
                &repo,
                &["worktree", "add", &tree.to_string_lossy(), &branch],
                cancelled,
            )?;
        }
        let pins = self.prepare_tree(&tree, &home, pinned, cancelled)?;
        if let JValue::Obj(fields) = &mut record {
            fields.retain(|(key, _)| !matches!(key.as_str(), "releasedAt" | "headSha" | "error"));
        }
        let rounds = match record.get("rounds") {
            JValue::Num(n) if n.is_finite() && n.fract() == 0.0 => *n + 1.0,
            _ => 2.0,
        };
        set(
            &mut record,
            "path",
            string(fs::canonicalize(&tree)?.to_string_lossy()),
        );
        set(&mut record, "home", string(home.to_string_lossy()));
        set(&mut record, "status", string("held"));
        set(
            &mut record,
            "pinned",
            JValue::Arr(pins.into_iter().map(string).collect()),
        );
        set(&mut record, "epoch", string(self.epoch.clone()));
        set(&mut record, "claimedAt", JValue::Num(now as f64));
        set(&mut record, "rounds", JValue::Num(rounds));
        self.write(&record)?;
        undo.armed = false;
        Ok(record)
    }
}
