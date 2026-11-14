//! The pairing guards' shared primitives against the verbatim shell functions
//! (`fixtures/assemble-release-guards-excerpt.sh`): every generated input runs under bash (with `LC_ALL=C`,
//! as the sorted outputs assume) and in Rust, and stdout, stderr and the exit status must be equal.
//! `RELEASE_ASSEMBLE_GUARD_ROUNDS` sets the number of generated inputs per primitive (default 400).
#![cfg(unix)]
#![allow(clippy::indexing_slicing)]

mod common;
use release_assemble::guard::{self, MoveKind};
use std::fs;
use std::path::Path;
use std::process::Command;

const EXCERPT: &str = include_str!("../fixtures/assemble-release-guards-excerpt.sh");

fn have_bash() -> bool {
    Command::new("bash")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn rounds() -> u64 {
    std::env::var("RELEASE_ASSEMBLE_GUARD_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400)
}

/// stdout, stderr, status of `bash -c '. EXCERPT; <script>' EXCERPT args...`.
fn shell(excerpt: &Path, script: &str, args: &[&[u8]]) -> (Vec<u8>, Vec<u8>, i32) {
    use std::os::unix::ffi::OsStrExt;
    let mut cmd = Command::new("bash");
    cmd.env("LC_ALL", "C")
        .arg("-c")
        .arg(format!(". \"$0\"\n{script}"))
        .arg(excerpt);
    for a in args {
        cmd.arg(std::ffi::OsStr::from_bytes(a));
    }
    match cmd.output() {
        Ok(o) => (o.stdout, o.stderr, o.status.code().unwrap_or(-1)),
        Err(e) => (Vec::new(), e.to_string().into_bytes(), -2),
    }
}

/// xorshift64*: the same inputs on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
    fn chance(&mut self, pct: usize) -> bool {
        self.below(100) < pct
    }
}

fn show(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn lines_of(v: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for l in v {
        out.extend_from_slice(l);
        out.push(b'\n');
    }
    out
}

const HEADERS: &[&str] = &[
    "[[route]]",
    "[[ route ]]",
    "[[\troute\t]]",
    "[route]",
    "[[route]",
    "[route]]",
    "[[route]]]",
    "[meta]",
    "[[other]]",
    "[route.x]",
    "[[ route . sub ]]",
    "[\"route\"]",
    "[[\"route\"]]",
    "[[route]] # a comment",
    "[[route]] trailing",
    "[a.b]",
    "[ a ]",
    "[a . b . c]",
    "[]",
    "[[]]",
    "[ok-name_1]",
];

const KEYS: &[&str] = &[
    "path",
    "owner",
    "switch",
    "methods",
    "\"path\"",
    "'owner'",
    "\"switch\"",
    "other",
    "a.b",
    "path ",
    "\"pa th\"",
    "'methods'",
    "-",
    "\"\"",
];

const VALUES: &[&str] = &[
    "\"/api/projects/{id}\"",
    "\"/api/projects/{id}/files/{*}\"",
    "'/api/freechats'",
    "\"/api/freechats/{id}\"",
    "\"/api/freechats/{id}/x\"",
    "\"/api/chats/{c}/move\"",
    "\"/api/chats/{c}/context\"",
    "\"/api/chats/{c}/other\"",
    "\"/api/other/{x}\"",
    "\"/api/projects/{unclosed\"",
    "rust",
    "\"rust\"",
    "'rust'",
    "\"node\"",
    "\"\"",
    "''",
    "",
    "\"NOEVIA_RUST_X\"",
    "\"GET\"",
    "[\"GET\", \"POST\"]",
    "[ 'GET' ]",
    "[\"GET\",\"put\",\"DELETE\"]",
    "[]",
    "[ \"GET\" ] # c",
    "[ \"GET\" ] x",
    "[[\"GET\"], [\"POST\"]]",
    "[\"a#b\", 'c]d']",
    "[\"GET\",",
    "[",
    "[ # only a comment",
    "\"unterminated",
    "'unterminated",
    "\"a\\b\"",
    "[\"a\\b\"]",
    "\"x\" # c",
    "\"x\"#c",
    "\"x\" y",
    "\"\"\"multi\"\"\"",
    "'''multi'''",
    "GET # bare with comment",
    "bare value",
    "{ inline = 1 }",
    "\"tab\there\"",
    "= \"x\"",
];

const CONT: &[&str] = &[
    "\"POST\",",
    "'PATCH'",
    "]",
    "] # end",
    "] junk",
    "\"DELETE\" ]",
    "]]",
    "\"a#b\" ]",
    "# comment inside",
    "",
    "[\"nested\"]",
    "\"bad\\escape\"",
    "\"open",
    "\"\"\"",
    "[[route]]",
];

const JUNK: &[&str] = &[
    "",
    "# comment",
    "   # indented comment",
    "x = 1",
    "route = [ { path = \"/a\" } ]",
    "route.path = \"/a\"",
    "just text",
    "\"quoted\" = 1",
    "=",
    "a = { b = 1 }",
    "a.b = 2",
];

fn gen_routes_toml(r: &mut Rng) -> Vec<u8> {
    let mut s = String::new();
    if r.chance(5) {
        s.push('\u{feff}');
    }
    let pad = |r: &mut Rng| -> &'static str {
        match r.below(6) {
            0 => "  ",
            1 => "\t",
            _ => "",
        }
    };
    let eol = |r: &mut Rng| -> &'static str {
        if r.chance(5) {
            "\r\n"
        } else {
            "\n"
        }
    };
    // Top-level lines before any header.
    for _ in 0..r.below(3) {
        let l = r.pick(JUNK);
        s.push_str(l);
        s.push_str(eol(r));
    }
    let tables = r.below(6);
    for _ in 0..tables {
        let h = if r.chance(80) {
            "[[route]]"
        } else {
            r.pick(HEADERS)
        };
        s.push_str(pad(r));
        s.push_str(h);
        s.push_str(eol(r));
        for _ in 0..1 + r.below(5) {
            if r.chance(10) {
                s.push_str(r.pick(JUNK));
                s.push_str(eol(r));
                continue;
            }
            let good = r.chance(75);
            let key = if good {
                ["path", "owner", "switch", "methods"][r.below(4)]
            } else {
                r.pick(KEYS)
            };
            let value = if good {
                match key {
                    "path" => VALUES[r.below(10)],
                    "owner" => ["\"rust\"", "\"node\"", "rust", "'rust'"][r.below(4)],
                    "switch" => ["\"NOEVIA_RUST_X\"", "\"\"", "''"][r.below(3)],
                    _ => VALUES[18 + r.below(9)],
                }
            } else {
                r.pick(VALUES)
            };
            let eq = ["=", " = ", "\t=\t", " =", "=  "][r.below(5)];
            s.push_str(pad(r));
            s.push_str(key);
            s.push_str(eq);
            s.push_str(value);
            s.push_str(eol(r));
            // An open array continues on the next lines.
            if value.starts_with('[') && !value.contains(']') {
                for _ in 0..r.below(4) {
                    s.push_str(pad(r));
                    s.push_str(r.pick(CONT));
                    s.push_str(eol(r));
                }
            }
        }
    }
    let mut b = s.into_bytes();
    if r.chance(10) {
        while b.last() == Some(&b'\n') || b.last() == Some(&b'\r') {
            b.pop();
        }
    }
    b
}

