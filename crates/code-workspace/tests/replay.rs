//! crates/code-workspace: every operation core's own code-workspace.cjs ran in
//! tests/fixtures/node.v1.json (tools/gen-fixtures.cjs: throwaway QA repositories, fixed commit
//! dates, a ticking clock), replayed through the Rust port with the same git in a fresh directory.
//! Each answer and the snapshot after every step (records, trust file, source refs and worktrees,
//! exclude file, the trees on disk, directory modes, whether a planted command ran) must match.
//! Where the port differs on purpose, the fixture shows Node's defect and the Rust answer is
//! checked against its own expectation here. A scenario whose answer depends on the version of git
//! (`empty-repo`) is compared with Node's recorded answer only when the git running the test does
//! what the recording git did; otherwise only what the port must hold either way is asserted.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use code_workspace::git::Git;
use code_workspace::{chown_tree, seal_tree, Claim, Error, Ids, Options, Workspaces};
use js_json::{wtf16, JValue};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/node.v1.json");
const DATE: &str = "2026-01-01T00:00:00Z";

fn fixture() -> JValue {
    wtf16::parse(&fs::read_to_string(FIXTURE).unwrap()).unwrap()
}

fn text(v: &JValue) -> String {
    wtf16::stringify(v).unwrap()
}

/// The generator's `norm`: the base directory, uid/gid numbers, and git's words after
/// `Command failed: <command>\n`.
fn norm(base: &str, t: &str) -> String {
    let mut t = t.replace(base, "<BASE>");
    for key in ["uid", "gid"] {
        let pat = format!("\"{key}\":");
        let mut out = String::new();
        let mut rest = t.as_str();
        while let Some(i) = rest.find(&pat) {
            out.push_str(&rest[..i + pat.len()]);
            rest = &rest[i + pat.len()..];
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            if digits > 0 {
                out.push_str(&format!("\"<{key}>\""));
                rest = &rest[digits..];
            }
        }
        out.push_str(rest);
        t = out;
    }
    if let Some(i) = t.find("Command failed: ") {
        if let Some(nl) = t[i..].find('\n') {
            t = format!("{}\n<stderr>", &t[..i + nl]);
        }
    }
    t
}

/// The script's own hermetic git (setting up repositories, acting as the harness).
fn shell(args: &[&str], cwd: &Path) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", "/nonexistent")
        .env("GIT_AUTHOR_DATE", DATE)
        .env("GIT_COMMITTER_DATE", DATE)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "QA")
        .env("GIT_AUTHOR_EMAIL", "qa@example.invalid")
        .env("GIT_COMMITTER_NAME", "QA")
        .env("GIT_COMMITTER_EMAIL", "qa@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The generator's `walk`: names as Node reads them (a name that is not UTF-8 has its U+FFFD
/// spelling, which names nothing on disk, so what is inside such a directory is not listed), and
/// listed down to ten levels.
fn walk(base: &Path, rel: &str) -> Vec<JValue> {
    let mut out = Vec::new();
    if rel.split('/').count() > 10 {
        return out;
    }
    let Ok(entries) = fs::read_dir(base.join(rel)) else {
        return out;
    };
    let mut entries: Vec<(String, fs::DirEntry)> = entries
        .flatten()
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e))
        .collect();
    entries.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    for (name, e) in entries {
        let r = format!("{rel}/{name}");
        let kind = e.file_type().unwrap();
        if name == ".git" {
            out.push(JValue::Str(format!("{r}/")));
        } else if kind.is_symlink() {
            out.push(JValue::Str(format!("{r} -> link")));
        } else if kind.is_dir() {
            out.push(JValue::Str(format!("{r}/")));
            out.extend(walk(base, &r));
        } else {
            out.push(JValue::Str(r));
        }
    }
    out
}

fn mode(p: &Path) -> JValue {
    fs::metadata(p).map_or(JValue::Null, |m| {
        JValue::Str(format!("{:o}", m.mode() & 0o777))
    })
}

fn snapshot(base: &Path, repos: &[String]) -> JValue {
    let b = base.to_str().unwrap();
    let root = base.join("tenant/code-workspaces");
    let mut records = Vec::new();
    if let Ok(entries) = fs::read_dir(&root) {
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        names.sort();
        for n in names {
            let p = root.join(&n);
            if fs::symlink_metadata(&p).unwrap().is_file() {
                records.push((n, JValue::Str(norm(b, &fs::read_to_string(&p).unwrap()))));
            }
        }
    }
    let mut git = Vec::new();
    for r in repos {
        let repo = base.join("repos").join(r);
        let exclude =
            fs::read_to_string(repo.join(".git/info/exclude")).map_or(JValue::Null, JValue::Str);
        git.push((
            r.clone(),
            JValue::obj([
                (
                    "refs",
                    JValue::Str(norm(
                        b,
                        &shell(
                            &["for-each-ref", "--format=%(refname) %(objectname)"],
                            &repo,
                        ),
                    )),
                ),
                (
                    "worktrees",
                    JValue::Str(norm(b, &shell(&["worktree", "list", "--porcelain"], &repo))),
                ),
                ("exclude", exclude),
            ]),
        ));
    }
    let homes = if base.join("trees").exists() {
        base.join("trees")
    } else {
        root.clone()
    }
    .join(".harness-home");
    JValue::obj([
        ("records", JValue::Obj(records)),
        ("git", JValue::Obj(git)),
        ("tenant", JValue::Arr(walk(base, "tenant"))),
        ("trees", JValue::Arr(walk(base, "trees"))),
        ("outside", JValue::Arr(walk(base, "outside"))),
        (
            "modes",
            JValue::obj([("root", mode(&root)), ("homes", mode(&homes))]),
        ),
        ("pwned", JValue::Bool(base.join("pwned").exists())),
    ])
}

