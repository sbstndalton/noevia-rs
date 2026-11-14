//! The chat rows pairing (`release_assemble::chat_rows`) against its verbatim guard block
//! (`fixtures/assemble-release-chat-rows-excerpt.sh`, noevia council/p12-3-chat-rows-guard), run under
//! `set -euo pipefail` with the verbatim primitives and the script's `die`, over generated release trees: the
//! verdict and the refusal text must be equal, and every verdict of the block must be reached.
//! `RELEASE_ASSEMBLE_GUARD_ROUNDS` sets the number of trees (default 400). With `ASSEMBLE_RELEASE_SH` set, the
//! excerpt must be whole consecutive lines of that script.
#![cfg(unix)]
#![allow(clippy::indexing_slicing, clippy::expect_used, clippy::unwrap_used)]

mod common;
use release_assemble::chat_rows::{self, ChatRows};
use std::fs;
use std::path::Path;
use std::process::Command;

const PRIMITIVES: &str = include_str!("../fixtures/assemble-release-guards-excerpt.sh");
const BLOCK: &str = include_str!("../fixtures/assemble-release-chat-rows-excerpt.sh");

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
    fn chance(&mut self, pct: usize) -> bool {
        self.below(100) < pct
    }
}

fn write(path: &Path, data: &str) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).expect("a parent directory");
    }
    fs::write(path, data).expect("a tree file");
}

/// A FEATURES entry: mostly listed once, sometimes absent, twice, or only in a comment.
fn entry(r: &mut Rng, name: &str, list: &mut String) {
    match r.below(10) {
        0 => {}
        1 => list.push_str(&format!("    // \"{name}\",\n")),
        2 => list.push_str(&format!("    \"{name}\",\n    \"{name}\",\n")),
        3 => list.push_str(&format!(
            "    #[cfg(feature = \"rust-storage-reads\")]\n    \"{name}\",\n"
        )),
        _ => list.push_str(&format!("    \"{name}\",\n")),
    }
}

/// A `pub const NAME: &str = "VALUE";` line: mostly right, sometimes another value, missing or commented.
fn rust_const(r: &mut Rng, name: &str, value: &str, out: &mut String) {
    match r.below(16) {
        0 => {}
        1 => out.push_str(&format!("pub const {name}: &str = \"{value}-v0\";\n")),
        2 => out.push_str(&format!("// pub const {name}: &str = \"{value}\";\n")),
        3 => out.push_str(&format!(
            "pub const {name}: &str = \"{value}\";\npub const {name}: &str = \"{value}\";\n"
        )),
        _ => out.push_str(&format!("pub const {name}: &str = \"{value}\";\n")),
    }
}

fn js_const(r: &mut Rng, name: &str, value: &str, out: &mut String, exports: &mut Vec<String>) {
    match r.below(16) {
        0 => {}
        1 => out.push_str(&format!("const {name} = '{value}-v0';\n")),
        2 => out.push_str(&format!("// const {name} = '{value}';\n")),
        _ => out.push_str(&format!("const {name} = '{value}';\n")),
    }
    if !r.chance(8) {
        exports.push(name.to_string());
    }
}

fn line(r: &mut Rng, text: &str, out: &mut String) {
    match r.below(14) {
        0 => {}
        1 => out.push_str(&format!("// {text}\n")),
        _ => out.push_str(&format!("{text}\n")),
    }
}

