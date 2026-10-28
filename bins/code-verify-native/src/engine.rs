//! Private production Git/copy/test flow. Test seams do not provide a runtime isolation bypass.
use super::lifecycle::Terminal;
use code_verify_native::{config_safe_path, Commands, Request, TailOutput};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    Outside,
    HostileSource,
    HeadMismatch,
}
impl Refusal {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::Outside => "outside",
            Self::HostileSource => "hostile_source",
            Self::HeadMismatch => "head_mismatch",
        }
    }
}
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for Refusal {}
pub(super) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
#[derive(Clone, Copy, Deserialize, Serialize)]
pub(super) struct Identity {
    pub(super) uid: u32,
    pub(super) gid: u32,
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "operation", deny_unknown_fields)]
pub(super) enum Plan {
    Exec {
        who: Identity,
        program: String,
        args: Vec<String>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
        cpu: u64,
    },
    Prepare {
        who: Identity,
        directories: Vec<PathBuf>,
    },
    Readonly {
        who: Identity,
        tree: PathBuf,
        parent: PathBuf,
    },
    Remove {
        who: Identity,
        path: PathBuf,
    },
    Signal {
        who: Identity,
    },
}
impl Plan {
    pub(super) fn who(&self) -> Identity {
        match self {
            Self::Exec { who, .. }
            | Self::Prepare { who, .. }
            | Self::Readonly { who, .. }
            | Self::Remove { who, .. }
            | Self::Signal { who } => *who,
        }
    }
}
pub(super) struct Config {
    pub(super) root: PathBuf,
    pub(super) copy: PathBuf,
    pub(super) scratch: PathBuf,
    pub(super) control: PathBuf,
    pub(super) socket: PathBuf,
    pub(super) commands: Commands,
    pub(super) path: String,
    pub(super) wall: u64,
    pub(super) tail: usize,
    pub(super) min_uptime: u64,
}
pub(super) struct Execution {
    pub(super) terminal: Terminal,
    pub(super) out: code_verify_native::TailOutput,
    pub(super) timed_out: bool,
    pub(super) combined: code_verify_native::TailOutput,
}
pub(super) fn tree_permissions(path: &Path, readonly: bool) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if m.file_type().is_symlink() {
        return Ok(());
    }
    if !m.is_dir() && !m.is_file() {
        return Err("unsupported copy entry".into());
    }
    if m.is_dir() {
        if !readonly {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        for e in fs::read_dir(path)? {
            tree_permissions(&e?.path(), readonly)?;
        }
    }
    // Preserve executable bits under readonly conversion.
    let mode = if readonly {
        m.permissions().mode() & !0o222
    } else if m.is_dir() {
        0o700
    } else {
        0o600
    };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}