fn outcome(base: &str, r: Result<JValue, Error>) -> JValue {
    match r {
        Ok(v) => JValue::obj([("ok", wtf16::parse(&norm(base, &text(&v))).unwrap())]),
        Err(e) => JValue::obj([(
            "error",
            JValue::obj([
                (
                    "status",
                    e.status.map_or(JValue::Null, |s| JValue::Num(f64::from(s))),
                ),
                (
                    "code",
                    e.code.map_or(JValue::Null, |c| JValue::Str(c.to_string())),
                ),
                ("link", JValue::Null),
                ("message", JValue::Str(norm(base, &e.message))),
            ]),
        )]),
    }
}

struct Run {
    _dir: tempfile::TempDir,
    base: std::path::PathBuf,
    repos: Vec<String>,
    stores: BTreeMap<String, Workspaces>,
    clock: Arc<AtomicU64>,
    me: Ids,
}

impl Run {
    fn new() -> Run {
        // The recording ran under this umask (the generator sets it).
        rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o022));
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let meta = fs::metadata(&base).unwrap();
        Run {
            _dir: dir,
            base,
            repos: Vec::new(),
            stores: BTreeMap::new(),
            clock: Arc::new(AtomicU64::new(0)),
            me: Ids {
                uid: meta.uid(),
                gid: meta.gid(),
            },
        }
    }

    /// `<base>/<p>`, `<FF>` being the byte 0xff (a name that is not UTF-8).
    fn at(&self, p: &str) -> std::path::PathBuf {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let mut bytes = self.base.as_os_str().as_bytes().to_vec();
        bytes.push(b'/');
        for (i, part) in p.split("<FF>").enumerate() {
            if i > 0 {
                bytes.push(0xff);
            }
            bytes.extend_from_slice(part.as_bytes());
        }
        std::path::PathBuf::from(std::ffi::OsString::from_vec(bytes))
    }

    fn b(&self) -> String {
        self.base.to_str().unwrap().to_string()
    }

    fn sub(&self, t: &str) -> String {
        t.replace("<BASE>", &self.b())
    }

    /// `{tip:<branch>}`: that branch's tip in the first source repository, read now.
    fn sha(&self, v: &str) -> String {
        match v.strip_prefix("{tip:").and_then(|r| r.strip_suffix('}')) {
            Some(branch) => {
                let repo = self.at(&format!("repos/{}", self.repos[0]));
                shell(&["rev-parse", &format!("refs/heads/{branch}")], &repo)
                    .trim()
                    .to_string()
            }
            None => v.to_string(),
        }
    }

    fn step(&mut self, s: &JValue) -> Option<JValue> {
        let op = s.get("op").as_str().unwrap();
        let str_of = |k: &str| s.get(k).as_str().unwrap_or("").to_string();
        let store = str_of("ws");
        let task = str_of("taskId");
        let r = match op {
            "repo" => {
                let name = str_of("name");
                let repo = self.at(&format!("repos/{name}"));
                fs::create_dir_all(&repo).unwrap();
                shell(&["init", "-q", "--template=", "-b", "main"], &repo);
                if let JValue::Obj(files) = s.get("files") {
                    for (n, body) in files {
                        fs::write(repo.join(n), body.as_str().unwrap()).unwrap();
                    }
                }
                if s.get("empty").as_bool() != Some(true) {
                    shell(&["add", "."], &repo);
                    shell(&["commit", "-qm", "QA first"], &repo);
                }
                self.repos.push(name);
                return None;
            }
            "dir" => {
                let p = self.at(&str_of("path"));
                fs::create_dir_all(&p).unwrap();
                if let Some(m) = s.get("mode").as_num() {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&p, fs::Permissions::from_mode(m as u32)).unwrap();
                }
                return None;
            }
            "write" => {
                let p = self.at(&str_of("path"));
                fs::write(&p, self.sub(&str_of("text"))).unwrap();
                if let Some(m) = s.get("mode").as_num() {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&p, fs::Permissions::from_mode(m as u32)).unwrap();
                }
                return None;
            }
            "rm" => {
                let p = self.at(&str_of("path"));
                if s.get("recursive").as_bool() == Some(true) {
                    let _ = fs::remove_dir_all(&p);
                } else {
                    let _ = fs::remove_file(&p);
                }
                return None;
            }
            "link" => {
                fs::hard_link(self.at(&str_of("target")), self.at(&str_of("path"))).unwrap();
                return None;
            }
            "mode" => Ok(
                fs::metadata(self.at(&str_of("path"))).map_or(JValue::Null, |m| {
                    JValue::Str(format!("{:o}", m.mode() & 0o7777))
                }),
            ),
            "symlink" => {
                std::os::unix::fs::symlink(self.sub(&str_of("target")), self.at(&str_of("path")))
                    .unwrap();
                return None;
            }
            "git" => {
                let JValue::Arr(a) = s.get("args") else {
                    panic!()
                };
                let args: Vec<String> = a.iter().map(|x| self.sub(x.as_str().unwrap())).collect();
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                shell(&refs, &self.at(&str_of("cwd")));
                return None;
            }
            "clock" => {
                self.clock
                    .store(s.get("t").as_num().unwrap() as u64, Ordering::SeqCst);
                return None;
            }
            "ws" => {
                let clock = self.clock.clone();
                let options = Options {
                    dir: self.at("tenant").to_str().unwrap().to_string(),
                    tree_root: s
                        .get("treeRoot")
                        .as_str()
                        .map(|t| self.at(t).to_str().unwrap().to_string()),
                    owner: (s.get("owner").as_str() == Some("self")).then_some(self.me),
                    noevia: Some(self.me),
                    mode: None,
                    epoch: str_of("epoch"),
                    change_max_buffer: s.get("changeMaxBuffer").as_num().map(|n| n as usize),
                };
                let git = Git {
                    extra_env: vec![
                        ("GIT_AUTHOR_DATE".into(), DATE.into()),
                        ("GIT_COMMITTER_DATE".into(), DATE.into()),
                    ],
                    ..Git::default()
                };
                let ws = Workspaces::with(
                    options,
                    Box::new(move || clock.fetch_add(1, Ordering::SeqCst) as f64),
                    git,
                    Box::new(chown_tree),
                );
                self.stores.insert(str_of("name"), ws);
                return None;
            }
            "claim" => {
                let strings = |k: &str| -> Vec<String> {
                    match s.get(k) {
                        JValue::Arr(a) => {
                            a.iter().map(|x| x.as_str().unwrap().to_string()).collect()
                        }
                        _ => Vec::new(),
                    }
                };
                let caps = strings("capabilities");
                let domains = strings("domains");
                let pinned = match s.get("pinned") {
                    JValue::Arr(a) => a.clone(),
                    _ => Vec::new(),
                };
                let repo = self.at(&str_of("repo")).to_str().unwrap().to_string();
                let branch = s.get("branch").as_str().map(str::to_string);
                self.stores[&store].claim(Claim {
                    task_id: &task,
                    repo_path: &repo,
                    branch: branch.as_deref(),
                    capabilities: &caps,
                    domains: &domains,
                    pinned: &pinned,
                })
            }
            "release" => {
                self.stores[&store].release(&task, s.get("removeBranch").as_bool() == Some(true))
            }
            "recover" => self.stores[&store].recover().map(JValue::Arr),
            "contains" => self.stores[&store]
                .contains(&task, &self.sub(&str_of("candidate")))
                .map(|r| r.map_or(JValue::Null, JValue::Bool)),
            "headSha" => self.stores[&store]
                .head_sha(&task)
                .map(|r| r.map_or(JValue::Null, JValue::Str)),
            "get" => self.stores[&store].get(&task),
            "list" => Ok(JValue::Arr(self.stores[&store].list())),
            "change" => {
                let max = s.get("maxFiles").as_num().map_or(200, |n| n as usize);
                self.stores[&store].change(&task, max)
            }
            "reclaim" => {
                let pinned = match s.get("pinned") {
                    JValue::Arr(a) => a.clone(),
                    _ => Vec::new(),
                };
                self.stores[&store].reclaim(&task, &pinned)
            }
            "projectSnapshot" => {
                let files = s.get("maxFiles").as_num().map_or(400, |n| n as usize);
                let readme = s
                    .get("maxReadmeBytes")
                    .as_num()
                    .map_or(8 * 1024, |n| n as usize);
                self.stores[&store].project_snapshot(&task, files, readme)
            }
            "mergePreflight" => {
                Ok(self.stores[&store].merge_preflight(&task, &self.sha(&str_of("sha"))))
            }
            "mergeVerified" => {
                Ok(self.stores[&store].merge_verified(&task, &self.sha(&str_of("sha"))))
            }
            "verifyCheckout" => {
                self.stores[&store].verify_checkout(&task, &self.sha(&str_of("sha")))
            }
            "branchTip" => Ok(self.stores[&store]
                .branch_tip(&task)
                .map_or(JValue::Null, JValue::Str)),
            other => panic!("unknown op {other}"),
        };
        Some(outcome(&self.b(), r))
    }
}

