//! The route pairings (`src/route_pairings.rs`) against their verbatim shell
//! (`fixtures/assemble-release-route-pairings-excerpt.sh`, with the primitives from the guards excerpt and
//! `core_requires` from the model pairings excerpt), run under `set -euo pipefail`, `LC_ALL=C`, with the
//! script's `die`, over generated release trees: the verdict and the refusal text must be equal, PASS must be
//! reached and at least the named number of verdicts.
//!
//! The shell side needs bash (and node for the project storage pairing, whose `core_requires` runs node);
//! without them each comparison is skipped (a failure under `RELEASE_ASSEMBLE_ORACLES_REQUIRED=1`).
//! `RELEASE_ASSEMBLE_GUARD_ROUNDS` sets the number of trees per group (default 400).
#![cfg(unix)]
#![allow(clippy::indexing_slicing, clippy::expect_used)]

mod common;
use release_assemble::route_pairings::{self, Earlier, Want};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

const EXCERPT: &str = include_str!("../fixtures/assemble-release-route-pairings-excerpt.sh");
const GUARDS: &str = include_str!("../fixtures/assemble-release-guards-excerpt.sh");
const MODEL: &str = include_str!("../fixtures/assemble-release-model-pairings-excerpt.sh");

const SECTIONS: &[&str] = &[
    "project_storage",
    "close_out",
    "decision_settings",
    "storage_integrations",
    "vault_mirror",
    "storage_connection",
    "providers",
    "chatgpt_signin",
    "mcp_credentials",
    "mcp_views",
    "provider_models",
    "google_drive",
    "offsite_backup",
    "model_sizing",
    "model_state_reads",
    "model_evidence",
];

fn section_of(text: &str, name: &str) -> String {
    let start = format!("#@{name}\n");
    let Some(at) = text.find(&start) else {
        return String::new();
    };
    let rest = &text[at + start.len()..];
    let end = rest.find("\n#@").map(|i| i + 1).unwrap_or(rest.len());
    rest[..end].to_string()
}

fn tool(name: &str) -> bool {
    Command::new(name)
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
    fn pick<'a>(&mut self, from: &[&'a str]) -> &'a str {
        from[self.below(from.len())]
    }
}

fn write(path: &Path, data: &str) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).expect("a parent directory");
    }
    fs::write(path, data).expect("a tree file");
}

// ---------------------------------------------------------------------------------------------------------
// Verbatim

/// Each section of the excerpt is a contiguous run of lines of the real script (when `ASSEMBLE_RELEASE_SH`
/// names it), and every section is there.
#[test]
fn route_pairings_excerpt_is_verbatim() {
    for name in SECTIONS {
        assert!(!section_of(EXCERPT, name).is_empty(), "section {name}");
    }
    assert_eq!(EXCERPT.matches("\n#@").count(), SECTIONS.len());
    let Ok(path) = std::env::var("ASSEMBLE_RELEASE_SH") else {
        common::skip("route_pairings_excerpt_is_verbatim: ASSEMBLE_RELEASE_SH not set");
        return;
    };
    let script = fs::read_to_string(path).expect("the script");
    for name in SECTIONS {
        let s = section_of(EXCERPT, name);
        assert!(script.contains(&s), "section {name} is not verbatim");
    }
}

// ---------------------------------------------------------------------------------------------------------
// Generated trees

/// One pair of a group: Core's refusal module, the front's capability, and the rows routes.toml should
/// carry for it (a path and its methods, `[]` = every method: no `methods` key).
struct Pair {
    module: &'static str,
    feature: &'static str,
    rows: &'static [(&'static str, &'static [&'static str])],
}

/// A file a group's extra checks read: its variants (`None` = absent); the first one is the good one.
type Extra = (&'static str, &'static [Option<&'static str>]);

struct Group {
    section: &'static str,
    pairs: &'static [Pair],
    /// Lines appended to index.cjs, one variant each (the first is the good one).
    index: &'static [&'static [&'static str]],
    /// Lines appended to the front's lib.rs.
    lib: &'static [&'static [&'static str]],
    extras: &'static [Extra],
    /// Paths that are not the pair's but are near them (an `Under` selection takes them).
    near: &'static [&'static str],
    min_verdicts: usize,
}

const METHOD_NOISE: &[&str] = &[
    "",
    "GET",
    "POST",
    "PUT",
    "DELETE",
    "GET,PUT",
    "GET,GET",
    "GET,POST",
    "PUT,DELETE",
    "DELETE,GET",
    "POST,PUT,DELETE",
    "POST,POST,PUT",
    "DELETE,POST,PUT",
    "GET,PUT,POST",
];

fn methods_line(list: &str) -> String {
    if list.is_empty() {
        return String::new();
    }
    let quoted: Vec<String> = list.split(',').map(|m| format!("\"{m}\"")).collect();
    format!("methods = [{}]\n", quoted.join(", "))
}

fn pick_variant<'a>(r: &mut Rng, variants: &[&'a str]) -> &'a str {
    if variants.len() == 1 || r.chance(80) {
        variants[0]
    } else {
        variants[1 + r.below(variants.len() - 1)]
    }
}

