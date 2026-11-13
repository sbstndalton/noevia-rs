//! Replays `tests/fixtures/code-harness-config.v1.json`, recorded from noevia-core's own
//! `server/code-harness-config.cjs` by `tools/gen-fixtures.cjs` (synthetic inputs only):
//!
//! - `configFor` and `pinFilesFor` over a grid of harness ids, models, endpoints, keys and token
//!   limits: the exact files (base, path, bytes) and record fields, or the exact refusal;
//! - `dshPatch` directly, `cwdPinPaths` per harness id, the exported constants;
//! - `writeHarnessConfig` and `writePinned` in fresh temp dirs with pre-placed links, files and
//!   directories: the outcome, every `lchown` call (path, uid, gid), and the whole tree afterwards
//!   (type, mode & 0o777, bytes, link target), temp paths written as `<T>`.
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use code_harness_config::{
    claude_security_settings, config_for, cwd_pin_paths, dsh_patch, permission_json, pin_files_for,
    write_harness_config, write_pinned, Owner, PinError, PinOptions, WriteRequest, DSH_DISABLED,
    DSH_NO_ASK_TOOLS, EDIT_TOOLS, PERMISSION, PI_READ_TOOLS,
};
use js_json::JValue;

fn fixture() -> JValue {
    js_json::parse(include_str!("fixtures/code-harness-config.v1.json")).unwrap()
}

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(items) => items,
        other => panic!("not an array: {other:?}"),
    }
}

fn st(v: &JValue) -> Option<&str> {
    v.as_str()
}

fn num(v: &JValue) -> Option<f64> {
    match v {
        JValue::Undefined => None,
        JValue::Num(n) => Some(*n),
        JValue::Obj(_) => Some(match v.get("num").as_str().unwrap() {
            "NaN" => f64::NAN,
            "Infinity" => f64::INFINITY,
            "-Infinity" => f64::NEG_INFINITY,
            "-0" => -0.0,
            other => panic!("unknown number {other}"),
        }),
        other => panic!("not a token count: {other:?}"),
    }
}

fn opts(i: &JValue) -> PinOptions<'_> {
    PinOptions {
        harness: st(i.get("harness")),
        model: st(i.get("model")),
        engine: st(i.get("engine")),
        api_key: st(i.get("apiKey")),
        context_tokens: num(i.get("contextTokens")),
        output_tokens: num(i.get("outputTokens")),
    }
}

fn js(v: &JValue) -> String {
    js_json::stringify(v).unwrap()
}

fn contents(f: &JValue) -> Vec<&str> {
    arr(f.get("contents"))
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect()
}

fn idx(v: &JValue) -> usize {
    v.as_num().unwrap() as usize
}

