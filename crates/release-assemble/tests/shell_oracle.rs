//! Differential tests against the shell itself: the verbatim excerpts of assemble-release.sh in
//! `fixtures/assemble-release-excerpt.sh` run under bash with stubbed tree work, and the Rust plan
//! must make the same calls, in the same order, and stop with the same refusal. Skipped (with a note)
//! when bash is not installed.

use release_assemble::lock::Lock;
use release_assemble::plan::{assemble_components, Source};
use std::path::{Path, PathBuf};
use std::process::Command;

const EXCERPT: &str = include_str!("../fixtures/assemble-release-excerpt.sh");

fn section(name: &str) -> String {
    let start = format!("#@{name}\n");
    let from = EXCERPT
        .find(&start)
        .map(|i| i + start.len())
        .unwrap_or(EXCERPT.len());
    let rest = EXCERPT.get(from..).unwrap_or_default();
    rest.split("\n#@").next().unwrap_or_default().to_string()
}

fn bash() -> Option<&'static str> {
    Command::new("bash")
        .arg("-c")
        .arg("true")
        .status()
        .ok()
        .filter(|s| s.success())
        .map(|_| "bash")
}

/// The stubs: every tree operation is logged; a fetched noevia-rs gets a Cargo.toml unless its ref
/// starts with "dead"; a fetch of a ref starting with "bad" fails like a download error.
const STUBS: &str = r#"
die() { echo "assemble-release: $*" >&2; exit 1; }
export_self() { echo "export $*" >> "$log"; }
fetch_remote() {
  case "$2" in bad*) die "could not download $1 at $2" ;; esac
  echo "fetch $*" >> "$log"
  if [ "$1" = noevia-rs ]; then case "$2" in dead*) ;; *) mkdir -p "$4"; : > "$4/Cargo.toml" ;; esac; fi
}
"#;

struct Recorder {
    log: Vec<String>,
}

impl Source for Recorder {
    fn export_self(&mut self, dest: &Path, prefix: &str, paths: &[&str]) -> Result<(), String> {
        self.log.push(format!(
            "export {} {prefix} {}",
            dest.display(),
            paths.join(" ")
        ));
        Ok(())
    }
    fn fetch(
        &mut self,
        repo: &str,
        reference: &str,
        sha256: &str,
        dest: &Path,
    ) -> Result<(), String> {
        if reference.starts_with("bad") {
            return Err(format!("could not download {repo} at {reference}"));
        }
        self.log.push(format!(
            "fetch {repo} {reference} {sha256} {}",
            dest.display()
        ));
        Ok(())
    }
    fn is_file(&self, path: &Path) -> bool {
        // The stub's rule: the rs fetch made a Cargo.toml unless its ref starts with "dead".
        let rs_fetch = self
            .log
            .iter()
            .rev()
            .find(|l| l.starts_with("fetch noevia-rs "));
        path.ends_with("rs/Cargo.toml")
            && rs_fetch
                .is_some_and(|l| !l.trim_start_matches("fetch noevia-rs ").starts_with("dead"))
    }
}

struct Outcome {
    log: Vec<String>,
    result: Result<String, String>,
}

fn run_shell(lock: &[u8], sha: &str, full: &str, dir: &Path) -> Outcome {
    let tree = dir.join("tree");
    let log = dir.join("log");
    let lock_file = dir.join("lock");
    let _ = std::fs::remove_dir_all(&tree);
    let _ = std::fs::remove_file(&log);
    std::fs::create_dir_all(&tree).ok();
    std::fs::write(&lock_file, lock).ok();
    std::fs::write(&log, b"").ok();
    let script = format!(
        "set -euo pipefail\n{STUBS}\nlock=\"$(cat -- \"$LOCK\")\"\n{}\necho \"refs $web_sha|$core_sha|$services_sha|$rs_sha\"\n",
        section("resolve")
    );
    let out = Command::new("bash")
        .arg("-c")
        .arg(&script)
        .env("LOCK", &lock_file)
        .env("log", &log)
        .env("tree", &tree)
        .env("sha", sha)
        .env("full_sha", full)
        .env("work", dir)
        .output();
    let Ok(out) = out else {
        return Outcome {
            log: vec![],
            result: Err("bash did not run".into()),
        };
    };
    if std::env::var("RELEASE_ASSEMBLE_DEBUG").is_ok() {
        eprintln!(
            "script:\n{script}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let log = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let result = if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim_end().to_string())
    };
    Outcome { log, result }
}

fn run_rust(lock: &[u8], sha: &str, full: &str, dir: &Path) -> Outcome {
    let tree: PathBuf = dir.join("tree");
    let mut rec = Recorder { log: vec![] };
    let result = assemble_components(&Lock::from_bytes(lock), sha, full, &tree, &mut rec)
        .map(|r| format!("refs {}|{}|{}|{}", r.web, r.core, r.services, r.rs))
        .map_err(|e| format!("assemble-release: {e}"));
    Outcome {
        log: rec.log,
        result,
    }
}

/// A small deterministic generator (no rand dependency).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        let n = items.len() as u64;
        items
            .get(usize::try_from(self.next() % n).unwrap_or(0))
            .copied()
            .unwrap_or("")
    }
}

