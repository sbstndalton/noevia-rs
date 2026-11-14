#![allow(clippy::indexing_slicing, clippy::expect_used, clippy::unwrap_used)]

use super::*;

fn tree(files: &[(&str, &str)]) -> Tree {
    files
        .iter()
        .map(|(p, t)| (p.to_string(), t.to_string()))
        .collect()
}

#[test]
fn literals_start_after_a_quote_and_stop_at_a_template_or_quote() {
    let text = "const PREFIX = '/internal/calibration-cancel/';\n\
                if (path === \"/internal/models/facts\") x();\n\
                const u = `/internal/chat/${op}`;\n\
                // '/internal/commented/out' is not served\n\
                 * '/internal/doc-comment'\n\
                const joined = base + '/x' + '/internal/not-first';\n\
                const bare = /internal/regex/;\n\
                const two = ['/internal/a/b', '/internal/c.d'];";
    assert_eq!(
        internal_literals(text),
        [
            "/internal/calibration-cancel/",
            "/internal/models/facts",
            "/internal/chat/",
            "/internal/not-first",
            "/internal/a/b",
            "/internal/c.d"
        ]
    );
}

#[test]
fn a_family_is_the_first_segment() {
    assert_eq!(
        family("/internal/calibration-cancel/facts").as_deref(),
        Some("/internal/calibration-cancel")
    );
    assert_eq!(
        family("/internal/budget-watch").as_deref(),
        Some("/internal/budget-watch")
    );
    assert_eq!(family("/internal/chat/").as_deref(), Some("/internal/chat"));
    assert_eq!(family("/internal/"), None);
    assert_eq!(family("/api/x"), None);
}

#[test]
fn a_family_is_named_only_as_a_whole_segment() {
    assert!(names_family(
        "pub const BRIDGE_PATH: &str = \"/internal/budget-watch\";",
        "/internal/budget-watch"
    ));
    assert!(names_family(
        "format!(\"{}/internal/approvals/revoke\", base)",
        "/internal/approvals"
    ));
    assert!(names_family("/internal/chat?x", "/internal/chat"));
    assert!(!names_family("/internal/chat-native/x", "/internal/chat"));
    assert!(!names_family("/internal/chats", "/internal/chat"));
    // A later whole match after a longer one still counts.
    assert!(names_family(
        "/internal/chats and /internal/chat/turn",
        "/internal/chat"
    ));
}

#[test]
fn families_core_serves_are_listed_with_their_rust_callers() {
    let core = tree(&[
        (
            "server/rust-calibration-cancel-bridge.cjs",
            "const PREFIX = '/internal/calibration-cancel/';\nif (path !== PREFIX + 'facts') return false;",
        ),
        (
            "server/rust-budget-watch.cjs",
            "const PATH = '/internal/budget-watch';",
        ),
        (
            "server/approval-revocation.cjs",
            "const PREFIX = '/internal/approvals/revoke';",
        ),
        // Tests and non-server files never serve.
        (
            "server/rust-x.test.cjs",
            "req.url = '/internal/test-only/facts';",
        ),
        ("tools/rec.cjs", "fetch('/internal/tool-only')"),
    ]);
    let rust = tree(&[
        (
            "bins/noevia-server/src/model_maint/budget_watch.rs",
            "pub const BRIDGE_PATH: &str = \"/internal/budget-watch\";",
        ),
        (
            "bins/noevia-server/src/approval_revocation.rs",
            ".post(format!(\"{}/internal/approvals/revoke\", self.base))",
        ),
        // Tests do not count as callers.
        (
            "bins/noevia-server/tests/calibration.rs",
            "post(\"/internal/calibration-cancel/facts\")",
        ),
        (
            "bins/noevia-server/src/calibration_tests.rs",
            "\"/internal/calibration-cancel/abort\"",
        ),
        // Only the file name, not the path: not a caller.
        (
            "bins/noevia-server/src/calibration_cancel_routes.rs",
            "//! (rust-calibration-cancel-bridge.cjs) answered before the worker moved.",
        ),
    ]);
    let list = bridges(&core, &rust);
    assert_eq!(
        list,
        [
            Bridge {
                family: "/internal/approvals".into(),
                served_by: vec!["server/approval-revocation.cjs".into()],
                callers: vec!["bins/noevia-server/src/approval_revocation.rs".into()],
            },
            Bridge {
                family: "/internal/budget-watch".into(),
                served_by: vec!["server/rust-budget-watch.cjs".into()],
                callers: vec!["bins/noevia-server/src/model_maint/budget_watch.rs".into()],
            },
            Bridge {
                family: "/internal/calibration-cancel".into(),
                served_by: vec!["server/rust-calibration-cancel-bridge.cjs".into()],
                callers: vec![],
            },
        ]
    );
    let noevia = tree(&[(
        "deploy/tools/assemble-release.sh",
        "a\n[ -f \"$tree/core/server/rust-calibration-cancel-bridge.cjs\" ] || die \"missing state bridge\"",
    )]);
    assert_eq!(
        report(&list, Some(&noevia)),
        "| Bridge family | Served by | Named in noevia |\n|---|---|---|\n\
         | `/internal/calibration-cancel` | `server/rust-calibration-cancel-bridge.cjs` | deploy/tools/assemble-release.sh:2 |\n"
    );
    assert_eq!(
        report(&list[..2], None),
        "| Bridge family | Served by | Named in noevia |\n|---|---|---|\n| (none) | | |\n"
    );
}