fn hostile_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    [
        "filter.",
        "alias.",
        "include.",
        "includeif.",
        "uploadpack.",
        "receive.",
        "sendemail.",
        "pack.",
        "protocol.",
        "transfer.",
    ]
    .iter()
    .any(|p| k.starts_with(p))
        || [
            "core.fsmonitor",
            "core.hookspath",
            "core.sshcommand",
            "core.gitproxy",
            "core.worktree",
            "core.askpass",
            "core.editor",
            "core.pager",
            "core.alternaterefscommand",
        ]
        .contains(&k.as_str())
        || (k.starts_with("diff.") && (k.ends_with(".command") || k.ends_with(".textconv")))
        || (k.starts_with("merge.") && k.ends_with(".driver"))
        || (k.starts_with("credential.") && k.ends_with("helper"))
}
pub(super) struct Verification {
    pub terminal: Terminal,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub output: TailOutput,
}
pub(super) trait Runner {
    fn git(&self) -> Identity;
    fn test(&self) -> Identity;
    fn source_config(&self, root: &Path, source: &Path) -> Result<Vec<u8>>;
    fn execute(&mut self, plan: &Plan, deadline: Instant, cap: usize) -> Result<Execution>;
    fn checked(&mut self, plan: Plan, deadline: Instant) -> Result<String> {
        let result = self.execute(&plan, deadline, 1_048_576)?;
        if result.terminal.code != Some(0) || result.timed_out || result.out.truncated {
            return Err("verified operation failed".into());
        }
        Ok(result.out.tail.trim().to_owned())
    }
}
pub(super) fn verify(c: &Config, a: &mut impl Runner, r: &Request) -> Result<Verification> {
    let began = Instant::now();
    let end = began
        .checked_add(Duration::from_millis(c.wall))
        .ok_or("wall budget")?;
    let command = c.commands.get(&r.repo).ok_or("not configured")?;
    let src = fs::canonicalize(&r.source).map_err(|_| Refusal::Outside)?;
    if !src.starts_with(&c.root) {
        return Err(Refusal::Outside.into());
    }
    if !config_safe_path(src.to_str().ok_or(Refusal::HostileSource)?) {
        return Err(Refusal::HostileSource.into());
    }
    let ctl = c.control.join(format!("noevia-verify-{}", r.nonce));
    let copy = c.copy.join(format!("noevia-verify-{}", r.nonce));
    let scratch = c.scratch.join(format!("noevia-verify-{}", r.nonce));
    let tree = copy.join("tree");
    fs::create_dir(&ctl)?;
    fs::set_permissions(&ctl, fs::Permissions::from_mode(0o755))?;
    let cfg = ctl.join("gitconfig");
    let escaped = src.to_str().ok_or("source encoding")?;
    fs::write(
        &cfg,
        format!("[safe]\n\tdirectory = \"{escaped}\"\n\tdirectory = \"{escaped}/.git\"\n"),
    )?;
    let config_copy = ctl.join("source-config");
    fs::write(
        &config_copy,
        a.source_config(&c.root, &src)
            .map_err(|_| Refusal::HostileSource)?,
    )?;
    fs::set_permissions(&cfg, fs::Permissions::from_mode(0o644))?;
    fs::set_permissions(&config_copy, fs::Permissions::from_mode(0o644))?;
    let env = vec![
        ("PATH".into(), c.path.clone()),
        ("HOME".into(), ctl.to_string_lossy().into_owned()),
        ("LANG".into(), "C.UTF-8".into()),
        (
            "GIT_CONFIG_GLOBAL".into(),
            cfg.to_string_lossy().into_owned(),
        ),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ("GIT_ASKPASS".into(), "/bin/false".into()),
    ];
    let cpu = c.wall.div_ceil(1000).max(1);
    let git_identity = a.git();
    let git_plan = |args: Vec<String>, cwd: &Path| Plan::Exec {
        who: git_identity,
        program: "git".into(),
        args,
        cwd: cwd.to_owned(),
        env: env.clone(),
        cpu,
    };
    let keys = a.checked(
        git_plan(
            vec![
                "config".into(),
                "--file".into(),
                config_copy.to_string_lossy().into_owned(),
                "--name-only".into(),
                "--list".into(),
            ],
            Path::new("/"),
        ),
        end,
    )?;
    if keys.lines().any(hostile_key) {
        return Err(Refusal::HostileSource.into());
    }
    a.checked(
        Plan::Prepare {
            who: a.git(),
            directories: vec![copy.clone()],
        },
        end,
    )?;
    let off = [
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.attributesFile=/dev/null",
        "-c",
        "protocol.file.allow=always",
        "-c",
        "advice.detachedHead=false",
    ];
    let args = |tail: Vec<String>| off.iter().map(|s| (*s).to_owned()).chain(tail).collect();
    a.checked(
        git_plan(
            args(vec![
                "clone".into(),
                "--quiet".into(),
                "--no-local".into(),
                "--no-hardlinks".into(),
                "--no-checkout".into(),
                "--config".into(),
                "core.hooksPath=/dev/null".into(),
                "--config".into(),
                "core.fsmonitor=false".into(),
                "--config".into(),
                "core.attributesFile=/dev/null".into(),
                src.to_string_lossy().into_owned(),
                tree.to_string_lossy().into_owned(),
            ]),
            Path::new("/"),
        ),
        end,
    )?;
    a.checked(
        git_plan(
            args(vec![
                "fsck".into(),
                "--no-dangling".into(),
                "--no-progress".into(),
            ]),
            &tree,
        ),
        end,
    )?;
    let commit = a.checked(
        git_plan(
            args(vec![
                "rev-parse".into(),
                "--verify".into(),
                "--end-of-options".into(),
                format!("{}^{{commit}}", r.head_sha),
            ]),
            &tree,
        ),
        end,
    )?;
    if commit != r.head_sha {
        return Err(Refusal::HeadMismatch.into());
    }
    a.checked(
        git_plan(
            args(vec![
                "checkout".into(),
                "--quiet".into(),
                "--detach".into(),
                r.head_sha.clone(),
            ]),
            &tree,
        ),
        end,
    )?;
    let head = a.checked(
        git_plan(args(vec!["rev-parse".into(), "HEAD".into()]), &tree),
        end,
    )?;
    let actual_tree = a.checked(
        git_plan(args(vec!["rev-parse".into(), "HEAD^{tree}".into()]), &tree),
        end,
    )?;
    let expected_tree = a.checked(
        git_plan(
            args(vec!["rev-parse".into(), format!("{}^{{tree}}", r.head_sha)]),
            &tree,
        ),
        end,
    )?;
    if head != r.head_sha || actual_tree != expected_tree {
        return Err(Refusal::HeadMismatch.into());
    }
    a.checked(
        Plan::Readonly {
            who: a.git(),
            tree: tree.clone(),
            parent: copy.clone(),
        },
        end,
    )?;
    let home = scratch.join("home");
    let tmp = scratch.join("tmp");
    a.checked(
        Plan::Prepare {
            who: a.test(),
            directories: vec![scratch.clone(), home.clone(), tmp.clone()],
        },
        end,
    )?;
    let env = vec![
        ("PATH".into(), c.path.clone()),
        ("HOME".into(), home.to_string_lossy().into_owned()),
        ("TMPDIR".into(), tmp.to_string_lossy().into_owned()),
        ("LANG".into(), "C.UTF-8".into()),
        ("CI".into(), "true".into()),
        ("NO_COLOR".into(), "1".into()),
    ];
    let result = a.execute(
        &Plan::Exec {
            who: a.test(),
            program: "/bin/sh".into(),
            args: vec!["-c".into(), command.into()],
            cwd: tree,
            env,
            cpu,
        },
        end,
        c.tail,
    )?;
    // Every cleanup helper is itself drained before its acknowledgement can be trusted.
    a.checked(
        Plan::Remove {
            who: a.test(),
            path: scratch,
        },
        Instant::now() + Duration::from_secs(5),
    )?;
    a.checked(
        Plan::Remove {
            who: a.git(),
            path: copy,
        },
        Instant::now() + Duration::from_secs(5),
    )?;
    fs::remove_dir_all(ctl)?;
    let output = result.combined;
    Ok(Verification {
        terminal: result.terminal,
        timed_out: result.timed_out,
        duration_ms: u64::try_from(began.elapsed().as_millis())?,
        output,
    })
}