#[test]
fn routes_toml_matches_the_shell() {
    if !have_bash() {
        common::skip("guard_primitives: bash is not installed");
        return;
    }
    let Some(dir) = tempfile::tempdir().ok() else {
        return;
    };
    let exc = dir.path().join("excerpt.sh");
    assert!(fs::write(&exc, EXCERPT).is_ok());
    let file = dir.path().join("routes.toml");
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let (mut ok, mut refused, mut project_lines, mut switched_lines) = (0u64, 0u64, 0u64, 0u64);
    let mut reasons = std::collections::BTreeSet::new();
    let mut inputs: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"\n".to_vec(),
        b"[[route]]\n".to_vec(),
        b"[[route]]\npath = \"/api/projects/{id}\"\nowner = \"rust\"\n".to_vec(),
        b"[[route]]\nmethods = [\n  \"GET\",\n".to_vec(),
        b"\xef\xbb\xbf[[route]]\npath='/api/freechats'\nowner='rust'\nmethods=['GET','POST']\n"
            .to_vec(),
    ];
    for _ in 0..rounds() * 3 {
        inputs.push(gen_routes_toml(&mut r));
    }
    for input in &inputs {
        assert!(fs::write(&file, input).is_ok());
        let fpath = file.as_os_str().as_encoded_bytes();
        let (out, err, status) = shell(&exc, "routes_toml \"$1\"", &[fpath]);
        let got = guard::routes_toml(input);
        let want_err = got
            .error
            .as_ref()
            .map(|e| format!("{e}\n").into_bytes())
            .unwrap_or_default();
        let want_status = if got.error.is_some() { 2 } else { 0 };
        assert_eq!(
            (show(&out), show(&err), status),
            (show(&got.stdout()), show(&want_err), want_status),
            "routes_toml over:\n{}",
            show(input)
        );
        match &got.error {
            Some(e) => {
                refused += 1;
                let reason = e.split_once(": ").map(|x| x.1).unwrap_or_default();
                reasons.insert(reason.chars().take(24).collect::<String>());
            }
            None => ok += 1,
        }
        // The two filters over the same file (their output, as the pipelines print it).
        let (fout, _, _) = shell(
            &exc,
            "project_routes_rs \"$1\"; echo ---; switched_routes \"$1\"",
            &[fpath],
        );
        let pr = guard::project_routes_rs_shell(&got);
        let sw = guard::switched_routes(&got);
        project_lines += pr.len() as u64;
        switched_lines += sw.len() as u64;
        let mut want = lines_of(&pr);
        want.extend_from_slice(b"---\n");
        want.extend_from_slice(&lines_of(&sw));
        assert_eq!(
            show(&fout),
            show(&want),
            "project_routes_rs/switched_routes over:\n{}",
            show(input)
        );
        if got.error.is_none() {
            assert_eq!(guard::project_routes_rs(&got), Some(pr));
        } else {
            assert_eq!(guard::project_routes_rs(&got), None);
        }
    }
    eprintln!(
        "COUNTS routes_toml inputs={} ok={ok} refused={refused} reasons={} project_lines={project_lines} switched_lines={switched_lines}",
        inputs.len(),
        reasons.len()
    );
    // The generator must reach both outcomes, many refusal reasons and both filters.
    assert!(
        ok * 10 >= inputs.len() as u64,
        "too few accepted inputs: {ok}"
    );
    assert!(
        refused * 10 >= inputs.len() as u64,
        "too few refusals: {refused}"
    );
    assert!(reasons.len() >= 10, "refusal reasons reached: {reasons:?}");
    assert!(project_lines > 0 && switched_lines > 0);
}