fn hex(n: usize, seed: u64) -> String {
    (0..n)
        .map(|i| {
            char::from(
                b"0123456789abcdef"
                    .get(((seed as usize) + i * 7) % 16)
                    .copied()
                    .unwrap_or(b'0'),
            )
        })
        .collect()
}

fn values(rng: &mut Lcg, sum: bool) -> String {
    let h40 = hex(40, rng.next());
    let h64 = hex(64, rng.next());
    let choice = rng.next() % 16;
    let v = match (sum, choice) {
        (_, 0) => String::new(),
        (false, 1..=3) => "self".into(),
        (false, 4) => "self ".into(),
        (false, 5) => "self\r".into(),
        (false, 6) => h40.to_uppercase(),
        (false, 7) => h40.get(..39).unwrap_or_default().to_string(),
        (false, 8) => format!("dead{}", h40.get(4..).unwrap_or_default()),
        (false, 9) => format!("bad0{}", h40.get(4..).unwrap_or_default()),
        (false, 10) => format!("{h40} # pinned"),
        (false, _) => h40,
        (true, 1) => h64.get(..63).unwrap_or_default().to_string(),
        (true, 2) => format!("{h64}\r"),
        (true, 3) => h64.to_uppercase(),
        (true, _) => h64,
    };
    v
}

fn random_lock(rng: &mut Lcg) -> Vec<u8> {
    let keys = ["NOEVIA_WEB", "NOEVIA_CORE", "NOEVIA_SERVICES", "NOEVIA_RS"];
    let mut lines: Vec<String> = vec!["# Pinned sources (generated)".into()];
    for k in keys {
        for (suffix, sum) in [("_REF", false), ("_SHA256", true)] {
            let copies = rng.next() % 5; // 0 = absent, 1..2 = once, 3..4 = twice (last wins)
            let copies = match copies {
                0 => 0,
                1 | 2 => 1,
                _ => 2,
            };
            for _ in 0..copies {
                let style = rng.pick(&["", "", "", "# ", " ", "export "]);
                lines.push(format!("{style}{k}{suffix}={}", values(rng, sum)));
            }
        }
        if rng.next().is_multiple_of(9) {
            lines.push(format!("{k}_REFX=self"));
        }
    }
    // Shuffle a little: lock order never matters except for duplicates.
    for i in (1..lines.len()).rev() {
        let j = usize::try_from(rng.next()).unwrap_or(0) % (i + 1);
        lines.swap(i, j);
    }
    let mut text = lines.join("\n").into_bytes();
    if rng.next().is_multiple_of(2) {
        text.extend_from_slice(b"\n\n");
    }
    text
}

fn compare(lock: &[u8], sha: &str, full: &str, dir: &Path) {
    let sh = run_shell(lock, sha, full, dir);
    let rs = run_rust(lock, sha, full, dir);
    assert_eq!(
        rs.result,
        sh.result,
        "verdict differs for lock:\n{}",
        String::from_utf8_lossy(lock)
    );
    assert_eq!(
        rs.log,
        sh.log,
        "tree calls differ for lock:\n{}",
        String::from_utf8_lossy(lock)
    );
}