#[cfg(test)]
mod tests {
    //! Same production verify() flow, real disposable Git repositories and private current-user
    //! execution seam. This does not claim PID1, credential, deadline or descendant isolation.
    use super::*;
    use code_verify_native::Tail;
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    struct Portable {
        who: Identity,
        operator_runs: usize,
        reject_readonly: bool,
    }
    impl Runner for Portable {
        fn git(&self) -> Identity {
            self.who
        }
        fn test(&self) -> Identity {
            self.who
        }
        fn source_config(&self, root: &Path, src: &Path) -> Result<Vec<u8>> {
            // Fixture-only seam. The actual production reader remains nofollow/descriptor-relative.
            if !src.starts_with(root) {
                return Err("fixture source outside".into());
            }
            Ok(fs::read(src.join(".git/config"))?)
        }
        fn execute(&mut self, plan: &Plan, deadline: Instant, cap: usize) -> Result<Execution> {
            if Instant::now() >= deadline {
                return Err("fixture deadline expired".into());
            }
            if plan.who().uid != self.who.uid {
                return Err("fixture ownership".into());
            }
            let mut out = Tail::new(cap);
            let mut combined = Tail::new(cap);
            let terminal = match plan {
                Plan::Exec {
                    program,
                    args,
                    cwd,
                    env,
                    ..
                } => {
                    if program == "/bin/sh" {
                        self.operator_runs += 1;
                    }
                    let output = Command::new(program)
                        .args(args)
                        .current_dir(cwd)
                        .env_clear()
                        .envs(env.iter().map(|(key, value)| (key, value)))
                        .stdin(Stdio::null())
                        .output()?;
                    out.push(&output.stdout);
                    combined.push(&output.stdout);
                    combined.push(&output.stderr);
                    Terminal {
                        code: output.status.code(),
                        signal: output.status.signal(),
                    }
                }
                Plan::Prepare { directories, .. } => {
                    for p in directories {
                        fs::create_dir(p)?;
                        fs::set_permissions(p, fs::Permissions::from_mode(0o700))?;
                    }
                    Terminal {
                        code: Some(0),
                        signal: None,
                    }
                }
                Plan::Readonly { tree, parent, .. } => {
                    if self.reject_readonly {
                        return Err("injected copy preparation fault".into());
                    }
                    tree_permissions(tree, true)?;
                    fs::set_permissions(parent, fs::Permissions::from_mode(0o555))?;
                    Terminal {
                        code: Some(0),
                        signal: None,
                    }
                }
                Plan::Remove { path, .. } => {
                    if path.try_exists()? {
                        tree_permissions(path, false)?;
                        fs::remove_dir_all(path)?;
                    }
                    Terminal {
                        code: Some(0),
                        signal: None,
                    }
                }
                Plan::Signal { .. } => return Err("portable tests cannot signal identities".into()),
            };
            Ok(Execution {
                terminal,
                out: out.read(),
                combined: combined.read(),
                timed_out: false,
            })
        }
    }
    struct Fixture {
        temp: tempfile::TempDir,
        config: Config,
        request: Request,
        runner: Portable,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = tree_permissions(self.temp.path(), false);
        }
    }
    fn git(repo: &Path, args: &[&str]) -> Result<String> {
        let o = Command::new("git")
            .args(args)
            .current_dir(repo)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin:/opt/homebrew/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::null())
            .output()?;
        if !o.status.success() {
            return Err(format!(
                "synthetic Git setup failed: {}",
                String::from_utf8_lossy(&o.stderr)
            )
            .into());
        }
        Ok(String::from_utf8(o.stdout)?.trim().into())
    }
    fn fixture(command: &str, sha256: bool) -> Result<Fixture> {
        let temp = tempfile::tempdir()?;
        let root = fs::canonicalize(temp.path())?;
        let source = root.join("sources");
        fs::create_dir(&source)?;
        let repo = source.join("app");
        fs::create_dir(&repo)?;
        if sha256 {
            git(&repo, &["init", "--quiet", "--object-format=sha256"])?;
        } else {
            git(&repo, &["init", "--quiet"])?;
        }
        fs::write(repo.join("payload"), "committed\n")?;
        git(&repo, &["add", "payload"])?;
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "synthetic",
            ],
        )?;
        let sha = git(&repo, &["rev-parse", "HEAD"])?;
        let copy = root.join("copy");
        let scratch = root.join("run");
        let control = root.join("control");
        for p in [&copy, &scratch, &control] {
            fs::create_dir(p)?;
        }
        let metadata = fs::metadata(&root)?;
        let who = Identity {
            uid: metadata.uid(),
            gid: metadata.gid(),
        };
        let request=Request::parse_line(&serde_json::to_vec(&serde_json::json!({"noevia":"verify","repo":"app","source":repo,"headSha":sha,"nonce":"b".repeat(32)}))?).map_err(|_|"fixture request")?;
        let config = Config {
            root: source,
            copy,
            scratch,
            control,
            socket: root.join("unused-test-socket"),
            commands: Commands::parse(&format!("app|{command}")),
            path: "/usr/local/bin:/usr/bin:/bin:/opt/homebrew/bin".into(),
            wall: 10000,
            tail: 16384,
            min_uptime: 1,
        };
        // These fields are deliberately not acted on by the portable engine seam.
        assert_eq!(config.min_uptime, 1);
        assert!(!config.socket.exists());
        Ok(Fixture {
            temp,
            config,
            request,
            runner: Portable {
                who,
                operator_runs: 0,
                reject_readonly: false,
            },
        })
    }
    #[test]
    fn real_copy_runs_requested_commit_and_keeps_output_as_data() -> Result<()> {
        let mut f = fixture(
            "cat payload; printf 'diagnostic' >&2; printf '{\"passed\":true}'; exit 7",
            false,
        )?;
        fs::write(
            Path::new(&f.request.source).join("payload"),
            "uncommitted-agent-content\n",
        )?;
        let observed_start = Instant::now();
        let result = verify(&f.config, &mut f.runner, &f.request)?;
        assert!(u128::from(result.duration_ms) <= observed_start.elapsed().as_millis());
        assert_eq!(result.terminal.code, Some(7));
        assert!(!result.timed_out);
        assert!(result.output.tail.contains("committed\n"));
        assert!(!result.output.tail.contains("uncommitted-agent"));
        assert!(result.output.tail.contains("diagnostic"));
        assert!(result.output.tail.contains("\"passed\":true"));
        assert_eq!(f.runner.operator_runs, 1);
        for path in [&f.config.copy, &f.config.scratch, &f.config.control] {
            assert_eq!(fs::read_dir(path)?.count(), 0);
        }
        Ok(())
    }
    #[test]
    fn hostile_source_config_never_admits_the_operator_command() -> Result<()> {
        let mut f = fixture("printf should-not-run", false)?;
        let config = Path::new(&f.request.source).join(".git/config");
        let mut file = fs::OpenOptions::new().append(true).open(config)?;
        file.write_all(b"\n[alias]\n\tverify = !printf injected\n")?;
        drop(file);
        assert!(verify(&f.config, &mut f.runner, &f.request).is_err());
        assert_eq!(f.runner.operator_runs, 0);
        Ok(())
    }
    #[test]
    fn absent_commit_or_corrupt_object_never_runs_tests() -> Result<()> {
        let mut missing = fixture("printf should-not-run", false)?;
        missing.request.head_sha = "f".repeat(40);
        assert!(verify(&missing.config, &mut missing.runner, &missing.request).is_err());
        assert_eq!(missing.runner.operator_runs, 0);
        let mut corrupt = fixture("printf should-not-run", false)?;
        let src = Path::new(&corrupt.request.source);
        let blob = git(src, &["rev-parse", "HEAD:payload"])?;
        let object = src
            .join(".git/objects")
            .join(blob.get(..2).ok_or("object prefix")?)
            .join(blob.get(2..).ok_or("object suffix")?);
        fs::set_permissions(&object, fs::Permissions::from_mode(0o600))?;
        fs::write(object, b"corrupted-object")?;
        assert!(verify(&corrupt.config, &mut corrupt.runner, &corrupt.request).is_err());
        assert_eq!(corrupt.runner.operator_runs, 0);
        Ok(())
    }
    #[test]
    fn failed_copy_freeze_and_outside_root_do_not_admit_tests() -> Result<()> {
        let mut f = fixture("printf should-not-run", false)?;
        f.runner.reject_readonly = true;
        assert!(verify(&f.config, &mut f.runner, &f.request).is_err());
        assert_eq!(f.runner.operator_runs, 0);
        let mut outside = fixture("printf should-not-run", false)?;
        outside.request.source = outside.config.control.to_string_lossy().into_owned();
        assert!(verify(&outside.config, &mut outside.runner, &outside.request).is_err());
        assert_eq!(outside.runner.operator_runs, 0);
        Ok(())
    }
    #[test]
    fn full_sha256_commit_is_verified_without_sha1_assumptions() -> Result<()> {
        let mut f = fixture("cat payload", true)?;
        assert_eq!(f.request.head_sha.len(), 64);
        let result = verify(&f.config, &mut f.runner, &f.request)?;
        assert_eq!(result.terminal.code, Some(0));
        assert_eq!(result.output.tail, "committed\n");
        assert_eq!(f.runner.operator_runs, 1);
        Ok(())
    }
}