const MOVE_LINES: &[&str] = &[
    "pub const MOVE_MARKER: &str = \"noevia-x/1\";",
    "  pub const MOVE_MARKER: &str = \"noevia-x/1\";  ",
    "pub const MOVE_MARKER: &str = \"noevia-x/2\";",
    "pub const MOVE_MARKER: &str = 'noevia-x/1';",
    "const MOVE_MARKER = 'noevia-x/1';",
    "const MOVE_MARKER = \"noevia-x/1\";",
    "const OTHER = 'noevia-x/1';",
    "async function MOVE_MARKER(req, res) {",
    "async function MOVE_MARKERx(req) {",
    "function MOVE_MARKER() {",
    "pub const FEATURES: &[&str] = &[",
    "pub const FEATURES: &[&str] = &[\"noevia-x/1\", \"other\"];",
    "pub const FEATURES: &[&str] = &[crate::x::MOVE_MARKER, \"a\"",
    "\"noevia-x/1\",",
    "  \"noevia-x/1\" , \"b\",",
    "noevia-x/1,",
    "crate::x::MOVE_MARKER,",
    "\"other\",",
    "];",
    "\"noevia-x/1\"];",
    "module.exports = {",
    "module.exports = { MOVE_MARKER, other };",
    "module.exports = { other }",
    "  MOVE_MARKER,",
    "  MOVE_MARKER",
    "MOVE_MARKER, x }",
    "};",
    "}",
    "// pub const MOVE_MARKER: &str = \"noevia-x/1\";",
    "/* pub const MOVE_MARKER: &str = \"noevia-x/1\"; */",
    "/*",
    " * MOVE_MARKER,",
    "*/",
    "x /* inline",
    "end */ pub const MOVE_MARKER: &str = \"noevia-x/1\";",
    "",
    ",",
    "\t",
];

