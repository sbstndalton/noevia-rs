//! Properties of the code-actions port: no input panics, everything stays bounded, and the answers
//! keep code-actions.cjs's fail-closed rules (the JS comparison itself is noevia-core's
//! tests/server/code-actions-differential.test.cjs, which also checks the switched host is never
//! more permissive than the JS).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use code_actions::{
    analyze_command, call, classify, decide, Action, Call, CommandValue, DecideInput, Refusal,
    MAX_INPUT_BYTES,
};
use prompt_framing::js::units;
use proptest::prelude::*;
use std::time::{Duration, Instant};

const WORDS: &[&str] = &[
    "ls",
    "cat",
    "echo",
    "rm",
    "git",
    "curl",
    "wget",
    "http",
    "npm",
    "pip",
    "npx",
    "sh",
    "bash",
    "eval",
    "python3",
    "node",
    "find",
    "xargs",
    "sudo",
    "env",
    "nice",
    "timeout",
    "ssh",
    "open",
    "gh",
    "aws",
    "docker",
    "cd",
    "make",
    "push",
    "install",
    "publish",
    "-c",
    "-e",
    "-exec",
    "-delete",
    "{}",
    ";",
    "+",
    "-q",
    "-o",
    "-O",
    "-",
    "-H",
    "Host:x",
    "FOO=bar",
    "$HOME",
    "~/x",
    "/abs",
    "rel",
    "alias.p=push",
    "core.pager=x",
    "https://example.com",
    "http://evil.test:1/x",
    "é",
    "日本",
    "\u{a0}",
    "\u{2028}",
    "|",
    "||",
    "&&",
    "&",
    ">",
    ">>",
    "2>&1",
    "<",
    "<<<",
    "$(",
    ")",
    "`",
    "(",
    "{",
    "}",
    "\"",
    "'",
    "\\",
    ">(",
    "<(",
    "\n",
];

fn command() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(WORDS), 0..14).prop_map(|w| w.join(" "))
}

fn execute(command: &str) -> Call {
    Call {
        kind: Some(units("execute")),
        raw_input: true,
        values: vec![(CommandValue::Text(units(command)), false)],
        outside: false,
        locations: vec![],
    }
}

fn names(actions: &[Action]) -> Vec<&'static str> {
    actions.iter().map(|a| a.name()).collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn random_bytes_never_panic(op in 0u8..5, body in prop::collection::vec(any::<u8>(), 0..256)) {
        let mut input = vec![op];
        input.extend(body);
        let (status, reply) = call(&input);
        prop_assert!(status <= 1);
        prop_assert!(!reply.is_empty());
    }

    #[test]
    fn classification_fails_closed(cmd in command()) {
        let Ok(c) = classify(&execute(&cmd)) else { return Ok(()); };
        // Something always comes out, and the worst class is among the classes found.
        prop_assert!(!c.actions.is_empty());
        prop_assert!(c.actions.contains(&c.action) || c.action == Action::Execute);
        prop_assert!(c.action != Action::None, "a command is never a no-op");
        // Only one plain command with one class is simple (an unreadable, empty command keeps
        // classify's defaults and is handled by `readable: false` and approval 'always').
        prop_assert_eq!(c.readable, !cmd.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}').is_empty());
        if c.simple && c.readable {
            prop_assert_eq!(c.actions.len(), 1);
            let seen = String::from_utf16_lossy(&c.command);
            // (Quoted or escaped operators are words.)
            let unquoted = !seen.contains(['"', '\'', '\\']);
            for op in ["|", ";", "&", "$(", "`", ">", "<", "\n"] {
                prop_assert!(!unquoted || !seen.contains(op), "{} is simple", seen);
            }
        }
        // Run-time commands never stand.
        let first = String::from_utf16_lossy(&c.command);
        let first = first.split([' ', '\t']).next().unwrap_or("");
        if ["sh", "bash", "eval", "xargs"].contains(&first) || first.starts_with('$') {
            prop_assert!(!c.standable, "{} stands", cmd);
        }
        // A command can be auto-allowed only on the domain list, never 'never'.
        prop_assert_ne!(c.approval, "never");
        // Every redirect target is a path the containment check sees.
        if c.actions.contains(&Action::Edit) && cmd.contains('>') && !cmd.contains("/dev/") {
            prop_assert!(!c.paths.is_empty() || !c.standable || !c.simple);
        }
        let _ = names(&c.actions);
    }

    #[test]
    fn deterministic(cmd in command()) {
        let a = classify(&execute(&cmd));
        let b = classify(&execute(&cmd));
        prop_assert_eq!(a, b);
    }

    #[test]
    fn decide_never_allows_without_cause(cmd in command(), approval in prop::sample::select(&["never", "always", "capability", "x"][..]),
        domains in prop::collection::vec(prop::sample::select(&["example.com", "evil.test", "", "com"][..]), 0..3),
        caps in prop::collection::vec(prop::sample::select(&["network", "execute_command", "delete", "edit_file"][..]), 0..3),
        inside in prop::option::of(any::<bool>())) {
        let Ok(c) = classify(&execute(&cmd)) else { return Ok(()); };
        let d = DecideInput {
            action: units(c.action.name()),
            approval: units(approval),
            command: c.command.clone(),
            readable: c.readable,
            paths: c.paths.clone(),
            actions: c.actions.iter().map(|a| units(a.name())).collect(),
            simple: c.simple,
            capabilities: caps.iter().map(|s| units(s)).collect(),
            domains: domains.iter().map(|s| units(s)).collect(),
            in_workspace: inside,
        };
        let Ok(r) = decide(&d) else { return Ok(()); };
        if r.decision == "allow" {
            prop_assert!(approval == "never" || (approval == "capability" && c.simple && !domains.is_empty()), "{:?}", r);
        }
        // A write that names a path outside the workspace is never allowed or offered.
        if inside == Some(false) && !c.paths.is_empty()
            && (c.actions.contains(&Action::Edit) || c.actions.contains(&Action::Delete)) {
            prop_assert_eq!(r.decision, "deny");
        }
    }
}

#[test]
fn find_chains_are_bounded() {
    for n in [64usize, 65, 1000, 50_000] {
        let cmd = format!("find {}", "-exec find ".repeat(n));
        let t = Instant::now();
        let r = analyze_command(&units(&cmd));
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "{n}: {:?}",
            t.elapsed()
        );
        if n > 64 {
            assert_eq!(r, Err(Refusal::TooLarge), "{n}");
        }
    }
}

#[test]
fn large_and_deep_inputs_stay_bounded() {
    let t = Instant::now();
    // 2 MiB of plain words, 200,000 pipes, 100,000 nested substitutions, a 1 MiB heredoc-ish string.
    let words = "ab ".repeat(700_000);
    let pipes = "ls | ".repeat(200_000);
    let nested = format!("{}x{}", "$(".repeat(100_000), ")".repeat(100_000));
    let quoted = format!("echo \"{}\"", "\\\"".repeat(500_000));
    for cmd in [words, pipes, nested, quoted] {
        let _ = classify(&execute(&cmd));
    }
    assert!(t.elapsed() < Duration::from_secs(20), "{:?}", t.elapsed());
    // Over the request cap: refused.
    let mut big = vec![1u8];
    big.extend(vec![b' '; MAX_INPUT_BYTES]);
    assert_eq!(call(&big).0, 1);
    // 100,000-deep JSON is bounded by the parser's depth cap.
    let mut deep = vec![1u8];
    deep.extend("[".repeat(100_000).bytes());
    assert_eq!(call(&deep).0, 1);
}