fn steps(f: &JValue, name: &str) -> Vec<JValue> {
    match f.get("scenarios").get(name) {
        JValue::Arr(a) => a.clone(),
        _ => panic!("no scenario {name}"),
    }
}

#[test]
fn fixture_records_its_source() {
    let f = fixture();
    // noevia-core 0c294a3 (#367); a fixture regenerated from another source fails here until
    // updated on purpose.
    assert_eq!(
        f.get("sources").get("code-workspace.cjs").as_str(),
        Some("6241fc912ab6ae35be20c2456d47aa48aa68930250163a61c0012e17706bf632")
    );
}

/// Whether the git running here makes a worktree on a branch that has no commit yet (git 2.39
/// refuses with exit code 255; newer git makes it). Node's own answer for `empty-repo` is whatever
/// the recording git did.
fn git_makes_worktree_on_unborn_branch() -> bool {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    shell(&["init", "-q", "--template=", "-b", "main"], &repo);
    Command::new("git")
        .args(["worktree", "add", "-B", "qa-probe"])
        .arg(dir.path().join("wt"))
        .current_dir(&repo)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", "/nonexistent")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap()
        .status
        .success()
}

/// Node's error `status` (git's exit code) is not the port's (a 500 has none): see the crate doc.
fn without_error_status(t: &str) -> String {
    let pat = r#""error":{"status":"#;
    match t.find(pat) {
        Some(i) => {
            let from = i + pat.len();
            let digits = t[from..].chars().take_while(char::is_ascii_digit).count();
            format!("{}null{}", &t[..from], &t[from + digits..])
        }
        None => t.to_string(),
    }
}

#[test]
fn every_node_scenario_replays() {
    let f = fixture();
    let JValue::Obj(all) = f.get("scenarios") else {
        panic!()
    };
    let mut seen = 0;
    for (name, _) in all {
        if name == "differences" {
            continue;
        }
        let script = steps(&f, name);
        // The claim of an unborn branch: git decides. Compared with Node's recorded answer only
        // when this git does what the recording git did; a recorded refusal differs from the
        // port's answer only in the error's status (the crate doc, difference 2).
        let mut refused_by_recording = false;
        if name == "empty-repo" {
            let claim = script
                .iter()
                .find(|s| s.get("op").as_str() == Some("claim"));
            refused_by_recording = claim.is_some_and(|c| c.get("result").get("error").is_object());
            if refused_by_recording == git_makes_worktree_on_unborn_branch() {
                eprintln!(
                    "empty-repo: this git ({}) answers an unborn branch differently from the recording ({}); asserting the port's own answer only",
                    if refused_by_recording { "makes it" } else { "refuses it" },
                    text(f.get("git"))
                );
                empty_repo_without_the_recorded_answer(&script);
                seen += 1;
                continue;
            }
        }
        let mut run = Run::new();
        let mut last = JValue::Null;
        for (i, s) in script.iter().enumerate() {
            if let Some(got) = run.step(s) {
                let want = text(s.get("result"));
                let want = if refused_by_recording {
                    without_error_status(&want)
                } else {
                    want
                };
                assert_eq!(text(&got), want, "{name} step {i}: {}", text(s));
            }
            let want = match s.get("snapshot") {
                JValue::Str(same) if same == "same" => last.clone(),
                other => other.clone(),
            };
            let got = snapshot(&run.base, &run.repos);
            assert_eq!(
                text(&got),
                text(&want),
                "{name} step {i} snapshot: {}",
                text(s)
            );
            last = want;
        }
        seen += 1;
    }
    assert_eq!(seen, 10);
}