#[test]
fn move_capability_matches_the_shell() {
    if !have_bash() {
        common::skip("guard_primitives: bash is not installed");
        return;
    }
    let Some(dir) = tempfile::tempdir().ok() else {
        return;
    };
    let exc = dir.path().join("excerpt.sh");
    assert!(fs::write(&exc, EXCERPT).is_ok());
    let file = dir.path().join("module.src");
    let mut r = Rng(0xD1B5_4A32_D192_ED03);
    let kinds = ["rust", "js", "function", "features", "feature_ref"];
    let mut trues = [0u64; 5];
    let script = "for k in rust js function features feature_ref; do move_capability \"$1\" \"$2\" \"$3\" $k; printf '%s=%s\\n' $k $?; done";
    let n = rounds() * 2;
    for case in 0..n {
        let mut s = String::new();
        for _ in 0..r.below(9) {
            s.push_str(r.pick(MOVE_LINES));
            s.push_str(if r.chance(5) { "\r\n" } else { "\n" });
        }
        let (name, value) = match r.below(4) {
            0 => ("MOVE_MARKER", "crate::x::MOVE_MARKER"),
            1 => ("MOVE_MARKER", "-"),
            _ => ("MOVE_MARKER", "noevia-x/1"),
        };
        let input = s.into_bytes();
        let missing = case % 97 == 0;
        if missing {
            fs::remove_file(&file).ok();
        } else {
            assert!(fs::write(&file, &input).is_ok());
        }
        let fpath = file.as_os_str().as_encoded_bytes();
        let (out, _, _) = shell(&exc, script, &[fpath, name.as_bytes(), value.as_bytes()]);
        let mut want = String::new();
        for (i, k) in kinds.iter().enumerate() {
            let kind = MoveKind::parse(k);
            assert!(kind.is_some());
            let Some(kind) = kind else { continue };
            let src = (!missing).then_some(input.as_slice());
            let yes = guard::move_capability(src, name, value, kind);
            if yes {
                trues[i] += 1;
            }
            want.push_str(&format!("{k}={}\n", if yes { 0 } else { 1 }));
        }
        assert_eq!(
            show(&out),
            want,
            "move_capability {name} {value} over:\n{}",
            show(&input)
        );
    }
    eprintln!("COUNTS move_capability inputs={n} true_per_kind={trues:?}");
    assert!(
        trues.iter().all(|t| *t > 0),
        "every kind must be true at least once: {trues:?}"
    );
}