#[test]
fn recorded_locks_match_the_shell() {
    if bash().is_none() {
        eprintln!("shell_oracle: bash not installed, skipped");
        return;
    }
    let dir = tempfile::tempdir().ok();
    let Some(dir) = dir else { return };
    let full = "11bf2da7aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let h40 = "e0954036625168bde8cd0634f50c1fa81bfcb9e7";
    let h64 = "fb4d0ef572520f6700030c76a4b087dd7412f7c4ab2ee74ce896ea00c9472acb";
    let fixed: Vec<String> = vec![
        // today's shape: every component pinned
        format!("NOEVIA_RS_REF={h40}\nNOEVIA_RS_SHA256={h64}\nNOEVIA_WEB_REF={h40}\nNOEVIA_WEB_SHA256={h64}\nNOEVIA_CORE_REF={h40}\nNOEVIA_CORE_SHA256={h64}\nNOEVIA_SERVICES_REF={h40}\nNOEVIA_SERVICES_SHA256={h64}\n"),
        // a lock from before #952: web/core self, no services key, no rs
        "NOEVIA_WEB_REF=self\nNOEVIA_CORE_REF=self\n".into(),
        // services key present but empty: refused, not self
        "NOEVIA_WEB_REF=self\nNOEVIA_CORE_REF=self\nNOEVIA_SERVICES_REF=\n".into(),
        // pinned without a checksum
        format!("NOEVIA_WEB_REF={h40}\n"),
        // rs checksum without a valid ref
        format!("NOEVIA_WEB_REF=self\nNOEVIA_CORE_REF=self\nNOEVIA_RS_SHA256={h64}\nNOEVIA_RS_REF=self\n"),
        // rs ref without a checksum: an empty rs/
        format!("NOEVIA_WEB_REF=self\nNOEVIA_CORE_REF=self\nNOEVIA_RS_REF={h40}\n"),
        // rs without Cargo.toml
        format!("NOEVIA_WEB_REF=self\nNOEVIA_CORE_REF=self\nNOEVIA_RS_REF=dead{}\nNOEVIA_RS_SHA256={h64}\n", &h40[4..]),
        // a download failure before a later lock error
        format!("NOEVIA_WEB_REF=bad0{}\nNOEVIA_WEB_SHA256={h64}\nNOEVIA_CORE_REF=nope\n", &h40[4..]),
        String::new(),
    ];
    for lock in &fixed {
        for sha in ["11bf2da7", full] {
            compare(lock.as_bytes(), sha, full, dir.path());
        }
    }
}

#[test]
fn generated_locks_match_the_shell() {
    if bash().is_none() {
        eprintln!("shell_oracle: bash not installed, skipped");
        return;
    }
    let Some(dir) = tempfile::tempdir().ok() else {
        return;
    };
    let rounds: u64 = std::env::var("RELEASE_ASSEMBLE_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600);
    let mut rng = Lcg(0x5eed_0005);
    let full = "11bf2da7bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    for _ in 0..rounds {
        let lock = random_lock(&mut rng);
        compare(&lock, "11bf2da7", full, dir.path());
    }
}

#[test]
fn lock_value_matches_sed() {
    if bash().is_none() {
        return;
    }
    let Some(dir) = tempfile::tempdir().ok() else {
        return;
    };
    let lock_value = section("resolve")
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    let mut rng = Lcg(0x10c4);
    for _ in 0..200 {
        let lock = random_lock(&mut rng);
        let file = dir.path().join("lock");
        std::fs::write(&file, &lock).ok();
        let script = format!(
            "set -euo pipefail\nlock=\"$(cat -- \"$LOCK\")\"\n{lock_value}\nfor k in NOEVIA_WEB_REF NOEVIA_WEB_SHA256 NOEVIA_CORE_REF NOEVIA_SERVICES_REF NOEVIA_RS_REF NOEVIA_RS_SHA256; do printf '%s=[%s]\\n' \"$k\" \"$(lock_value $k)\"; done\n"
        );
        let out = Command::new("bash")
            .arg("-c")
            .arg(&script)
            .env("LOCK", &file)
            .output();
        let Ok(out) = out else { return };
        let l = Lock::from_bytes(&lock);
        let mut want = Vec::new();
        for k in [
            "NOEVIA_WEB_REF",
            "NOEVIA_WEB_SHA256",
            "NOEVIA_CORE_REF",
            "NOEVIA_SERVICES_REF",
            "NOEVIA_RS_REF",
            "NOEVIA_RS_SHA256",
        ] {
            want.extend_from_slice(format!("{k}=[").as_bytes());
            let mut v = l.value(k);
            while v.last() == Some(&b'\n') {
                v.pop();
            }
            want.extend_from_slice(&v);
            want.extend_from_slice(b"]\n");
        }
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&want),
            "lock:\n{:?}\nstderr: {}",
            String::from_utf8_lossy(&lock),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// When ASSEMBLE_RELEASE_SH points to the real script, every excerpt line must still be in it.
#[test]
fn excerpt_is_verbatim() {
    let Ok(path) = std::env::var("ASSEMBLE_RELEASE_SH") else {
        eprintln!("excerpt_is_verbatim: ASSEMBLE_RELEASE_SH not set, skipped");
        return;
    };
    let real = std::fs::read_to_string(path).unwrap_or_default();
    for section_name in ["resolve", "finish", "sources"] {
        let s = section(section_name);
        assert!(
            real.contains(s.trim_end()),
            "the {section_name} excerpt no longer appears verbatim"
        );
    }
}