fn pick_extra(r: &mut Rng, variants: &[Option<&'static str>]) -> Option<&'static str> {
    if variants.len() == 1 || r.chance(80) {
        variants[0]
    } else {
        variants[1 + r.below(variants.len() - 1)]
    }
}

fn gen_tree(r: &mut Rng, root: &Path, g: &Group) {
    fs::remove_dir_all(root).ok();
    fs::create_dir_all(root).expect("the tree root");
    // All pairs of a group move together most of the time, so that PASS is reached.
    let together = r.chance(60);
    let all_moved = r.chance(70);
    let moved: Vec<bool> = g
        .pairs
        .iter()
        .map(|_| {
            if together {
                all_moved
            } else {
                r.chance(60)
            }
        })
        .collect();

    // The front: FEATURES and extra lines.
    let mut lib = String::new();
    match r.below(16) {
        0 => {}
        1 => lib.push_str("// pub const FEATURES: &[&str] = &[\"rust-auth\"];\n"),
        _ => {
            lib.push_str("pub const FEATURES: &[&str] = &[\n    \"rust-auth\",\n");
            for (p, m) in g.pairs.iter().zip(&moved) {
                if *m {
                    lib.push_str(&format!("    \"{}\",\n", p.feature));
                    if r.chance(3) {
                        lib.push_str(&format!("    \"{}\",\n", p.feature));
                    }
                }
            }
            lib.push_str("];\n");
        }
    }
    for line in g.lib {
        lib.push_str(pick_variant(r, line));
        lib.push('\n');
    }
    if !lib.is_empty() {
        write(&root.join("rs/bins/noevia-server/src/lib.rs"), &lib);
    }

    // Core: index.cjs with each refusal module required (or not, or only in a comment), plus extra lines.
    let mut index = String::from("'use strict';\n");
    for (p, m) in g.pairs.iter().zip(&moved) {
        let core_moved = if r.chance(6) { !*m } else { *m };
        if core_moved {
            match r.below(20) {
                0 => index.push_str(&format!("// const x = require('./{}');\n", p.module)),
                1 => index.push_str(&format!("const x = require( './{}' );\n", p.module)),
                _ => index.push_str(&format!("const x = require('./{}');\n", p.module)),
            }
        }
    }
    for line in g.index {
        index.push_str(pick_variant(r, line));
        index.push('\n');
    }
    write(&root.join("core/server/index.cjs"), &index);

    // The route contract.
    if !r.chance(6) {
        let mut toml = String::from("# routes\n[[route]]\npath = \"/api/other\"\nowner = \"node\"\n");
        for (p, m) in g.pairs.iter().zip(&moved) {
            let owner = if *m { "rust" } else { "node" };
            for (path, good) in p.rows {
                if r.chance(4) {
                    continue;
                }
                let copies = if r.chance(4) { 2 } else { 1 };
                for _ in 0..copies {
                    let o = if r.chance(5) {
                        if owner == "rust" {
                            "node"
                        } else {
                            "rust"
                        }
                    } else {
                        owner
                    };
                    toml.push_str(&format!("[[route]]\npath = \"{path}\"\nowner = \"{o}\"\n"));
                    if r.chance(4) {
                        toml.push_str("switch = \"NOEVIA_RUST_X\"\n");
                    }
                    let list = if r.chance(85) {
                        good.join(",")
                    } else {
                        r.pick(METHOD_NOISE).to_string()
                    };
                    toml.push_str(&methods_line(&list));
                }
            }
        }
        for path in g.near {
            if r.chance(10) {
                toml.push_str(&format!("[[route]]\npath = \"{path}\"\nowner = \"rust\"\n"));
            }
        }
        if r.chance(3) {
            toml.push_str("[[route]]\npath = \"/api/broken\"\nmethods = [\"GET\"\n");
        }
        write(&root.join("rs/contracts/http/routes.toml"), &toml);
    }

    for (rel, variants) in g.extras {
        if let Some(content) = pick_extra(r, variants) {
            write(&root.join(rel), content);
        }
    }
}

fn shell_verdict(dir: &Path, block: &str, pre: &str, tree: &Path) -> String {
    let prims = dir.join("primitives.sh");
    if !prims.exists() {
        let text = format!("{GUARDS}{}", section_of(MODEL, "core_requires"));
        fs::write(&prims, text).expect("the primitives");
    }
    let script = format!(
        "set -euo pipefail\ndie() {{ echo \"assemble-release: $*\" >&2; exit 1; }}\n. \"$0\"\ntree=\"$1\"\n{pre}{block}echo PASS\n"
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

/// Run a group: each round a new tree and one random bit for the earlier verdict the section takes (`pre`
/// spells it for the shell, `rust` gets it as a bool).
fn compare(
    g: &Group,
    seed: u64,
    needs_node: bool,
    pre: &dyn Fn(bool) -> String,
    rust: &dyn Fn(&Path, bool) -> Result<(), String>,
) {
    if !tool("bash") || (needs_node && !tool("node")) {
        common::skip(&format!(
            "route_pairings {}: bash{} needed",
            g.section,
            if needs_node { " and node" } else { "" }
        ));
        return;
    }
    let dir = tempfile::tempdir().expect("a temporary directory");
    let block = section_of(EXCERPT, g.section);
    assert!(!block.is_empty(), "section {}", g.section);
    let tree = dir.path().join("tree");
    let mut r = Rng(seed);
    let mut seen = BTreeMap::<String, u64>::new();
    let n = rounds();
    for round in 0..n {
        gen_tree(&mut r, &tree, g);
        let flag = r.chance(50);
        let want = shell_verdict(dir.path(), &block, &pre(flag), &tree);
        let got = verdict(rust(&tree, flag));
        assert_eq!(got, want, "{} round {round} (flag {flag})", g.section);
        *seen.entry(want).or_default() += 1;
    }
    eprintln!("COUNTS route_pairings {} trees={n} verdicts={seen:?}", g.section);
    assert!(seen.contains_key("PASS"), "{}: PASS reached ({seen:#?})", g.section);
    assert!(
        seen.len() >= g.min_verdicts,
        "{}: {} verdicts reached ({seen:#?})",
        g.section,
        seen.len()
    );
}

fn no_pre(_: bool) -> String {
    String::new()
}

fn simple(g: &Group, seed: u64, name: &'static str) {
    compare(g, seed, false, &no_pre, &|t, _| {
        route_pairings::by_name(t, name, Earlier::default()).expect("a pairing name")
    });
}

// ---------------------------------------------------------------------------------------------------------
// The groups

const PROJECT_STORAGE: Group = Group {
    section: "project_storage",
    pairs: &[Pair {
        module: "rust-project-storage.cjs",
        feature: "rust-project-storage",
        rows: &[],
    }],
    index: &[&[
        "",
        "const outbox = require('./rust-index-outbox.cjs');",
        "// require('./rust-index-outbox.cjs')",
        "const s = 'require(\"./rust-project-storage.cjs\")';",
        "const broken = 'open",
        "const t = `${require('./rust-project-storage.cjs')}`;",
    ]],
    lib: &[],
    extras: &[(
        "core/server/rust-index-outbox.cjs",
        &[
            None,
            Some("'use strict';\nconst storage = require('./rust-project-storage.cjs');\n"),
            Some("'use strict';\n/* require('./rust-project-storage.cjs') */\n"),
            Some("'use strict';\nconst bad = `open\n"),
        ],
    )],
    near: &[],
    min_verdicts: 4,
};

#[test]
fn project_storage_pairing_matches_the_shell() {
    compare(
        &PROJECT_STORAGE,
        0x9e0f_0000_0000_0001,
        true,
        &|lifecycle| {
            format!(
                "project_lifecycle_rs_owner={}\n",
                if lifecycle { "rust" } else { "" }
            )
        },
        &|t, lifecycle| route_pairings::project_storage_pairing(t, lifecycle).map(|_| ()),
    );
}

const CLOSE_OUT: Group = Group {
    section: "close_out",
    pairs: &[
        Pair {
            module: "rust-health.cjs",
            feature: "rust-health",
            rows: &[("/api/health", &["GET"])],
        },
        Pair {
            module: "rust-auto-roles.cjs",
            feature: "rust-auto-roles",
            rows: &[("/api/auto-roles", &["GET"])],
        },
        Pair {
            module: "rust-workspace.cjs",
            feature: "rust-workspace",
            rows: &[("/api/workspace", &["GET"])],
        },
        Pair {
            module: "rust-chat-search.cjs",
            feature: "rust-chat-search",
            rows: &[("/api/chats/search", &["GET"])],
        },
        Pair {
            module: "rust-chat-export.cjs",
            feature: "rust-chat-export",
            rows: &[("/api/chats/{chatId}/export", &["GET"])],
        },
    ],
    index: &[],
    lib: &[],
    extras: &[
        (
            "core/server/rust-chat-search.cjs",
            &[
                Some("'use strict';\nconst OWNED_ROUTES = Object.freeze(new Set(['GET /api/chats/search']));\n"),
                None,
                Some("// const OWNED_ROUTES = Object.freeze(new Set(['GET /api/chats/search']));\n"),
                Some("const OWNED_ROUTES = Object.freeze(new Set(['GET /api/chats/search', 'POST /api/chats/search']));\n"),
                Some("  const OWNED_ROUTES = Object.freeze(new Set(['GET /api/chats/search']));\n"),
            ],
        ),
        (
            "core/server/rust-chat-export.cjs",
            &[
                Some("const OWNED_ROUTES = Object.freeze(new Set(['GET /api/chats/{chatId}/export']));\n"),
                None,
                Some("const OWNED_ROUTES = Object.freeze(new Set(['GET /api/chats/{id}/export']));\n"),
            ],
        ),
        (
            "core/server/routes/chat-lists.cjs",
            &[
                Some("'use strict';\nrouter.get('/api/chats', list);\n"),
                Some("if (p === '/api/chats/search') return search(req, res);\n"),
                Some("// if (p === '/api/chats/search') return search(req, res);\n"),
                Some("res.end(chatMarkdown(chat));\n"),
                Some("/* res.end(chatMarkdown(chat)); */\n"),
                Some("res.end(chatMarkdown (chat));\n"),
                None,
            ],
        ),
    ],
    near: &[],
    min_verdicts: 6,
};

#[test]
fn close_out_pairings_match_the_shell() {
    // The auto-roles row with the write pair: GET and PUT.
    compare(
        &CLOSE_OUT,
        0xc105_e000_0000_0002,
        false,
        &|put| {
            format!(
                "auto_roles_write_owner={}\n",
                if put { "rust" } else { "" }
            )
        },
        &|t, put| route_pairings::close_out_pairings(t, put),
    );
}

#[test]
fn decision_settings_pairing_matches_the_shell() {
    const G: Group = Group {
        section: "decision_settings",
        pairs: &[Pair {
            module: "rust-decision-settings.cjs",
            feature: "rust-decision-settings",
            rows: &[
                ("/api/admin/decision-settings", &[]),
                ("/api/admin/decision-settings/test", &[]),
            ],
        }],
        index: &[],
        lib: &[],
        extras: &[],
        near: &["/api/admin/decision-settings/other"],
        min_verdicts: 4,
    };
    simple(&G, 0xdec1_0000_0000_0003, "decision-settings");
}

#[test]
fn storage_integrations_pairing_matches_the_shell() {
    const G: Group = Group {
        section: "storage_integrations",
        pairs: &[Pair {
            module: "rust-storage-integrations.cjs",
            feature: "rust-storage-integrations",
            rows: &[
                ("/api/integrations/storage/folder", &["POST"]),
                ("/api/integrations/storage/test", &["POST"]),
                ("/api/integrations/storage/nextcloud/start", &["POST"]),
                ("/api/integrations/storage/nextcloud/poll", &["POST"]),
                ("/api/integrations/storage/nextcloud/cancel", &["POST"]),
            ],
        }],
        index: &[],
        lib: &[],
        extras: &[],
        near: &[],
        min_verdicts: 4,
    };
    simple(&G, 0x5701_0000_0000_0004, "storage-integrations");
}

#[test]
fn vault_mirror_pairing_matches_the_shell() {
    const G: Group = Group {
        section: "vault_mirror",
        pairs: &[Pair {
            module: "rust-vault-mirror-preferences.cjs",
            feature: "rust-vault-mirror-preferences",
            rows: &[("/api/chat-vault-mirror/preferences", &[])],
        }],
        index: &[],
        lib: &[],
        extras: &[],
        near: &[],
        min_verdicts: 4,
    };
    simple(&G, 0x7a01_0000_0000_0005, "vault-mirror");
}

#[test]
fn storage_connection_pairing_matches_the_shell() {
    const G: Group = Group {
        section: "storage_connection",
        pairs: &[Pair {
            module: "rust-storage-connection.cjs",
            feature: "rust-storage-connection",
            rows: &[("/api/integrations/storage", &["GET", "PUT"])],
        }],
        index: &[],
        lib: &[],
        extras: &[],
        near: &[],
        min_verdicts: 4,
    };
    simple(&G, 0x5c0e_0000_0000_0006, "storage-connection");
}

#[test]
fn provider_pairings_match_the_shell() {
    const G: Group = Group {
        section: "providers",
        pairs: &[
            Pair {
                module: "rust-providers.cjs",
                feature: "rust-providers",
                rows: &[
                    ("/api/providers", &["GET", "POST"]),
                    ("/api/providers/{id}", &["PUT", "DELETE"]),
                    ("/api/providers/chatgpt", &["DELETE", "GET"]),
                ],
            },
            Pair {
                module: "rust-providers-reseal.cjs",
                feature: "rust-providers-reseal",
                rows: &[],
            },
            // The probe's row: POST alone without the registry, POST, PUT and DELETE with it; the
            // generator writes the registry's set and the noise covers POST alone.
            Pair {
                module: "rust-provider-test.cjs",
                feature: "rust-provider-test",
                rows: &[("/api/providers/test", &["DELETE", "POST", "PUT"])],
            },
        ],
        index: &[],
        lib: &[],
        extras: &[
            (
                "rs/bins/noevia-server/src/project_store.rs",
                &[
                    Some("    .route(\"/v1/providers/reseal\", post(reseal))\n"),
                    Some("    // /v1/providers/reseal\n"),
                    None,
                ],
            ),
            (
                "core/server/project-store-client.cjs",
                &[
                    Some("async function providersReseal(body) {}\n"),
                    Some("async function providerReseal(body) {}\n"),
                    None,
                ],
            ),
        ],
        near: &[],
        min_verdicts: 6,
    };
    simple(&G, 0x9a0f_0000_0000_0007, "providers");
}

#[test]
fn chatgpt_signin_pairing_matches_the_shell() {
    const G: Group = Group {
        section: "chatgpt_signin",
        pairs: &[Pair {
            module: "rust-chatgpt-oauth.cjs",
            feature: "rust-chatgpt-oauth",
            rows: &[
                ("/api/providers/chatgpt/device", &["POST"]),
                ("/api/providers/chatgpt/device/poll", &["POST"]),
                ("/api/providers/chatgpt/device/cancel", &["POST"]),
                ("/api/providers/chatgpt/models", &["GET"]),
            ],
        }],
        index: &[],
        lib: &[],
        extras: &[],
        near: &["/api/providers/chatgpt/device/other"],
        min_verdicts: 4,
    };
    simple(&G, 0xc6a7_0000_0000_0008, "chatgpt-signin");
}

#[test]
fn mcp_credentials_pairing_matches_the_shell() {
    const G: Group = Group {
        section: "mcp_credentials",
        pairs: &[Pair {
            module: "rust-mcp-credentials.cjs",
            feature: "rust-mcp-credentials",
            rows: &[
                ("/api/mcp-keys/servers", &["GET"]),
                ("/api/mcp-oauth/servers", &["GET"]),
                ("/api/mcp-oauth/callback", &["GET"]),
                ("/api/mcp-keys/{*}", &["DELETE", "PUT"]),
                ("/api/mcp-oauth/{id}", &["DELETE"]),
                ("/api/mcp-oauth/{id}/connect", &["POST"]),
            ],
        }],
        index: &[],
        lib: &[],
        extras: &[
            (
                "rs/bins/noevia-server/src/project_store.rs",
                &[
                    Some("    .route(\"/v1/mcp/session\", post(session))\n"),
                    Some("    // .route(\"/v1/mcp/session\", post(session))\n"),
                    None,
                ],
            ),
            (
                "core/server/project-store-client.cjs",
                &[
                    Some("  mcpSession(a);\n  mcpKey (b);\n  mcpReseal(c);\n"),
                    Some("  mcpSession(a);\n  xmcpKey(b);\n  mcpReseal(c);\n"),
                    Some("  mcpSession(a);\n  mcpKey(b);\n  // mcpReseal(c);\n"),
                    Some("mcpSession(a); mcpKey(b);\n"),
                    None,
                ],
            ),
        ],
        near: &[],
        min_verdicts: 6,
    };
    simple(&G, 0x3c9c_0000_0000_0009, "mcp-credentials");
}

#[test]
fn mcp_views_pairings_match_the_shell() {
    const G: Group = Group {
        section: "mcp_views",
        pairs: &[
            Pair {
                module: "rust-mcp-directory.cjs",
                feature: "rust-mcp-directory",
                rows: &[
                    ("/api/admin/mcp-directory", &[]),
                    ("/api/admin/mcp-directory/custom", &[]),
                    ("/api/admin/mcp-directory/custom/preview", &[]),
                    ("/api/admin/mcp-directory/{*}", &[]),
                    ("/api/admin/mcp-directory/{*}/keys", &[]),
                    ("/api/admin/mcp-directory/{id}/oauth-client", &[]),
                ],
            },
            Pair {
                module: "rust-toolboxes-permitted.cjs",
                feature: "rust-toolboxes-permitted",
                rows: &[("/api/toolboxes/permitted", &[])],
            },
            Pair {
                module: "rust-connectors.cjs",
                feature: "rust-connectors",
                rows: &[
                    ("/api/connectors", &["GET"]),
                    ("/api/connectors/nextcloud/policy", &["PUT"]),
                ],
            },
        ],
        index: &[
            &[
                "createChatFactsPublisher({ dir });",
                "// createChatFactsPublisher({ dir });",
                "",
            ],
            &[
                "createMcpDiscoveryPublisher ({ dir });",
                "createMcpDiscoveryPublisherX({ dir });",
                "",
            ],
        ],
        lib: &[&["pub mod mcp_discovery;", "// pub mod mcp_discovery;", "pub mod mcp_discoveryx;"]],
        extras: &[
            (
                "core/server/toolbox-chat-facts.cjs",
                &[Some("'use strict';\n"), None],
            ),
            (
                "core/server/mcp-discovery-snapshot.cjs",
                &[Some("'use strict';\n"), None],
            ),
            (
                "core/server/directory-mcp.cjs",
                &[
                    Some("db.prepare('SELECT * FROM directory_mcp_servers');\n"),
                    Some("db.prepare('INSERT OR REPLACE INTO directory_mcp_servers VALUES (?)');\n"),
                    Some("db.prepare('UPDATE \"directory_mcp_servers\" SET a = 1');\n"),
                    Some("db.prepare('DELETE FROM directory_mcp_servers');\n"),
                    Some("// db.prepare('DELETE FROM directory_mcp_servers');\n"),
                    None,
                ],
            ),
            (
                "core/server/rust-mcp-credentials.cjs",
                &[Some("const FRONT_TABLES = ['directory_mcp_servers'];\n"), Some("'use strict';\n")],
            ),
            (
                "rs/bins/noevia-server/src/mcp_broker.rs",
                &[Some("const T: &str = \"directory_mcp_servers\";\n"), None],
            ),
        ],
        near: &["/api/admin/mcp-directory/extra", "/api/admin/mcp-directoryx"],
        min_verdicts: 8,
    };
    compare(
        &G,
        0x3c9d_0000_0000_000a,
        false,
        &|md| format!("md_rs_owner={}\n", if md { "rust" } else { "" }),
        &|t, md| route_pairings::mcp_views_pairings(t, md),
    );
}

#[test]
fn provider_models_pairing_matches_the_shell() {
    const G: Group = Group {
        section: "provider_models",
        pairs: &[Pair {
            module: "rust-provider-models.cjs",
            feature: "rust-provider-models",
            rows: &[("/api/providers/{providerId}/models{*?}", &[])],
        }],
        index: &[],
        lib: &[],
        extras: &[],
        near: &[],
        min_verdicts: 4,
    };
    simple(&G, 0x9e0d_0000_0000_000b, "provider-models");
}

#[test]
fn google_drive_pairing_matches_the_shell() {
    const G: Group = Group {
        section: "google_drive",
        pairs: &[Pair {
            module: "rust-google-drive.cjs",
            feature: "rust-google-drive",
            rows: &[
                ("/api/connectors/gdrive/connect", &["POST"]),
                ("/api/connectors/gdrive/disconnect", &["POST"]),
                ("/api/connectors/gdrive/policy", &["PUT"]),
                ("/api/connectors/gdrive/backup-copy", &["PUT"]),
            ],
        }],
        index: &[],
        lib: &[],
        extras: &[],
        near: &[],
        min_verdicts: 4,
    };
    simple(&G, 0x6d71_0000_0000_000c, "google-drive");
}

#[test]
fn offsite_backup_pairing_matches_the_shell() {
    const G: Group = Group {
        section: "offsite_backup",
        pairs: &[Pair {
            module: "rust-offsite-backup.cjs",
            feature: "rust-offsite-backup",
            rows: &[
                ("/api/admin/offsite-backup", &[]),
                ("/api/admin/offsite-backup/copy", &[]),
                ("/api/admin/offsite-backup/google/connect", &[]),
                ("/api/admin/offsite-backup/google/disconnect", &[]),
                ("/api/admin/offsite-backup/new-store", &[]),
                ("/api/admin/offsite-backup/recovery-key", &[]),
                ("/api/admin/offsite-backup/run", &[]),
                ("/api/admin/offsite-backup/verify", &[]),
            ],
        }],
        index: &[],
        lib: &[],
        extras: &[(
            "core/server/rust-offsite-backup.cjs",
            &[
                Some("const OWNED = ['/api/admin/offsite-backup', '/api/admin/offsite-backup/new-store'];\n"),
                Some("const OWNED = ['/api/admin/offsite-backup'];\n"),
                None,
            ],
        )],
        near: &["/api/admin/offsite-backup/extra", "/api/admin/offsite-backups"],
        min_verdicts: 5,
    };
    simple(&G, 0x0ff5_0000_0000_000d, "offsite-backup");
}

#[test]
fn model_sizing_pairing_matches_the_shell() {
    const G: Group = Group {
        section: "model_sizing",
        pairs: &[Pair {
            module: "rust-model-sizing.cjs",
            feature: "rust-model-sizing",
            rows: &[
                ("/api/models/estimate", &["GET"]),
                ("/api/models/preset/suggest", &["GET"]),
            ],
        }],
        index: &[],
        lib: &[],
        extras: &[],
        near: &[],
        min_verdicts: 4,
    };
    simple(&G, 0x5121_0000_0000_000e, "model-sizing");
}

const SECRET_LINES: &[&str] = &[
    "const FRONT_SECRET = String(process.env.NOEVIA_PROJECT_STORE_TOKEN || '');",
    "",
    "// const FRONT_SECRET = String(process.env.NOEVIA_PROJECT_STORE_TOKEN || '');",
    "const FRONT_SECRET = String(process.env.NOEVIA_PROJECT_STORE_TOKEN);",
];

#[test]
fn model_state_reads_pairing_matches_the_shell() {
    const G: Group = Group {
        section: "model_state_reads",
        pairs: &[Pair {
            module: "rust-model-state-reads.cjs",
            feature: "rust-model-state-reads",
            rows: &[
                ("/api/models/inference-budget", &["GET"]),
                ("/api/models/installed", &["GET"]),
            ],
        }],
        index: &[
            SECRET_LINES,
            &["const facts = createModelFactsBridge({", "", "const facts = createModelFactsBridge ({"],
            &["  store,", "  store,\n  a,\n  b,\n  c,"],
            &["  secret: FRONT_SECRET,", "  secret: OTHER,", ""],
            &["});"],
            &[
                "if (await modelFactsBridge(req, res)) return;",
                "// if (await modelFactsBridge(req, res)) return;",
                "",
            ],
        ],
        lib: &[],
        extras: &[
            (
                "core/server/rust-model-state-reads.cjs",
                &[
                    Some("const OWNED_PATHS = Object.freeze(['/api/models/inference-budget', '/api/models/installed']);\n"),
                    Some("const OWNED_PATHS = Object.freeze(['/api/models/installed']);\n"),
                    None,
                ],
            ),
            (
                "core/server/routes/models.cjs",
                &[
                    Some("router.get('/api/models', list);\n"),
                    Some("res.json(inferenceEstimates());\n"),
                    Some("// res.json({ installed: await modelsInstalled()) });\n"),
                    None,
                ],
            ),
            (
                "core/server/rust-model-facts.cjs",
                &[Some("'use strict';\n"), None],
            ),
        ],
        near: &[],
        min_verdicts: 7,
    };
    simple(&G, 0x3057_0000_0000_000f, "model-state-reads");
}

#[test]
fn model_evidence_pairings_match_the_shell() {
    const G: Group = Group {
        section: "model_evidence",
        pairs: &[
            Pair {
                module: "rust-model-maintenance.cjs",
                feature: "rust-model-maintenance",
                rows: &[
                    ("/api/models/presets/reload", &["POST"]),
                    ("/api/models/evidence", &["GET"]),
                    ("/api/models/evidence/import", &["POST"]),
                ],
            },
            Pair {
                module: "rust-model-evidence-recheck.cjs",
                feature: "rust-model-evidence-recheck",
                rows: &[("/api/models/evidence/recheck", &["POST"])],
            },
        ],
        index: &[
            SECRET_LINES,
            &["const admission = createAdmissionRoutes({", ""],
            &["  secret: FRONT_SECRET,", "  secret: OTHER,"],
            &["});"],
            &[
                "if (await inferenceAdmission.handle(req, res)) return;",
                "// inferenceAdmission.handle(req, res)",
                "",
            ],
        ],
        lib: &[],
        extras: &[
            (
                "core/server/rust-model-evidence-recheck.cjs",
                &[
                    Some("const OWNED_ROUTES = new Set(([\n  'POST /api/models/evidence/recheck',\n]));\n"),
                    Some("const OWNED_ROUTES = new Set((['ALL /api/models/evidence/recheck']));\n"),
                    Some("const OWNED_ROUTES = new Set((['POST /api/models/evidence/recheck', 'GET /api/models/evidence']));\n"),
                    None,
                ],
            ),
            (
                "core/server/routes/models.cjs",
                &[
                    Some("router.get('/api/models', list);\n"),
                    Some("const probe = createVisionProbe(model);\n"),
                    Some("file({ source: 'recheck' });\n"),
                    None,
                ],
            ),
            (
                "core/server/inference-admission-lease.cjs",
                &[Some("'use strict';\n"), None],
            ),
        ],
        near: &[],
        min_verdicts: 8,
    };
    simple(&G, 0xe41d_0000_0000_0010, "model-evidence");
}

// ---------------------------------------------------------------------------------------------------------
// Fixed cases (no shell)

fn tree_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("a temp dir");
    for (rel, data) in files {
        write(&tmp.path().join(rel), data);
    }
    tmp
}

fn features_lib(features: &[&str]) -> String {
    let mut s = String::from("pub const FEATURES: &[&str] = &[\n");
    for f in features {
        s.push_str(&format!("    \"{f}\",\n"));
    }
    s.push_str("];\n");
    s
}

#[test]
fn every_name_passes_on_an_empty_tree() {
    let tmp = tempfile::tempdir().expect("a temp dir");
    for name in route_pairings::NAMES {
        let got = route_pairings::by_name(tmp.path(), name, Earlier::default());
        assert_eq!(got, Some(Ok(())), "{name}");
    }
    assert_eq!(
        route_pairings::by_name(tmp.path(), "nope", Earlier::default()),
        None
    );
}

#[test]
fn a_mixed_pair_is_refused_and_a_full_pair_passes() {
    let lib = features_lib(&["rust-provider-models"]);
    let t = tree_with(&[("rs/bins/noevia-server/src/lib.rs", &lib)]);
    assert_eq!(
        route_pairings::route_pair(t.path(), &route_pairings::PROVIDER_MODELS),
        Err("provider model list ownership pairing: Core and Rust must move together".to_string())
    );
    let t = tree_with(&[
        ("rs/bins/noevia-server/src/lib.rs", &lib),
        (
            "core/server/index.cjs",
            "const m = require('./rust-provider-models.cjs');\n",
        ),
    ]);
    assert_eq!(
        route_pairings::route_pair(t.path(), &route_pairings::PROVIDER_MODELS),
        Err("provider model list ownership pairing: missing route contract".to_string())
    );
    let t = tree_with(&[
        ("rs/bins/noevia-server/src/lib.rs", &lib),
        (
            "core/server/index.cjs",
            "const m = require('./rust-provider-models.cjs');\n",
        ),
        (
            "rs/contracts/http/routes.toml",
            "[[route]]\npath = \"/api/providers/{providerId}/models{*?}\"\nowner = \"rust\"\n",
        ),
    ]);
    assert_eq!(
        route_pairings::route_pair(t.path(), &route_pairings::PROVIDER_MODELS),
        Ok(true)
    );
}

#[test]
fn an_ere_marker_ignores_comments_and_a_fixed_one_does_not() {
    // MCP credentials read index.cjs through code_has: a commented require is no load, so the front alone
    // moved is a mixed pair; decision settings read it with grep -F, so the comment counts.
    let lib = features_lib(&["rust-mcp-credentials", "rust-decision-settings"]);
    let t = tree_with(&[
        ("rs/bins/noevia-server/src/lib.rs", &lib),
        (
            "core/server/index.cjs",
            "// require('./rust-mcp-credentials.cjs')\n// require('./rust-decision-settings.cjs')\n",
        ),
    ]);
    assert_eq!(
        route_pairings::mcp_credentials_pairing(t.path()),
        Err("MCP credential ownership pairing: Core and Rust must move together".to_string())
    );
    assert_eq!(
        route_pairings::route_pair(t.path(), &route_pairings::DECISION_SETTINGS),
        Err("decision settings ownership pairing: missing route contract".to_string())
    );
}

#[test]
fn the_probe_widens_with_the_registry() {
    assert!(Want::Counted(&["POST", "PUT", "DELETE"], 3).allow(b"DELETE,POST,PUT"));
    let probe_only = features_lib(&["rust-provider-test"]);
    let toml_post = "[[route]]\npath = \"/api/providers/test\"\nowner = \"rust\"\nmethods = [\"POST\"]\n";
    let t = tree_with(&[
        ("rs/bins/noevia-server/src/lib.rs", &probe_only),
        (
            "core/server/index.cjs",
            "require('./rust-provider-test.cjs');\n",
        ),
        ("rs/contracts/http/routes.toml", toml_post),
    ]);
    assert_eq!(route_pairings::provider_pairings(t.path()), Ok(()));
    let toml_three = "[[route]]\npath = \"/api/providers/test\"\nowner = \"rust\"\nmethods = [\"POST\", \"PUT\", \"DELETE\"]\n";
    let t = tree_with(&[
        ("rs/bins/noevia-server/src/lib.rs", &probe_only),
        (
            "core/server/index.cjs",
            "require('./rust-provider-test.cjs');\n",
        ),
        ("rs/contracts/http/routes.toml", toml_three),
    ]);
    assert_eq!(
        route_pairings::provider_pairings(t.path()),
        Err("provider test ownership pairing: exactly POST of the path must belong to Rust".to_string())
    );
}

#[test]
fn a_recheck_without_its_pair_stays_nodes() {
    let lib = features_lib(&["rust-model-maintenance"]);
    let toml = "[[route]]\npath = \"/api/models/presets/reload\"\nowner = \"rust\"\nmethods = [\"POST\"]\n\
[[route]]\npath = \"/api/models/evidence\"\nowner = \"rust\"\nmethods = [\"GET\"]\n\
[[route]]\npath = \"/api/models/evidence/import\"\nowner = \"rust\"\nmethods = [\"POST\"]\n\
[[route]]\npath = \"/api/models/evidence/recheck\"\nowner = \"rust\"\nmethods = [\"POST\"]\n";
    let t = tree_with(&[
        ("rs/bins/noevia-server/src/lib.rs", &lib),
        (
            "core/server/index.cjs",
            "require('./rust-model-maintenance.cjs');\n",
        ),
        ("rs/contracts/http/routes.toml", toml),
    ]);
    assert_eq!(
        route_pairings::model_evidence_pairings(t.path()),
        Err("model maintenance ownership pairing: the recheck stays Node's".to_string())
    );
}

#[test]
fn cli_route_pairing_names_and_verdicts() {
    let bin = env!("CARGO_BIN_EXE_release-assemble");
    let tmp = tempfile::tempdir().expect("a temp dir");
    let dir = tmp.path().to_str().expect("a utf-8 path");
    for name in route_pairings::NAMES {
        let out = Command::new(bin)
            .args(["pairing", name, dir])
            .output()
            .expect("the binary runs");
        assert!(out.status.success(), "{name}: {out:?}");
    }
    for verdicts in ["", "auto-roles-write", "discovery", "auto-roles-write,discovery"] {
        let out = Command::new(bin)
            .args(["pairing", "close-out", dir, verdicts])
            .output()
            .expect("the binary runs");
        assert!(out.status.success(), "{verdicts:?}: {out:?}");
    }
    let out = Command::new(bin)
        .args(["pairing", "close-out", dir, "bogus"])
        .output()
        .expect("the binary runs");
    assert_eq!(out.status.code(), Some(2));
    // A refusal: the front alone moved.
    write(
        &tmp.path().join("rs/bins/noevia-server/src/lib.rs"),
        &features_lib(&["rust-offsite-backup"]),
    );
    let out = Command::new(bin)
        .args(["pairing", "offsite-backup", dir])
        .output()
        .expect("the binary runs");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim_end(),
        "assemble-release: off-site backup ownership pairing: Core and Rust must move together"
    );
}
