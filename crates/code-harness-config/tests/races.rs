//! The descriptor walk (named difference `fd-walk`): links swapped in after a check, the real
//! hand-over never following a link, and links already at the target. Deterministic: every swap
//! happens inside the hand-over hook, between the check of a component and the walk entering it.
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::ffi::OsStr;
use std::fs;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use code_harness_config::{
    write_harness_config, write_pinned, Chown, Handover, Owner, PinError, PinOptions, WriteRequest,
};

/// Changes no owner; runs `on_dir` with the full path of each directory just made.
struct Swap<F: FnMut(&Path)> {
    on_dir: F,
    files: Vec<PathBuf>,
}

impl<F: FnMut(&Path)> Handover for Swap<F> {
    fn directory(
        &mut self,
        _: BorrowedFd<'_>,
        _: &OsStr,
        path: &Path,
        _: Owner,
    ) -> std::io::Result<()> {
        (self.on_dir)(path);
        Ok(())
    }
    fn file(&mut self, _: BorrowedFd<'_>, path: &Path, _: Owner) -> std::io::Result<()> {
        self.files.push(path.to_path_buf());
        Ok(())
    }
}

fn me(p: &Path) -> Owner {
    let m = fs::metadata(p).unwrap();
    Owner {
        uid: m.uid(),
        gid: m.gid(),
    }
}

/// Every entry under `dir`, recursively, without following links.
fn listing(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            out.push(p.strip_prefix(dir).unwrap().to_string_lossy().into_owned());
            if fs::symlink_metadata(&p).unwrap().is_dir() {
                stack.push(p);
            }
        }
    }
    out.sort();
    out
}

fn layout() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let (cwd, home, outside) = (
        t.path().join("cwd"),
        t.path().join("home"),
        t.path().join("outside"),
    );
    for d in [&cwd, &home, &outside] {
        fs::create_dir(d).unwrap();
    }
    fs::write(outside.join("keep.txt"), "outside\n").unwrap();
    (t, cwd, home, outside)
}

const OPTS: PinOptions<'static> = PinOptions {
    harness: Some("claude-code"),
    model: Some("m"),
    engine: Some("http://engine.invalid/v1"),
    api_key: Some("synthetic-key"),
    context_tokens: None,
    output_tokens: None,
};

#[test]
fn a_link_planted_after_the_directory_check_is_refused() {
    let (_t, cwd, home, outside) = layout();
    let target = outside.clone();
    let mut hook = Swap {
        on_dir: |made: &Path| {
            // The harness user, now owning `.claude`, replaces it with a link out of the tree.
            if made.ends_with(".claude") && made.starts_with(&cwd) {
                fs::rename(made, made.with_file_name(".claude.moved")).unwrap();
                symlink(&target, made).unwrap();
            }
        },
        files: Vec::new(),
    };
    let req = WriteRequest {
        cwd: Some(&cwd),
        home: Some(&home),
        owner: Some(Owner { uid: 1, gid: 1 }),
        options: OPTS,
    };
    let err = write_harness_config(&req, &mut hook).unwrap_err();
    assert!(matches!(err, PinError::Refused(_)), "{err:?}");
    assert_eq!(err.status(), Some(409));
    assert_eq!(
        err.to_string(),
        "Refused to pin the harness configuration: .claude is a symbolic link."
    );
    assert_eq!(
        listing(&outside),
        ["keep.txt"],
        "nothing written outside the tree"
    );
    assert_eq!(
        fs::read_to_string(outside.join("keep.txt")).unwrap(),
        "outside\n"
    );
    assert!(listing(&cwd.join(".claude.moved")).is_empty());
    assert!(hook.files.is_empty(), "nothing handed over");
    assert!(
        listing(&home).is_empty(),
        "the walk stopped at the first file"
    );
}