/// What the port must do with an empty repository whatever git does with its unborn branch. The
/// answer is chosen by probing this git, not by reading the claim's own output: if this git makes a
/// worktree on an unborn branch, the claim must hold one with `baseSha: null`, the release must say
/// `released`, and no tree (or home) may be left; if it refuses, the claim must be git's own refusal
/// and leave no record, tree or home behind.
fn empty_repo_without_the_recorded_answer(script: &[JValue]) {
    let makes_it = git_makes_worktree_on_unborn_branch();
    let mut run = Run::new();
    let mut claimed = false;
    for s in script {
        let Some(got) = run.step(s) else { continue };
        match s.get("op").as_str() {
            Some("claim") if makes_it => {
                assert_eq!(
                    got.get("ok").get("baseSha"),
                    &JValue::Null,
                    "{}",
                    text(&got)
                );
                assert_eq!(
                    got.get("ok").get("status").as_str(),
                    Some("held"),
                    "{}",
                    text(&got)
                );
                claimed = true;
            }
            Some("claim") => {
                let t = text(&got);
                assert!(
                    got.get("error").is_object()
                        && got
                            .get("error")
                            .get("message")
                            .as_str()
                            .is_some_and(|m| m
                                .starts_with("Command failed: git worktree add -B noevia/task-")),
                    "{t}"
                );
            }
            Some("headSha") => assert_eq!(text(&got), r#"{"ok":null}"#),
            Some("release") if makes_it => {
                assert!(claimed);
                assert_eq!(
                    got.get("ok").get("status").as_str(),
                    Some("released"),
                    "{}",
                    text(&got)
                );
            }
            _ => {}
        }
    }
    let snap = snapshot(&run.base, &run.repos);
    let id = "00000000-0000-4000-8000-000000000001";
    // Released, or never made: no tree is left, and the home (made before git is asked, and kept
    // by Node too) holds nothing.
    let tree = run.base.join("tenant/code-workspaces").join(id);
    assert!(!tree.exists(), "the tree was left: {}", text(&snap));
    let home = run
        .base
        .join("tenant/code-workspaces/.harness-home")
        .join(id);
    assert!(
        !home.exists() || fs::read_dir(&home).unwrap().next().is_none(),
        "the home holds something: {}",
        text(&snap)
    );
    let JValue::Obj(records) = snap.get("records") else {
        panic!("{}", text(&snap))
    };
    let kept = records.iter().filter(|(n, _)| n.ends_with(".json")).count();
    assert_eq!(kept, usize::from(makes_it), "{}", text(&snap));
}

#[test]
fn differences_from_node() {
    let f = fixture();
    let all = steps(&f, "differences");
    let node: Vec<String> = all
        .iter()
        .filter(|s| !matches!(s.get("result"), JValue::Undefined))
        .map(|s| text(s.get("result")))
        .collect();
    // Node: git's exit code as the error's status; a second clone on a held branch; an empty list.
    assert!(node[0].contains(r#""status":128"#), "{}", node[0]);
    assert!(node[2].starts_with(r#"{"ok":{"#), "{}", node[2]);
    assert_eq!(node[3], r#"{"ok":[]}"#);
    // Node's frozen copy of the config was written into the directory the `.git` link names, and
    // renamed over the file in it: mode 0644 where the harness left 0600.
    assert_eq!(node[6], r#"{"ok":"644"}"#);
    // Node's release of a partial clone with none of its objects went through and committed the
    // harness's change: `git status` fetched them from the remote the config named.
    assert!(node[8].contains(r#""status":"released""#), "{}", node[8]);
    assert!(node[8].contains(r#""headSha":""#), "{}", node[8]);

    let mut run = Run::new();
    let mut got = Vec::new();
    for s in &all {
        if let Some(a) = run.step(s) {
            got.push(text(&a));
        }
    }
    assert!(got[0].starts_with(r#"{"error":{"status":null,"code":null,"link":null,"message":"Command failed: git worktree add -B main "#), "{}", got[0]);
    assert_eq!(got[1], node[1]);
    assert_eq!(
        got[2],
        r#"{"error":{"status":409,"code":null,"link":null,"message":"Another task already writes qa/x in this repository (held)"}}"#
    );
    assert!(
        got[3].contains(r#""taskId":"00000000-0000-4000-8000-000000000002""#),
        "{}",
        got[3]
    );
    // The same refusal, and nothing written through the link.
    assert!(
        node[5].contains("the clone\u{2019}s .git is not a plain directory"),
        "{}",
        node[5]
    );
    // (The clocks differ: Node's claim of the second clone on the branch ticked one more.)
    let refusal = |t: &str| t[t.find("\"error\":").unwrap()..].to_string();
    assert_eq!(refusal(&got[5]), refusal(&node[5]));
    assert_eq!(got[6], r#"{"ok":"600"}"#);
    // The port refuses the partial clone's config before any git call is made on the tree, and
    // keeps the tree: nothing was fetched from the remote it names.
    assert!(
        got[8].contains(r#""status":"stuck""#)
            && got[8].contains("its .git/config sets ")
            && got[8].contains("extensions.partialclone")
            && got[8].contains("remote.origin.promisor"),
        "{}",
        got[8]
    );
    assert!(run
        .base
        .join("trees/00000000-0000-4000-8000-000000000005/.git")
        .exists());
    assert!(!run.base.join("pwned").exists(), "a planted command ran");
}

#[test]
fn no_planted_command_ever_runs_as_noevia() {
    // Replayed through the port: after every step noevia takes in the hostile scenarios, nothing
    // planted has run.
    let f = fixture();
    for name in ["hostile-clone", "refusals", "differences"] {
        let mut run = Run::new();
        for (i, s) in steps(&f, name).iter().enumerate() {
            let harness = matches!(s.get("op").as_str(), Some("git" | "write" | "rm"));
            run.step(s);
            if harness {
                // The harness's own git may run what it planted, as the harness; not noevia.
                let _ = fs::remove_file(run.base.join("pwned"));
            } else {
                assert!(!run.base.join("pwned").exists(), "{name} step {i}");
            }
        }
    }
}

/// A `git` that logs its working directory and arguments, then runs the real one.
fn logging_git(dir: &Path) -> (Git, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("git.log");
    let script = dir.join("qa-git");
    let real = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap()
        .stdout;
    let real = String::from_utf8(real).unwrap();
    fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$(pwd) | $*\" >> '{}'\necho \"GIT_NO_LAZY_FETCH=$GIT_NO_LAZY_FETCH\" >> '{}.env'\nexec '{}' \"$@\"\n",
            log.display(),
            log.display(),
            real.trim()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let git = Git {
        program: script.to_str().unwrap().to_string(),
        extra_env: vec![
            ("GIT_AUTHOR_DATE".into(), DATE.into()),
            ("GIT_COMMITTER_DATE".into(), DATE.into()),
        ],
        ..Git::default()
    };
    (git, log)
}

#[test]
fn every_git_call_on_a_held_tree_switches_off_what_the_tree_could_run() {
    let mut run = Run::new();
    let f = fixture();
    let setup = steps(&f, "clone");
    run.step(&setup[0]);
    let (git, log) = logging_git(&run.base);
    let clock = run.clock.clone();
    let ws = Workspaces::with(
        Options {
            dir: run.at("tenant").to_str().unwrap().to_string(),
            tree_root: Some(run.at("trees").to_str().unwrap().to_string()),
            owner: Some(run.me),
            noevia: Some(run.me),
            mode: None,
            epoch: "qa".into(),
            change_max_buffer: None,
        },
        Box::new(move || clock.fetch_add(1, Ordering::SeqCst) as f64),
        git,
        Box::new(chown_tree),
    );
    let id = "00000000-0000-4000-8000-000000000001";
    let repo = run.at("repos/qa-repo").to_str().unwrap().to_string();
    ws.claim(Claim {
        task_id: id,
        repo_path: &repo,
        ..Claim::default()
    })
    .unwrap();
    let tree = run.at(&format!("trees/{id}"));
    fs::write(tree.join("a.txt"), "edited\n").unwrap();
    let released = ws.release(id, false).unwrap();
    assert_eq!(released.get("status").as_str(), Some("released"));
    let tree = tree.to_str().unwrap().to_string();
    let lines = fs::read_to_string(&log).unwrap();
    let mut seen = 0;
    let mut released_with = 0;
    for line in lines.lines() {
        // A commit message's later lines carry no " | ".
        let Some((cwd, cmd)) = line.split_once(" | ") else {
            continue;
        };
        // Every call made in the tree, and every one that reads from it (the fetch), once the
        // claim has made it: all with the tree's own commands switched off.
        let on_tree = cwd == tree || cmd.contains(&tree);
        let making = cmd.starts_with("clone ")
            || cmd.starts_with("checkout ")
            || cmd.starts_with("config --file");
        if on_tree && !making {
            seen += 1;
            assert!(
                cmd.starts_with(
                    "-c core.fsmonitor=false -c core.hooksPath=/dev/null -c core.attributesFile=/dev/null -c commit.gpgSign=false -c tag.gpgSign=false "
                ),
                "{line}"
            );
        }
        // The release's own status, add and commit also run without auto-maintenance, submodule
        // recursion or the ext transport; the fetch without the ext transport.
        let clone_off = "-c gc.auto=0 -c maintenance.auto=false -c diff.ignoreSubmodules=all -c submodule.recurse=false -c protocol.ext.allow=never -c protocol.allow=never ";
        for verb in [" status ", " add ", " commit "] {
            if on_tree && cmd.contains(verb) {
                assert!(cmd.contains(clone_off), "{line}");
                released_with += 1;
            }
        }
        if cmd.contains(" fetch ") {
            assert!(cmd.contains("-c protocol.ext.allow=never fetch"), "{line}");
            released_with += 1;
        }
    }
    assert!(seen >= 5 && released_with >= 4, "{lines}");
    // No git call lazily fetches a missing object from a promisor remote the harness named.
    let envs = fs::read_to_string(log.with_extension("log.env")).unwrap();
    assert!(envs.lines().count() >= 8, "{envs}");
    assert!(envs.lines().all(|l| l == "GIT_NO_LAZY_FETCH=1"), "{envs}");
}

#[test]
fn a_link_to_a_place_that_cannot_be_named_is_not_inside() {
    use std::os::unix::ffi::OsStrExt;
    let mut run = Run::new();
    let f = fixture();
    for s in steps(&f, "worktree").iter().take(5) {
        run.step(s);
    }
    let id = "00000000-0000-4000-8000-000000000001";
    let outside = run.base.join(std::ffi::OsStr::from_bytes(b"outside-\xff"));
    fs::create_dir_all(&outside).unwrap();
    let tree = run.at(&format!("tenant/code-workspaces/{id}"));
    std::os::unix::fs::symlink(&outside, tree.join("evil")).unwrap();
    let ws = &run.stores["w"];
    assert_eq!(ws.contains(id, "evil").unwrap(), Some(false));
    assert_eq!(ws.contains(id, "evil/new.txt").unwrap(), Some(false));
    assert_eq!(ws.contains(id, "a.txt").unwrap(), Some(true));
}

#[test]
fn the_hand_over_never_follows_a_link() {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    fs::create_dir_all(tree.join("sub")).unwrap();
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret"), "x").unwrap();
    std::os::unix::fs::symlink(&outside, tree.join("sub/link")).unwrap();
    std::os::unix::fs::symlink(outside.join("secret"), tree.join("file-link")).unwrap();
    let ctimes = || {
        (
            fs::metadata(&outside).unwrap().ctime_nsec(),
            fs::metadata(outside.join("secret")).unwrap().ctime_nsec(),
        )
    };
    let before = ctimes();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let me = fs::metadata(dir.path()).unwrap();
    chown_tree(
        &tree,
        Ids {
            uid: me.uid(),
            gid: me.gid(),
        },
    )
    .unwrap();
    assert_eq!(before, ctimes(), "a link was followed");
}

#[test]
fn the_hand_over_never_follows_a_directory_swapped_for_a_link_mid_walk() {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret"), "x").unwrap();
    fs::create_dir_all(tree.join("d")).unwrap();
    fs::write(tree.join("d/secret"), "qa").unwrap();
    let ctime = || fs::metadata(outside.join("secret")).unwrap().ctime_nsec();
    let before = ctime();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let me = fs::metadata(dir.path()).unwrap();
    // Right after `d` is handed over and before it is entered, the harness swaps it for a link.
    let swap = |name: &std::ffi::CStr| {
        if name.to_bytes() == b"d" {
            fs::rename(tree.join("d"), tree.join("d.real")).unwrap();
            std::os::unix::fs::symlink(&outside, tree.join("d")).unwrap();
        }
    };
    let swapped = code_workspace::chown_tree_with(
        &tree,
        Ids {
            uid: me.uid(),
            gid: me.gid(),
        },
        &swap,
    )
    .unwrap_err();
    // Refused, as in Node, and what the link names was not touched.
    assert!(
        swapped
            .to_string()
            .ends_with("d: it was replaced while it was being checked"),
        "{swapped}"
    );
    assert_eq!(before, ctime(), "the walk followed the swapped link");
}

#[test]
fn the_seal_takes_modes_away_and_never_follows_a_directory_swapped_for_a_link() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret"), "x").unwrap();
    fs::set_permissions(outside.join("secret"), fs::Permissions::from_mode(0o666)).unwrap();
    fs::create_dir_all(tree.join("d")).unwrap();
    fs::create_dir_all(tree.join("sub")).unwrap();
    fs::write(tree.join("sub/tool"), "t").unwrap();
    fs::write(tree.join("sub/other"), "t").unwrap();
    fs::hard_link(tree.join("sub/other"), tree.join("other-too")).unwrap();
    let set = |p: std::path::PathBuf, m: u32| {
        fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap()
    };
    set(tree.join("sub/tool"), 0o6775);
    set(tree.join("sub/other"), 0o666);
    set(tree.join("sub"), 0o2777);
    let me = fs::metadata(dir.path()).unwrap();
    let ids = Ids {
        uid: me.uid(),
        gid: me.gid(),
    };
    let mode = |p: std::path::PathBuf| fs::metadata(p).unwrap().mode() & 0o7777;
    let swap = |name: &std::ffi::CStr| {
        if name.to_bytes() == b"d" {
            fs::rename(tree.join("d"), tree.join("d.real")).unwrap();
            std::os::unix::fs::symlink(&outside, tree.join("d")).unwrap();
        }
    };
    let e = code_workspace::walk_tree(&tree, ids, true, &swap).unwrap_err();
    assert!(
        e.to_string()
            .contains("it was replaced while it was being checked"),
        "{e}"
    );
    assert_eq!(mode(outside.join("secret")), 0o666, "the link was followed");
    // Undisturbed: setuid, setgid and group/other write are gone; a file with a second link is left.
    fs::remove_file(tree.join("d")).unwrap();
    seal_tree(&tree, ids).unwrap();
    assert_eq!(mode(tree.join("sub/tool")), 0o755);
    assert_eq!(mode(tree.join("sub")), 0o755);
    assert_eq!(mode(tree.join("sub/other")), 0o666);
}

#[test]
fn the_runner_stops_git_at_its_buffer_cap_and_its_deadline() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    // A "git" that writes far past the cap, and one that leaves a child holding the pipes.
    let chatty = dir.path().join("chatty");
    fs::write(&chatty, "#!/bin/sh\nyes qa-output\n").unwrap();
    let stuck = dir.path().join("stuck");
    fs::write(&stuck, "#!/bin/sh\nsleep 30 &\nsleep 30\n").unwrap();
    for p in [&chatty, &stuck] {
        fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let capped = code_workspace::git::RunOptions {
        max_buffer: Some(1024),
        ..Default::default()
    };
    let git = Git {
        program: chatty.to_str().unwrap().to_string(),
        ..Git::default()
    };
    let t = Instant::now();
    let e = git.run(&[], None, &capped).unwrap_err();
    assert!(e.too_large && e.message.ends_with("ENOBUFS"), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(10));
    let git = Git {
        program: stuck.to_str().unwrap().to_string(),
        timeout: Duration::from_millis(300),
        ..Git::default()
    };
    let t = Instant::now();
    let e = git.run(&[], None, &Default::default()).unwrap_err();
    assert!(e.message.ends_with("ETIMEDOUT"), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    let git = Git {
        program: dir.path().join("missing").to_str().unwrap().to_string(),
        ..Git::default()
    };
    let e = git.run(&[], None, &Default::default()).unwrap_err();
    assert!(e.message.ends_with(" ENOENT"), "{e:?}");
}

// ── What the harness plants in a tree it held (review of noevia-rs#367) ─────────────────────────

fn task_id(n: u32) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

/// A clone-mode store (owner: this user) on a fresh QA repository, as the `refusals` scenario has it.
fn clone_store(run: &mut Run) -> Arc<Workspaces> {
    let f = fixture();
    run.step(&steps(&f, "refusals")[0]);
    let clock = run.clock.clone();
    Arc::new(Workspaces::with(
        Options {
            dir: run.at("tenant").to_str().unwrap().to_string(),
            tree_root: Some(run.at("trees").to_str().unwrap().to_string()),
            owner: Some(run.me),
            noevia: Some(run.me),
            mode: None,
            epoch: "qa".into(),
            change_max_buffer: None,
        },
        Box::new(move || clock.fetch_add(1, Ordering::SeqCst) as f64),
        Git {
            extra_env: vec![
                ("GIT_AUTHOR_DATE".into(), DATE.into()),
                ("GIT_COMMITTER_DATE".into(), DATE.into()),
            ],
            ..Git::default()
        },
        Box::new(chown_tree),
    ))
}

/// Claims task `n` in `ws` and returns its tree.
fn claim_n(ws: &Workspaces, run: &Run, n: u32) -> std::path::PathBuf {
    let id = task_id(n);
    let repo = run.at("repos/qa-repo").to_str().unwrap().to_string();
    let record = ws
        .claim(Claim {
            task_id: &id,
            repo_path: &repo,
            ..Claim::default()
        })
        .unwrap();
    std::path::PathBuf::from(record.get("path").as_str().unwrap())
}

/// Releases task `n` on another thread and answers within a deadline: a release that hangs fails the
/// test instead of holding it forever.
fn release_n(ws: &Arc<Workspaces>, n: u32) -> JValue {
    let (tx, rx) = std::sync::mpsc::channel();
    let ws = ws.clone();
    let id = task_id(n);
    std::thread::spawn(move || {
        let _ = tx.send(ws.release(&id, false));
    });
    rx.recv_timeout(std::time::Duration::from_secs(60))
        .expect("the release did not finish within its deadline")
        .unwrap()
}

fn stuck_with(record: &JValue, want: &str) {
    assert_eq!(
        record.get("status").as_str(),
        Some("stuck"),
        "{}",
        text(record)
    );
    let why = record.get("error").as_str().unwrap_or("");
    assert!(why.contains(want), "wanted {want:?} in {why:?}");
}

fn fifo_at(path: &Path) {
    use rustix::fs::{mknodat, FileType, Mode, CWD};
    mknodat(CWD, path, FileType::Fifo, Mode::from_raw_mode(0o666), 0).unwrap();
}

#[test]
fn an_attributes_file_that_is_not_a_plain_small_file_is_refused_not_read() {
    let mut run = Run::new();
    let ws = clone_store(&mut run);
    // A FIFO as `.gitattributes`: reading it would block, with the store's lock held.
    let tree = claim_n(&ws, &run, 1);
    fifo_at(&tree.join(".gitattributes"));
    stuck_with(&release_n(&ws, 1), ".gitattributes is not a regular file");
    assert!(tree.exists(), "the tree is kept for inspection");
    // ... in a nested directory.
    let tree = claim_n(&ws, &run, 2);
    fs::create_dir_all(tree.join("a/b")).unwrap();
    fifo_at(&tree.join("a/b/.gitattributes"));
    stuck_with(
        &release_n(&ws, 2),
        "a/b/.gitattributes is not a regular file",
    );
    // `.git/info/attributes` a link to /dev/zero: following it reads until memory runs out.
    let tree = claim_n(&ws, &run, 3);
    fs::create_dir_all(tree.join(".git/info")).unwrap();
    std::os::unix::fs::symlink("/dev/zero", tree.join(".git/info/attributes")).unwrap();
    stuck_with(
        &release_n(&ws, 3),
        ".git/info/attributes is not a regular file",
    );
    // `.git/info` itself a link: not followed either.
    let tree = claim_n(&ws, &run, 4);
    let outside = run.base.join("outside-info");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("attributes"), "* filter=x\n").unwrap();
    let _ = fs::remove_dir_all(tree.join(".git/info"));
    std::os::unix::fs::symlink(&outside, tree.join(".git/info")).unwrap();
    stuck_with(
        &release_n(&ws, 4),
        ".git/info/attributes is not a regular file",
    );
    // A `.gitattributes` that is a link (even a dangling one) is not read.
    let tree = claim_n(&ws, &run, 5);
    std::os::unix::fs::symlink("/dev/zero", tree.join(".gitattributes")).unwrap();
    stuck_with(&release_n(&ws, 5), ".gitattributes is not a regular file");
    let tree = claim_n(&ws, &run, 6);
    std::os::unix::fs::symlink("nowhere", tree.join(".gitattributes")).unwrap();
    stuck_with(&release_n(&ws, 6), ".gitattributes is not a regular file");
    // One over the cap is refused (a sparse file: nothing near its length is read).
    let tree = claim_n(&ws, &run, 7);
    let big = fs::File::create(tree.join(".gitattributes")).unwrap();
    big.set_len(1 << 40).unwrap();
    stuck_with(&release_n(&ws, 7), ".gitattributes is larger than");
    // The refusals are not blanket: a plain, small, harmless one is read and the release goes on.
    let tree = claim_n(&ws, &run, 8);
    fs::write(tree.join(".gitattributes"), "*.txt text eol=lf\n").unwrap();
    fs::write(tree.join("b.txt"), "b\n").unwrap();
    let done = release_n(&ws, 8);
    assert_eq!(
        done.get("status").as_str(),
        Some("released"),
        "{}",
        text(&done)
    );
    // And a filter named in a plain one is still refused, by name.
    let tree = claim_n(&ws, &run, 9);
    fs::write(tree.join(".gitattributes"), "*.txt filter=x\n").unwrap();
    stuck_with(&release_n(&ws, 9), ".gitattributes names a filter driver");
}

#[test]
fn a_huge_git_config_is_refused_not_read() {
    let mut run = Run::new();
    let ws = clone_store(&mut run);
    let tree = claim_n(&ws, &run, 1);
    let config = fs::OpenOptions::new()
        .append(true)
        .open(tree.join(".git/config"))
        .unwrap();
    // 8 GiB, sparse; a filesystem that cannot hold one gets one just over the cap.
    if config.set_len(8 << 30).is_err() {
        config.set_len(2 << 20).unwrap();
    }
    drop(config);
    let done = release_n(&ws, 1);
    stuck_with(
        &done,
        "Could not take the workspace back from the harness user",
    );
    stuck_with(&done, ".git/config is larger than 1048576 bytes");
    // A config that is not too large but is a FIFO or a link is refused as before.
    let tree = claim_n(&ws, &run, 2);
    fs::remove_file(tree.join(".git/config")).unwrap();
    fifo_at(&tree.join(".git/config"));
    stuck_with(&release_n(&ws, 2), ".git/config is not a plain file");
}

#[test]
fn a_partial_clone_cannot_make_noevia_contact_the_remote_it_names() {
    for (n, (key, value)) in [
        ("extensions.partialclone", "origin"),
        ("remote.origin.promisor", "true"),
        ("remote.origin.partialclonefilter", "blob:none"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut run = Run::new();
        let ws = clone_store(&mut run);
        let n = n as u32 + 1;
        let tree = claim_n(&ws, &run, n);
        shell(&["config", key, value], &tree);
        shell(
            &[
                "config",
                "remote.origin.url",
                &run.sub("<BASE>/repos/qa-repo"),
            ],
            &tree,
        );
        fs::write(tree.join("a.txt"), "changed\n").unwrap();
        stuck_with(&release_n(&ws, n), key);
        assert!(!run.base.join("pwned").exists());
    }
}

#[test]
fn removing_a_tree_stops_at_the_depth_limit_and_never_follows_a_link() {
    let dir = tempfile::tempdir().unwrap();
    let deep = |levels: usize| "d/".repeat(levels);
    // Within the limit: all gone, files and links included.
    let ok = dir.path().join("ok");
    fs::create_dir_all(ok.join(deep(200))).unwrap();
    fs::write(ok.join(deep(200)).join("f"), "f").unwrap();
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret"), "x").unwrap();
    std::os::unix::fs::symlink(&outside, ok.join("link")).unwrap();
    code_workspace::remove_all(ok.to_str().unwrap()).unwrap();
    assert!(!ok.exists());
    assert!(outside.join("secret").exists(), "a link was followed");
    // Beyond it: an error, no crash, and the part not reached is left.
    let too_deep = dir.path().join("too-deep");
    fs::create_dir_all(too_deep.join(deep(300))).unwrap();
    let e = code_workspace::remove_all(too_deep.to_str().unwrap()).unwrap_err();
    assert!(e.to_string().contains("nested too deeply"), "{e}");
    assert!(too_deep.exists());
    // Missing, a file and a link: no error; the link itself goes, not what it names.
    code_workspace::remove_all(dir.path().join("missing").to_str().unwrap()).unwrap();
    let file = dir.path().join("file");
    fs::write(&file, "f").unwrap();
    code_workspace::remove_all(file.to_str().unwrap()).unwrap();
    assert!(!file.exists());
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    code_workspace::remove_all(link.to_str().unwrap()).unwrap();
    assert!(!link.exists() && outside.join("secret").exists());
}

#[test]
fn a_harness_home_nested_past_the_limit_does_not_stop_the_release() {
    let mut run = Run::new();
    let ws = clone_store(&mut run);
    let tree = claim_n(&ws, &run, 1);
    let home = run.at(&format!("trees/.harness-home/{}", task_id(1)));
    fs::create_dir_all(home.join("d/".repeat(300))).unwrap();
    fs::write(tree.join("b.txt"), "b\n").unwrap();
    let done = release_n(&ws, 1);
    // As in Node: the home is not the task's work, and its removal failing is ignored.
    assert_eq!(
        done.get("status").as_str(),
        Some("released"),
        "{}",
        text(&done)
    );
    assert!(!tree.exists());
    assert!(
        home.exists(),
        "the part past the limit is left, not followed"
    );
}

#[test]
fn sealing_removes_acls_and_the_write_bits_of_fifos_and_sockets() {
    use rustix::fs::{getxattr, setxattr, XattrFlags};
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    fs::create_dir_all(tree.join("sub")).unwrap();
    fs::write(tree.join("sub/f.txt"), "f").unwrap();
    let me = fs::metadata(dir.path()).unwrap();
    let ids = Ids {
        uid: me.uid(),
        gid: me.gid(),
    };
    // FIFO and socket, group- and other-writable.
    fifo_at(&tree.join("fifo"));
    fs::set_permissions(tree.join("fifo"), fs::Permissions::from_mode(0o666)).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(tree.join("sock")).unwrap();
    fs::set_permissions(tree.join("sock"), fs::Permissions::from_mode(0o777)).unwrap();
    // ACLs, as the kernel's binary xattr (version 2; entries: tag, permission, id).
    let acl = |entries: &[(u16, u16, u32)]| {
        let mut v = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in entries {
            v.extend_from_slice(&tag.to_le_bytes());
            v.extend_from_slice(&perm.to_le_bytes());
            v.extend_from_slice(&id.to_le_bytes());
        }
        v
    };
    let none = u32::MAX;
    let access = acl(&[
        (1, 6, none),
        (2, 6, 4242),
        (4, 4, none),
        (0x10, 6, none),
        (0x20, 4, none),
    ]);
    let default = acl(&[
        (1, 7, none),
        (2, 7, 4242),
        (4, 5, none),
        (0x10, 7, none),
        (0x20, 5, none),
    ]);
    let set_access = setxattr(
        tree.join("sub/f.txt"),
        "system.posix_acl_access",
        &access,
        XattrFlags::empty(),
    );
    let set_default = setxattr(
        tree.join("sub"),
        "system.posix_acl_default",
        &default,
        XattrFlags::empty(),
    );
    let acls = set_access.is_ok() && set_default.is_ok();
    if !acls {
        eprintln!("this filesystem takes no POSIX ACLs ({set_access:?}, {set_default:?}); only the seal's call path and the modes are checked");
    }
    seal_tree(&tree, ids).unwrap();
    let mode = |p: &str| fs::metadata(tree.join(p)).unwrap().mode() & 0o777;
    assert_eq!(mode("fifo") & 0o022, 0, "the fifo kept group/other write");
    assert_eq!(mode("sock") & 0o022, 0, "the socket kept group/other write");
    if acls {
        let mut buf = [0u8; 256];
        for (p, name) in [
            ("sub/f.txt", "system.posix_acl_access"),
            ("sub", "system.posix_acl_default"),
        ] {
            let got = getxattr(tree.join(p), name, &mut buf[..]);
            assert_eq!(got, Err(rustix::io::Errno::NODATA), "{p} kept {name}");
        }
    }
}