const CORE_LINES: &[&str] = &[
    "const OWNED_ROUTES = new Set([",
    "const OWNED_ROUTES = new Set(['GET /api/projects/{id}']));",
    "const OTHER_ROUTES = new Set([",
    "  const OWNED_ROUTES = new Set([",
    "  'GET /api/projects/{id}',",
    "  'POST /api/chats/{chatId}/move', 'DELETE /api/freechats/{id}',",
    "  'GET /api/x/{a}/{b}', 'PUT /api/y/{unclosed',",
    "  'get /api/lower',",
    "  ' /api/no-method',",
    "  'GET api/no-slash',",
    "  \"GET /api/double-quoted\",",
    "  'GET /api/unterminated",
    "  'GET /api/a''POST /api/b'",
    "  'GET /api/same',",
    "  'GET /api/same',",
    "]));",
    "  ]));  // end",
    "]);",
    "// 'GET /api/commented'",
    "",
];

#[test]
fn project_routes_core_matches_the_shell() {
    if !have_bash() {
        common::skip("guard_primitives: bash is not installed");
        return;
    }
    let Some(dir) = tempfile::tempdir().ok() else {
        return;
    };
    let exc = dir.path().join("excerpt.sh");
    assert!(fs::write(&exc, EXCERPT).is_ok());
    let file = dir.path().join("rust-projects.cjs");
    let mut r = Rng(0x2545_F491_4F6C_DD1D);
    let mut nonempty = 0u64;
    let n = rounds();
    for case in 0..n {
        let mut s = String::new();
        for _ in 0..r.below(10) {
            s.push_str(r.pick(CORE_LINES));
            s.push('\n');
        }
        let name = if r.chance(80) {
            "OWNED_ROUTES"
        } else {
            "OTHER_ROUTES"
        };
        let input = s.into_bytes();
        let missing = case % 53 == 0;
        if missing {
            fs::remove_file(&file).ok();
        } else {
            assert!(fs::write(&file, &input).is_ok());
        }
        let fpath = file.as_os_str().as_encoded_bytes();
        let (out, _, _) = shell(
            &exc,
            "project_routes_core \"$1\" \"$2\"",
            &[fpath, name.as_bytes()],
        );
        let got = guard::project_routes_core((!missing).then_some(input.as_slice()), name);
        if !got.is_empty() {
            nonempty += 1;
        }
        assert_eq!(
            show(&out),
            show(&lines_of(&got)),
            "project_routes_core {name} over:\n{}",
            show(&input)
        );
    }
    eprintln!("COUNTS project_routes_core inputs={n} nonempty={nonempty}");
    assert!(nonempty * 10 >= n, "too few inputs with routes: {nonempty}");
}

const CODE_LINES: &[&str] = &[
    "const a = require('./x.cjs');",
    "  // a comment naming require('./x.cjs')",
    "/* block opens",
    "   still in the block require",
    "   closes */ require after close",
    "/* one-line block */",
    " * star line",
    "# hash line",
    "\t# tab hash",
    "code // trailing comment",
    "x = '/*' // not at the start",
    "*/",
    "",
    "   ",
    "\x0b vertical tab start // ",
    "needle here",
    "/*/ odd",
];