#[test]
fn an_ancestor_swapped_after_the_walk_entered_it_is_not_followed() {
    let (_t, cwd, _home, outside) = layout();
    fs::create_dir(cwd.join("a")).unwrap();
    let (a, out) = (cwd.join("a"), outside.clone());
    let mut hook = Swap {
        on_dir: |made: &Path| {
            // `a/b` was just made, `a` is already held: move `a` away and point its name outside,
            // where a matching `b` waits for a write by path.
            if made.ends_with("a/b") {
                fs::rename(&a, a.with_file_name("a.moved")).unwrap();
                fs::create_dir(out.join("b")).unwrap();
                symlink(&out, &a).unwrap();
            }
        },
        files: Vec::new(),
    };
    write_pinned(
        &cwd,
        "a/b/f",
        "pinned\n",
        Some(Owner { uid: 1, gid: 1 }),
        &mut hook,
    )
    .unwrap();
    assert_eq!(
        listing(&outside),
        ["b", "keep.txt"],
        "no file written through the swapped link"
    );
    assert_eq!(
        fs::read_to_string(cwd.join("a.moved/b/f")).unwrap(),
        "pinned\n"
    );
    assert_eq!(
        hook.files,
        [cwd.join("a/b/f")],
        "the file was handed over (through its descriptor)"
    );
}

#[test]
fn a_link_at_the_target_is_refused_never_written_through() {
    let (_t, cwd, _home, outside) = layout();
    symlink(outside.join("keep.txt"), cwd.join("opencode.json")).unwrap();
    symlink(outside.join("new.txt"), cwd.join("dangling.json")).unwrap();
    let mut hook = Swap {
        on_dir: |_: &Path| {},
        files: Vec::new(),
    };
    for name in ["opencode.json", "dangling.json"] {
        let err = write_pinned(&cwd, name, "pinned\n", None, &mut hook).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("Refused to pin the harness configuration: {name} is a symbolic link.")
        );
        assert!(
            fs::symlink_metadata(cwd.join(name))
                .unwrap()
                .file_type()
                .is_symlink(),
            "left in place"
        );
    }
    assert_eq!(listing(&outside), ["keep.txt"]);
    assert_eq!(
        fs::read_to_string(outside.join("keep.txt")).unwrap(),
        "outside\n"
    );
}

fn ctime(p: &Path) -> (i64, i64) {
    let m = fs::symlink_metadata(p).unwrap();
    (m.ctime(), m.ctime_nsec())
}

#[test]
fn the_real_hand_over_never_follows_a_link() {
    let t = tempfile::tempdir().unwrap();
    let target = t.path().join("target");
    fs::write(&target, "x").unwrap();
    let link = t.path().join("link");
    symlink(&target, &link).unwrap();
    let owner = me(&target);
    let dir = fs::File::open(t.path()).unwrap();
    let (target_before, link_before) = (ctime(&target), ctime(&link));
    std::thread::sleep(Duration::from_millis(50));
    Chown
        .directory(dir.as_fd(), OsStr::new("link"), &link, owner)
        .unwrap();
    assert_eq!(
        ctime(&target),
        target_before,
        "the link's target was not touched"
    );
    assert_ne!(
        ctime(&link),
        link_before,
        "the link itself was (the check can see a change)"
    );

    // The file hand-over goes through the descriptor it is given.
    let file = fs::File::open(&target).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    Chown.file(file.as_fd(), &link, owner).unwrap();
    assert_ne!(ctime(&target), target_before);
}

#[test]
fn the_real_hand_over_writes_end_to_end() {
    let (_t, cwd, home, _outside) = layout();
    let req = WriteRequest {
        cwd: Some(&cwd),
        home: Some(&home),
        owner: Some(me(&cwd)),
        options: OPTS,
    };
    let record = write_harness_config(&req, &mut Chown).unwrap();
    assert_eq!(
        record.files,
        ["./.claude/settings.local.json", "~/.claude/settings.json"]
    );
    let body = fs::read_to_string(home.join(".claude/settings.json")).unwrap();
    assert!(body.contains("\"ANTHROPIC_API_KEY\": \"synthetic-key\""));
}

#[test]
fn debug_output_leaves_the_key_out() {
    let (_t, cwd, home, _outside) = layout();
    let req = WriteRequest {
        cwd: Some(&cwd),
        home: Some(&home),
        owner: None,
        options: OPTS,
    };
    let pinned = code_harness_config::pin_files_for(&OPTS).unwrap();
    let config = code_harness_config::config_for(&PinOptions {
        harness: Some("opencode"),
        ..OPTS
    })
    .unwrap();
    for text in [
        format!("{req:?}"),
        format!("{OPTS:?}"),
        format!("{pinned:?}"),
        format!("{config:?}"),
    ] {
        assert!(!text.contains("synthetic-key"), "{text}");
    }
}