fn gen(r: &mut Rng, root: &Path) {
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).expect("the tree");
    if !r.chance(4) {
        let mut lib = String::from("pub const FEATURES: &[&str] = &[\n    \"rust-auth\",\n");
        entry(r, chat_rows::PROVIDERS_FEATURE, &mut lib);
        entry(r, chat_rows::SIGNIN_FEATURE, &mut lib);
        lib.push_str("    \"rust-chat-turn\",\n];\n");
        write(&root.join("rs/bins/noevia-server/src/lib.rs"), &lib);
    }
    if !r.chance(4) {
        let mut p = String::from("//! The provider rows.\n");
        rust_const(r, "CAPS_TOKEN", chat_rows::PROVIDERS_CAP, &mut p);
        rust_const(r, "SWITCH", chat_rows::PROVIDERS_SWITCH, &mut p);
        rust_const(r, "SIGNIN_CAPS_TOKEN", chat_rows::SIGNIN_CAP, &mut p);
        rust_const(r, "SIGNIN_SWITCH", chat_rows::SIGNIN_SWITCH, &mut p);
        write(&root.join("rs/bins/noevia-server/src/chat_provider.rs"), &p);
    }
    if !r.chance(4) {
        let mut n = String::from("'use strict';\n");
        let mut exports = vec!["HANDOFF_HEADER".to_string()];
        line(r, "function plainProviderRow(t) {\n  return t;\n}", &mut n);
        js_const(r, "SIGNIN_CAP", chat_rows::SIGNIN_CAP, &mut n, &mut exports);
        js_const(
            r,
            "SIGNIN_SWITCH",
            chat_rows::CORE_SIGNIN_SWITCH,
            &mut n,
            &mut exports,
        );
        line(r, "function signinRow(t) {\n  return t;\n}", &mut n);
        n.push_str(&format!("module.exports = {{ {} }};\n", exports.join(", ")));
        write(&root.join("core/server/rust-chat-native.cjs"), &n);
    }
    if !r.chance(4) {
        let mut c = String::from("'use strict';\nconst facts = {\n");
        line(
            r,
            "          providers: handoffCaps.has('providers'),",
            &mut c,
        );
        line(
            r,
            "          signin: handoffCaps.has(native.SIGNIN_CAP), signinSwitch: process.env[native.SIGNIN_SWITCH] === '1',",
            &mut c,
        );
        c.push_str("};\n");
        write(&root.join("core/server/chat.cjs"), &c);
    }
}

fn shell_verdict(dir: &Path, tree: &Path) -> String {
    let prims = dir.join("primitives.sh");
    if !prims.exists() {
        fs::write(&prims, PRIMITIVES).expect("the primitives excerpt");
    }
    let script = format!(
        "set -euo pipefail\ndie() {{ echo \"assemble-release: $*\" >&2; exit 1; }}\n. \"$0\"\ntree=\"$1\"\n{BLOCK}echo PASS\n"
    );
    let out = Command::new("bash")
        .env("LC_ALL", "C")
        .arg("-c")
        .arg(script)
        .arg(&prims)
        .arg(tree)
        .output()
        .expect("bash runs");
    if out.status.success() {
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    } else {
        format!(
            "{} (status {:?})",
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .last()
                .unwrap_or_default(),
            out.status.code()
        )
    }
}

fn verdict(got: Result<(), String>) -> String {
    match got {
        Ok(()) => "PASS".to_string(),
        Err(e) => format!("assemble-release: {e} (status Some(1))"),
    }
}

#[test]
fn chat_rows_pairing_matches_the_shell() {
    if !have_bash() {
        common::skip("guard_chat_rows: bash is not installed");
        return;
    }
    let dir = tempfile::tempdir().expect("a temporary directory");
    let tree = dir.path().join("tree");
    let mut r = Rng(0xC4A7_5100_0000_0012);
    let mut seen = std::collections::BTreeMap::<String, u64>::new();
    let n = rounds();
    for _ in 0..n {
        gen(&mut r, &tree);
        let want = shell_verdict(dir.path(), &tree);
        let got = verdict(chat_rows::chat_rows_pairing(&tree));
        assert_eq!(
            got,
            want,
            "chat rows pairing over the generated tree {:?}",
            ChatRows::read(&tree)
        );
        *seen.entry(want).or_default() += 1;
    }
    eprintln!("COUNTS guard_chat_rows trees={n} verdicts={seen:?}");
    // PASS, sign-in without providers, providers declaration, Core providers, sign-in declaration, Core sign-in.
    assert_eq!(seen.len(), 6, "verdicts reached: {seen:?}");
}

