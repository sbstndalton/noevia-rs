//! crates/code-workspace: every operation core's own code-workspace.cjs ran in
//! tests/fixtures/node.v1.json (tools/gen-fixtures.cjs: throwaway QA repositories, fixed commit
//! dates, a ticking clock), replayed through the Rust port with the same git in a fresh directory.
//! Each answer and the snapshot after every step (records, trust file, source refs and worktrees,
//! exclude file, the trees on disk, directory modes, whether a planted command ran) must match.
//! Where the port differs on purpose, the fixture shows Node's defect and the Rust answer is
//! checked against its own expectation here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use code_workspace::git::Git;
use code_workspace::{chown_tree, Claim, Error, Ids, Options, Workspaces};
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

fn walk(base: &Path, rel: &str) -> Vec<JValue> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(base.join(rel)) else {
        return out;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().into_string().unwrap();
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

    fn at(&self, p: &str) -> std::path::PathBuf {
        self.base.join(p)
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
                fs::create_dir_all(self.at(&str_of("path"))).unwrap();
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
                let _ = fs::remove_file(self.at(&str_of("path")));
                return None;
            }
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
    // core a72ae94; a fixture regenerated from another source fails here until updated on purpose.
    assert_eq!(
        f.get("sources").get("code-workspace.cjs").as_str(),
        Some("d8cb106b0b6b59013b7b522b85ff2862b36c8cef7cb93e73c61e0deea54c3577")
    );
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
        let mut run = Run::new();
        let mut last = JValue::Null;
        for (i, s) in steps(&f, name).iter().enumerate() {
            if let Some(got) = run.step(s) {
                assert_eq!(
                    text(&got),
                    text(s.get("result")),
                    "{name} step {i}: {}",
                    text(s)
                );
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
    assert_eq!(seen, 8);
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
    // Node's release ran the signing program (stuck only afterwards); the port refuses first.
    assert!(node[5].contains(r#""status":"stuck""#), "{}", node[5]);
    assert!(got[5].contains("Refused to release the workspace: its .git/config sets commit.gpgsign, gpg.program, which would make git run a command."), "{}", got[5]);
    // Node released the clone with a worktree config; the port does not touch it.
    assert!(node[7].contains(r#""status":"released""#), "{}", node[7]);
    assert!(got[7].contains("Refused to release the workspace: its .git/config.worktree would be read as configuration."), "{}", got[7]);
    assert!(!run.base.join("pwned").exists(), "a planted command ran");
}

#[test]
fn no_planted_command_ever_runs_as_noevia() {
    // Replayed through the port: after every step noevia takes in the hostile scenarios, nothing
    // planted has run.
    let f = fixture();
    for name in ["hostile-clone", "differences"] {
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
            "#!/bin/sh\necho \"$(pwd) | $*\" >> '{}'\nexec '{}' \"$@\"\n",
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
                    "-c core.fsmonitor=false -c core.hooksPath=/dev/null -c core.attributesFile=/dev/null "
                ),
                "{line}"
            );
        }
        if cmd.contains(" commit ") {
            assert!(cmd.contains("-c commit.gpgSign=false"), "{line}");
        }
    }
    assert!(seen >= 5, "{lines}");
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