#[test]
fn constants_match_node() {
    let f = fixture();
    let c = f.get("constants");
    assert_eq!(js(c.get("PERMISSION")), js(&permission_json()));
    assert_eq!(PERMISSION.len(), 3);
    let list = |v: &JValue| -> Vec<String> {
        arr(v)
            .iter()
            .map(|x| x.as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(list(c.get("EDIT_TOOLS")), EDIT_TOOLS);
    assert_eq!(list(c.get("PI_READ_TOOLS")), PI_READ_TOOLS);
    assert_eq!(list(c.get("DSH_NO_ASK_TOOLS")), DSH_NO_ASK_TOOLS);
    assert_eq!(list(c.get("DSH_DISABLED")), DSH_DISABLED);
    assert_eq!(
        c.get("claudeSecuritySettings").as_str().unwrap(),
        js(&claude_security_settings())
    );
}

#[test]
fn config_for_matches_node() {
    let f = fixture();
    let cases = arr(f.get("configFor"));
    assert!(cases.len() > 1000);
    for case in cases {
        let i = case.get("in");
        let got = config_for(&opts(i));
        match (case.get("out"), case.get("err"), got) {
            (JValue::Obj(_), _, Ok(cfg)) => {
                let want = case.get("out");
                assert_eq!(cfg.name, want.get("name").as_str().unwrap(), "{}", js(i));
                assert_eq!(
                    js(&cfg.json),
                    want.get("json").as_str().unwrap(),
                    "{}",
                    js(i)
                );
            }
            (_, JValue::Obj(_), Err(r)) => {
                let want = case.get("err");
                assert_eq!(
                    Some(f64::from(r.status)),
                    want.get("status").as_num(),
                    "{}",
                    js(i)
                );
                assert_eq!(
                    r.message,
                    want.get("message").as_str().unwrap(),
                    "{}",
                    js(i)
                );
            }
            (_, _, got) => panic!(
                "{}: node {} / {}, rust {got:?}",
                js(i),
                js(case.get("out")),
                js(case.get("err"))
            ),
        }
    }
}

#[test]
fn pin_files_for_matches_node() {
    let f = fixture();
    let text = contents(&f);
    let cases = arr(f.get("pinFilesFor"));
    let mut files = 0;
    for case in cases {
        let i = case.get("in");
        match (case.get("out"), pin_files_for(&opts(i))) {
            (JValue::Obj(_), Ok(p)) => {
                let want = case.get("out");
                assert_eq!(
                    p.harness,
                    want.get("harness").as_str().unwrap(),
                    "{}",
                    js(i)
                );
                let wf = arr(want.get("files"));
                assert_eq!(p.files.len(), wf.len(), "{}", js(i));
                for (g, w) in p.files.iter().zip(wf) {
                    assert_eq!(
                        g.base.as_str(),
                        w.get("base").as_str().unwrap(),
                        "{}",
                        js(i)
                    );
                    assert_eq!(g.path, w.get("path").as_str().unwrap(), "{}", js(i));
                    assert_eq!(
                        g.content,
                        text[idx(w.get("content"))],
                        "{} {}",
                        js(i),
                        g.path
                    );
                    files += 1;
                }
                assert_eq!(js(&permission_json()), js(want.get("permission")));
                assert_eq!(p.model, want.get("model").as_str().unwrap(), "{}", js(i));
                assert_eq!(
                    p.endpoint,
                    want.get("endpoint").as_str().unwrap(),
                    "{}",
                    js(i)
                );
            }
            (JValue::Undefined, Err(r)) => {
                let want = case.get("err");
                assert_eq!(
                    Some(f64::from(r.status)),
                    want.get("status").as_num(),
                    "{}",
                    js(i)
                );
                assert_eq!(
                    r.message,
                    want.get("message").as_str().unwrap(),
                    "{}",
                    js(i)
                );
            }
            (want, got) => panic!(
                "{}: node {} {}, rust {got:?}",
                js(i),
                js(want),
                js(case.get("err"))
            ),
        }
    }
    assert!(files > 1000, "{files}");
}

#[test]
fn dsh_patch_matches_node() {
    let f = fixture();
    let text = contents(&f);
    for case in arr(f.get("dshPatch")) {
        let i = case.get("in");
        let got = dsh_patch(
            i.get("name").as_str().unwrap(),
            i.get("baseURL").as_str().unwrap(),
            st(i.get("apiKey")),
            num(i.get("contextTokens")).unwrap(),
            num(i.get("outputTokens")).unwrap(),
        );
        assert_eq!(got, text[idx(case.get("out"))], "{}", js(i));
    }
}

#[test]
fn cwd_pin_paths_match_node() {
    let f = fixture();
    for case in arr(f.get("cwdPinPaths")) {
        let want: Vec<&str> = arr(case.get("out"))
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(
            cwd_pin_paths(st(case.get("harness"))),
            want,
            "{}",
            js(case.get("harness"))
        );
    }
}

// ── Writes ───────────────────────────────────────────────────────────────────────────────────

fn errno_name(e: &std::io::Error) -> &'static str {
    match e.raw_os_error() {
        Some(1) => "EPERM",
        Some(2) => "ENOENT",
        Some(13) => "EACCES",
        Some(17) => "EEXIST",
        Some(20) => "ENOTDIR",
        Some(21) => "EISDIR",
        Some(40) => "ELOOP",
        _ => "OTHER",
    }
}

fn fill(s: &str, t: &str) -> String {
    s.replace("<T>", t)
}

fn strip(s: &str, t: &str) -> String {
    s.replace(t, "<T>")
}

fn chmod(p: &Path, mode: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn setup(t: &Path, ops: &[JValue]) {
    let ts = t.to_str().unwrap();
    for o in ops {
        let p = t.join(o.get("path").as_str().unwrap());
        let mode = o.get("mode").as_num().map(|m| m as u32);
        match o.get("op").as_str().unwrap() {
            "dir" => {
                std::fs::create_dir(&p).unwrap();
                chmod(&p, mode.unwrap());
            }
            "file" => {
                std::fs::write(&p, o.get("content").as_str().unwrap()).unwrap();
                chmod(&p, mode.unwrap());
            }
            "link" => {
                std::os::unix::fs::symlink(fill(o.get("target").as_str().unwrap(), ts), &p).unwrap()
            }
            "chmod" => {}
            other => panic!("unknown op {other}"),
        }
    }
    for o in ops.iter().filter(|o| o.get("op").as_str() == Some("chmod")) {
        chmod(
            &t.join(o.get("path").as_str().unwrap()),
            o.get("mode").as_num().unwrap() as u32,
        );
    }
}

fn restore(t: &Path, ops: &[JValue]) {
    for o in ops.iter().filter(|o| o.get("op").as_str() == Some("chmod")) {
        chmod(&t.join(o.get("path").as_str().unwrap()), 0o700);
    }
}

/// The tree as the generator lists it: `path type mode|target content`, one line per entry.
fn tree(t: &Path) -> Vec<String> {
    fn walk(t: &Path, rel: &str, out: &mut Vec<String>) {
        let ts = t.to_str().unwrap();
        let abs = if rel.is_empty() {
            t.to_path_buf()
        } else {
            t.join(rel)
        };
        let mut names: Vec<String> = std::fs::read_dir(&abs)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        for name in names {
            let r = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let p = t.join(&r);
            let md = std::fs::symlink_metadata(&p).unwrap();
            if md.file_type().is_symlink() {
                let target = std::fs::read_link(&p).unwrap();
                out.push(format!("{r} link {}", strip(target.to_str().unwrap(), ts)));
            } else if md.is_dir() {
                out.push(format!("{r} dir {:o}", md.permissions().mode() & 0o777));
                walk(t, &r, out);
            } else {
                let body = std::fs::read_to_string(&p).unwrap();
                out.push(format!(
                    "{r} file {:o} {body:?}",
                    md.permissions().mode() & 0o777
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(t, "", &mut out);
    out.sort();
    out
}

fn tree_of(entries: &JValue) -> Vec<String> {
    let mut out: Vec<String> = arr(entries)
        .iter()
        .map(|e| {
            let r = e.get("path").as_str().unwrap();
            match e.get("type").as_str().unwrap() {
                "link" => format!("{r} link {}", e.get("target").as_str().unwrap()),
                "dir" => format!("{r} dir {:o}", e.get("mode").as_num().unwrap() as u32),
                _ => format!(
                    "{r} file {:o} {:?}",
                    e.get("mode").as_num().unwrap() as u32,
                    e.get("content").as_str().unwrap()
                ),
            }
        })
        .collect();
    out.sort();
    out
}

/// Permission bits stop this process (it is not root and has no override).
fn unprivileged() -> bool {
    let dir = tempfile::tempdir().unwrap();
    let ro = dir.path().join("ro");
    std::fs::create_dir(&ro).unwrap();
    chmod(&ro, 0o500);
    let stopped = std::fs::create_dir(ro.join("x")).is_err();
    chmod(&ro, 0o700);
    stopped
}

#[test]
fn writes_match_node() {
    let f = fixture();
    let unprivileged = unprivileged();
    let cases = arr(f.get("writes"));
    assert!(cases.len() > 50);
    let mut checked = 0;
    for case in cases {
        let name = case.get("name").as_str().unwrap();
        let ops = arr(case.get("setup"));
        if !unprivileged && ops.iter().any(|o| o.get("op").as_str() == Some("chmod")) {
            // Root ignores the permission bits these cases rely on.
            continue;
        }
        let dir = tempfile::Builder::new()
            .prefix("noevia-hconf-")
            .tempdir()
            .unwrap();
        let t = dir.path();
        let ts = t.to_str().unwrap().to_owned();
        setup(t, ops);
        let fail_at = case.get("failChownAt").as_num().map(|n| n as usize);
        let mut chowns: Vec<String> = Vec::new();
        let mut chown = |p: &Path, uid: u32, gid: u32| -> std::io::Result<()> {
            chowns.push(format!("{} {uid} {gid}", strip(p.to_str().unwrap(), &ts)));
            if fail_at == Some(chowns.len()) {
                return Err(std::io::Error::from_raw_os_error(1));
            }
            Ok(())
        };
        let result: Result<Option<JValue>, PinError> = if let JValue::Obj(_) = case.get("pinned") {
            let p = case.get("pinned");
            let root = fill(p.get("root").as_str().unwrap(), &ts);
            write_pinned(
                Path::new(&root),
                p.get("relative").as_str().unwrap(),
                p.get("content").as_str().unwrap(),
                None,
                &mut chown,
            )
            .map(|()| None)
        } else {
            let c = case.get("call");
            let cwd = st(c.get("cwd")).map(|s| fill(s, &ts));
            let home = st(c.get("home")).map(|s| fill(s, &ts));
            let owner = match c.get("owner") {
                JValue::Obj(_) => Some(Owner {
                    uid: c.get("owner").get("uid").as_num().unwrap() as u32,
                    gid: c.get("owner").get("gid").as_num().unwrap() as u32,
                }),
                _ => None,
            };
            let req = WriteRequest {
                cwd: cwd.as_deref().map(Path::new),
                home: home.as_deref().map(Path::new),
                owner,
                options: opts(c.get("options")),
            };
            write_harness_config(&req, &mut chown).map(|r| Some(r.to_json()))
        };
        let want = case.get("result");
        match (want.get("ok"), want.get("err"), &result) {
            (JValue::Undefined, JValue::Undefined, _) => panic!("{name}: no result recorded"),
            (JValue::Undefined, e, Err(got)) => {
                if let JValue::Str(code) = e.get("code") {
                    let PinError::Io {
                        syscall, source, ..
                    } = got
                    else {
                        panic!("{name}: {got:?}")
                    };
                    assert_eq!(errno_name(source), code, "{name}");
                    assert_eq!(*syscall, e.get("syscall").as_str().unwrap(), "{name}");
                } else {
                    assert_eq!(
                        got.status().map(f64::from),
                        e.get("status").as_num(),
                        "{name}"
                    );
                    assert_eq!(
                        got.to_string(),
                        e.get("message").as_str().unwrap(),
                        "{name}"
                    );
                }
            }
            (JValue::Null, _, Ok(None)) => {}
            (ok @ JValue::Obj(_), _, Ok(Some(got))) => assert_eq!(js(got), js(ok), "{name}"),
            (ok, err, got) => panic!("{name}: node {} {}, rust {got:?}", js(ok), js(err)),
        }
        let want_chowns: Vec<String> = arr(case.get("chowns"))
            .iter()
            .map(|c| {
                let c = arr(c);
                format!(
                    "{} {} {}",
                    c[0].as_str().unwrap(),
                    c[1].as_num().unwrap(),
                    c[2].as_num().unwrap()
                )
            })
            .collect();
        assert_eq!(chowns, want_chowns, "{name}");
        restore(t, ops);
        assert_eq!(tree(t), tree_of(case.get("tree")), "{name}");
        checked += 1;
    }
    assert!(checked > 50, "{checked}");
}