#[test]
fn code_lines_and_code_has_match_the_shell() {
    if !have_bash() {
        common::skip("guard_primitives: bash is not installed");
        return;
    }
    let Some(dir) = tempfile::tempdir().ok() else {
        return;
    };
    let exc = dir.path().join("excerpt.sh");
    assert!(fs::write(&exc, EXCERPT).is_ok());
    let file = dir.path().join("module.cjs");
    let mut r = Rng(0x0BAD_5EED_1234_5678);
    let needles = ["require", "needle", "close", "star", "block", "a = req"];
    let (mut has_true, mut has_false) = (0u64, 0u64);
    let n = rounds();
    for case in 0..n {
        let mut s = String::new();
        for _ in 0..r.below(9) {
            s.push_str(r.pick(CODE_LINES));
            s.push_str(if r.chance(5) { "\r\n" } else { "\n" });
        }
        let mut input = s.into_bytes();
        if r.chance(15) {
            while input.last() == Some(&b'\n') {
                input.pop();
            }
        }
        let missing = case % 61 == 0;
        if missing {
            fs::remove_file(&file).ok();
        } else {
            assert!(fs::write(&file, &input).is_ok());
        }
        let needle = r.pick(&needles);
        let fpath = file.as_os_str().as_encoded_bytes();
        let (out, _, _) = shell(
            &exc,
            "code_lines \"$1\"; echo \"---\"; code_has \"$1\" \"$2\"; echo \"has=$?\"",
            &[fpath, needle.as_bytes()],
        );
        let src = (!missing).then_some(input.as_slice());
        let has = guard::code_has_fixed(src, needle.as_bytes());
        if has {
            has_true += 1;
        } else {
            has_false += 1;
        }
        let mut want = guard::code_lines(src);
        want.extend_from_slice(format!("---\nhas={}\n", if has { 0 } else { 1 }).as_bytes());
        assert_eq!(
            show(&out),
            show(&want),
            "code_lines/code_has {needle:?} over:\n{}",
            show(&input)
        );
    }
    eprintln!("COUNTS code_has inputs={n} true={has_true} false={has_false}");
    assert!(has_true > 0 && has_false > 0);
}

/// Hand-written cases from the script's own comments (routes.toml shapes it must refuse or read).
#[test]
fn routes_toml_named_cases() {
    let ok = guard::routes_toml(
        b"# c\n[[ route ]]\n\"path\" = '/api/projects/{id}' # c\nowner\t=\t\"rust\"\nmethods = [\n 'GET', # c\n \"POST\" ]\n[[route]]\npath = \"/api/x\"\nswitch = \"\"\n",
    );
    assert_eq!(ok.error, None);
    assert_eq!(
        String::from_utf8_lossy(&ok.stdout()),
        "/api/projects/{id}\trust\t-\tGET,  POST\n/api/x\t-\t?\t-\n"
    );
    assert_eq!(
        guard::project_routes_rs(&ok),
        Some(vec![
            b"GET /api/projects/{}".to_vec(),
            b"POST /api/projects/{}".to_vec()
        ])
    );
    assert_eq!(guard::switched_routes(&ok), vec![b"? /api/x".to_vec()]);
    for (src, want) in [
        (
            "[[\"route\"]]\n",
            "line 1: table header is not a plain bare name",
        ),
        ("route = [ {path = 1} ]\n", "line 1: a top-level route key"),
        ("[[route]]\na.b = 1\n", "line 2: cannot classify this line"),
        (
            "[[route]]\npath = \"\"\"x\"\"\"\n",
            "line 2: multi-line strings",
        ),
        (
            "[[route]]\nmethods = [\n\"GET\"\n",
            "line 3: unterminated array",
        ),
        ("[route]\n", "line 1: route must be an array of tables"),
        ("[[route.x]]\n", "line 1: a sub-table of route"),
        (
            "[[route]]\npath = \"a\\b\"\n",
            "line 2: an escape sequence in path",
        ),
    ] {
        let got = guard::routes_toml(src.as_bytes());
        let e = got.error.unwrap_or_default();
        assert!(e.starts_with(want), "{e:?} does not start with {want:?}");
    }
}

/// The excerpt's lines must still be lines of the real script (when `ASSEMBLE_RELEASE_SH` names it).
#[test]
fn guard_excerpt_is_verbatim() {
    let Ok(path) = std::env::var("ASSEMBLE_RELEASE_SH") else {
        common::skip("guard_excerpt_is_verbatim: ASSEMBLE_RELEASE_SH not set");
        return;
    };
    let real = fs::read_to_string(path).unwrap_or_default();
    let real_lines: std::collections::HashSet<&str> = real.lines().collect();
    let mut checked = 0;
    for l in EXCERPT.lines().skip(2) {
        if l.starts_with("#@") {
            continue;
        }
        assert!(real_lines.contains(l), "not a line of the script: {l:?}");
        checked += 1;
    }
    assert!(checked > 150, "checked only {checked} lines");
}