/// The front and Core as the council branches write them (rs council/p12-3-signin-guard, core
/// council/p10-1-core-signin-rows): today's FEATURES (providers only) passes beside that Core and is refused
/// beside a Core without the hand-off.
#[test]
fn the_council_branches_pair() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let t = dir.path();
    write(
        &t.join("rs/bins/noevia-server/src/lib.rs"),
        "pub const FEATURES: &[&str] = &[\n    \"rust-providers-reseal\",\n    // Council p12-3: provider-row turns.\n    #[cfg(feature = \"rust-storage-reads\")]\n    \"chat-providers-v1\",\n    \"rust-chat-turn\",\n];\n",
    );
    write(
        &t.join("rs/bins/noevia-server/src/chat_provider.rs"),
        "pub const CAPS_TOKEN: &str = \"providers\";\n/// The switch.\npub const SWITCH: &str = \"NOEVIA_RUST_CHAT_PROVIDERS\";\npub const SIGNIN_CAPS_TOKEN: &str = \"signin-transport-v1\";\npub const SIGNIN_SWITCH: &str = \"NOEVIA_RUST_CHAT_SIGNIN\";\npub const PROVIDERS_FEATURE: &str = \"chat-providers-v1\";\n",
    );
    write(
        &t.join("core/server/rust-chat-native.cjs"),
        "'use strict';\nfunction plainProviderRow(t) {\n  return !!t;\n}\nconst SIGNIN_CAP = 'signin-transport-v1';\nconst SIGNIN_SWITCH = 'NOEVIA_RUST_SIGNIN_TRANSPORT';\nfunction signinRow(t) {\n  return !!t;\n}\nmodule.exports = { HANDOFF_HEADER, plainProviderRow, signinRow, frontRow, SIGNIN_CAP, SIGNIN_SWITCH, createToolBridge };\n",
    );
    write(
        &t.join("core/server/chat.cjs"),
        "        const r = native.refusal({\n          providers: handoffCaps.has('providers'),\n          signin: handoffCaps.has(native.SIGNIN_CAP), signinSwitch: process.env[native.SIGNIN_SWITCH] === '1',\n        });\n",
    );
    let facts = ChatRows::read(t);
    assert_eq!(
        facts,
        ChatRows {
            rs_providers: true,
            rs_signin: false,
            rs_providers_decl: true,
            rs_signin_decl: true,
            core_providers: true,
            core_signin: true,
        }
    );
    assert_eq!(chat_rows::chat_rows_pairing(t), Ok(()));
    // Core main: no provider-row hand-off yet.
    write(
        &t.join("core/server/rust-chat-native.cjs"),
        "'use strict';\nmodule.exports = { HANDOFF_HEADER };\n",
    );
    let e = chat_rows::chat_rows_pairing(t).unwrap_err();
    assert!(
        e.contains("Core never hands a provider row's turn off"),
        "{e}"
    );
    if have_bash() {
        assert_eq!(
            verdict(chat_rows::chat_rows_pairing(t)),
            shell_verdict(t, t)
        );
    }
}

#[test]
fn excerpt_is_verbatim() {
    let Ok(path) = std::env::var("ASSEMBLE_RELEASE_SH") else {
        common::skip("guard_chat_rows excerpt_is_verbatim: ASSEMBLE_RELEASE_SH not set");
        return;
    };
    let real = fs::read_to_string(path).unwrap_or_default();
    let real_lines: Vec<&str> = real.lines().collect();
    let block: Vec<&str> = BLOCK.lines().collect();
    assert!(!block.is_empty());
    let found = real_lines
        .windows(block.len())
        .any(|w| w == block.as_slice());
    assert!(found, "the chat rows block is not whole consecutive lines of the script (noevia council/p12-3-chat-rows-guard)");
}
