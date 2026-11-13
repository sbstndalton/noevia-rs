//! `git`, run the way core code-workspace.cjs `defaultRun` runs it (`execFileSync('git', args, …)`):
//! a cleared environment holding only [`GIT_ENV_KEYS`] of the server's own plus the caller's,
//! stdin closed, both outputs captured up to a buffer cap, a two-minute deadline, and Node's error
//! text: `Command failed: git <args>` and, when git said anything, a newline and its stderr.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// code-workspace.cjs `GIT_ENV_KEYS` (#1916): what git needs of the server's environment, and
/// nothing else (no provider keys, no store secrets).
pub const GIT_ENV_KEYS: [&str; 9] = [
    "PATH", "HOME", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR", "USER", "LOGNAME",
];

/// `execFileSync`'s default `maxBuffer`.
pub const DEFAULT_MAX_BUFFER: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(120);

/// A failed git run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitError {
    /// Node's `error.message`.
    pub message: String,
    /// What git wrote to stderr (Node's `error.stderr`).
    pub stderr: String,
    /// An output passed its buffer cap (Node's `ENOBUFS`).
    pub too_large: bool,
}

/// One git call's options (`defaultRun`'s third and fourth arguments).
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// The caller's own environment, added over [`GIT_ENV_KEYS`].
    pub env: Vec<(String, String)>,
    pub max_buffer: Option<usize>,
    /// Keep the output untrimmed.
    pub raw: bool,
}

/// How git is started. `extra_env` is added to every call after the caller's (tests pin the commit
/// dates with it, exactly as the fixture generator does).
#[derive(Debug, Clone)]
pub struct Git {
    pub program: String,
    pub extra_env: Vec<(String, String)>,
    pub timeout: Duration,
}

impl Default for Git {
    fn default() -> Self {
        Git {
            program: "git".into(),
            extra_env: Vec::new(),
            timeout: TIMEOUT,
        }
    }
}

/// `String.prototype.trim`: JS whitespace and line terminators at both ends.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| {
        matches!(
            c,
            '\u{9}'..='\u{d}'
                | ' '
                | '\u{a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    })
}

fn read_capped(mut from: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut over = false;
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let chunk = buf.get(..n).unwrap_or(&[]);
                if out.len() + chunk.len() > cap {
                    over = true;
                    // Keep draining so the child is never blocked on a full pipe.
                    continue;
                }
                out.extend_from_slice(chunk);
            }
        }
    }
    (out, over)
}

impl Git {
    /// Runs `git args` in `cwd` (the process's own directory when `None`): its stdout, trimmed
    /// unless `raw`, or the failure.
    pub fn run(
        &self,
        args: &[String],
        cwd: Option<&Path>,
        options: &RunOptions,
    ) -> Result<String, GitError> {
        let command_line = std::iter::once(self.program.as_str())
            .chain(args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        let failed = |stderr: String, too_large: bool, why: Option<&str>| {
            let mut message = match why {
                Some(why) => format!("spawnSync {} {why}", self.program),
                None => format!("Command failed: {command_line}"),
            };
            if why.is_none() && !stderr.is_empty() {
                message.push('\n');
                message.push_str(&stderr);
            }
            GitError {
                message,
                stderr,
                too_large,
            }
        };
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for key in GIT_ENV_KEYS {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        for (k, v) in options.env.iter().chain(self.extra_env.iter()) {
            command.env(k, v);
        }
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command
            .spawn()
            .map_err(|e| failed(String::new(), false, Some(&format!("{}", e.kind()))))?;
        let cap = options.max_buffer.unwrap_or(DEFAULT_MAX_BUFFER);
        let out = child
            .stdout
            .take()
            .map(|o| std::thread::spawn(move || read_capped(o, cap)));
        let err = child
            .stderr
            .take()
            .map(|e| std::thread::spawn(move || read_capped(e, cap)));
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let join = |h: Option<std::thread::JoinHandle<(Vec<u8>, bool)>>| {
            h.and_then(|h| h.join().ok()).unwrap_or_default()
        };
        let (stdout, out_over) = join(out);
        let (stderr, err_over) = join(err);
        let stderr = String::from_utf8_lossy(&stderr).into_owned();
        let Some(status) = status else {
            return Err(failed(stderr, false, Some("ETIMEDOUT")));
        };
        if out_over || err_over {
            return Err(failed(stderr, true, Some("ENOBUFS")));
        }
        if !status.success() {
            return Err(failed(stderr, false, None));
        }
        let text = String::from_utf8_lossy(&stdout).into_owned();
        Ok(if options.raw {
            text
        } else {
            js_trim(&text).to_string()
        })
    }
}
