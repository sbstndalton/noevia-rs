//! The single-path pairings whose Core side is a refusal module plus a facts or state bridge (council p10):
//! auto-tune settings (#2287), dismissal (#2293), resume (#2301), cancellation (#2294) and untuned
//! discovery (#2295), the idle unload read (#1863), the model assignments read (#2238) and the preset read.
//! Every one of these blocks in `assemble-release.sh` has the same shape:
//!
//! 1. Core's `index.cjs` requires the refusal module exactly when the front's `FEATURES` names the
//!    capability (`Core and Rust must move together`).
//! 2. The path's rows in `routes.toml` all belong to the paired owner: Rust's with no switch and the one
//!    method set the pair moves (every method, or only `GET`), Node's otherwise.
//! 3. With the new pair: exactly one such row, the refusal module refuses exactly the moved methods, the
//!    old public handler and policy are gone (`! code_has`), what stays in Core is still there
//!    (`code_has … && …`), the bridge file exists, `index.cjs` mounts it and hands its constructor the
//!    front's captured secret within three lines.
//!
//! [`FactsPairing`] is that block as data; each `const` below is one block, with its refusal texts as the
//! script spells them (including `auto-tune resume`'s copied "the dismissal path"). `tests/guard_facts.rs`
//! compares each with its verbatim block over generated trees. The model downloads read is not here: its
//! residual-fallback branch runs Core's module under `node`. Inert: nothing in the release path calls this.

use crate::bridged::{captured_secret, code_has, secret_near};
use crate::guard::{
    code_lines, contains, grep_fixed, move_capability, project_routes_core, read_file, records,
    routes_toml_file, tab_fields, MoveKind,
};
use std::path::Path;

/// How the refusal module must state the moved methods.
#[derive(Clone, Copy, Debug)]
pub enum Refusal<'a> {
    /// `project_routes_core FILE | tr '\n' ' '` equals this one entry (`ALL /api/…` or `GET /api/…`).
    Owned(&'a str),
    /// `code_has FILE ERE`: one code line matches (the exact `OWNED_ROUTES` declaration as a regex).
    Line(&'a str),
    /// The block's awk over the raw file: comment lines and `/* … */` blocks skipped, exactly one line
    /// starting `const OWNED_ROUTES = `, and the last one equal to this line. A missing file is refused.
    OnlyDeclaration(&'a str),
}

/// One check after the refusal, in the block's order. Files are relative to `core/server/` unless they
/// start with `rs/`.
#[derive(Clone, Copy, Debug)]
pub enum Check<'a> {
    /// `! code_has FILE ERE || die TEXT`.
    Gone {
        file: &'a str,
        ere: &'a str,
        text: &'a str,
    },
    /// `code_has F1 E1 && code_has F2 E2 … || die TEXT`.
    Kept {
        all: &'a [(&'a str, &'a str)],
        text: &'a str,
    },
    /// `[ ! -f FILE ] || die TEXT`.
    NoFile { file: &'a str, text: &'a str },
}

/// One facts or state bridge pairing.
#[derive(Clone, Copy, Debug)]
pub struct FactsPairing<'a> {
    pub label: &'a str,
    /// The refusal module, as `index.cjs` requires it (`rust-autotune-clear.cjs`).
    pub module: &'a str,
    pub rs_feature: &'a str,
    pub path: &'a str,
    /// The methods field a Rust row must carry: `-` (every method) or `GET`.
    pub methods: &'a str,
    /// The die text when a row belongs to the wrong owner.
    pub rows_text: &'a str,
    pub refusal: Refusal<'a>,
    pub refusal_text: &'a str,
    pub checks: &'a [Check<'a>],
    pub bridge_file: &'a str,
    /// `state bridge` or `facts bridge`, as the block's texts name it.
    pub bridge: &'a str,
    /// The mount `index.cjs` must hold (a fixed string) and the bridge's constructor (an ERE).
    pub mount: &'a str,
    pub ctor: &'a str,
}

const MODELS: &str = "routes/models.cjs";
const AUTOTUNE: &str = "llamacpp-full-autotune.cjs";
const MANAGER: &str = "llamacpp-manager.cjs";

/// #2287 auto-tune settings.
pub const AUTOTUNE_SETTINGS: FactsPairing<'static> = FactsPairing {
    label: "auto-tune settings ownership pairing",
    module: "rust-autotune-settings.cjs",
    rs_feature: "rust-autotune-settings",
    path: "/api/models/autotune/settings",
    methods: "-",
    rows_text: "all methods must belong to the paired owner",
    refusal: Refusal::Owned("ALL /api/models/autotune/settings"),
    refusal_text: "Core must refuse exactly the settings path",
    checks: &[
        Check::Gone {
            file: MODELS,
            ere: "/api/models/autotune/settings",
            text: "Core still carries the public handler",
        },
        Check::Gone {
            file: AUTOTUNE,
            ere: "function setSettings",
            text: "Core still carries the public settings policy",
        },
        Check::Kept {
            all: &[
                (MANAGER, "settingsFacts: autotuner.settingsFacts"),
                (MANAGER, "commitSettings: autotuner.commitSettings"),
            ],
            text: "Core does not expose the live actor primitives",
        },
    ],
    bridge_file: "rust-autotune-settings-bridge.cjs",
    bridge: "state bridge",
    mount: "autotuneSettingsBridge(req, res)",
    ctor: r"createAutotuneSettingsBridge\(\{",
};

/// #1863 idle unload read: only the GET moves; the PUT writer stays in Core.
pub const IDLE_UNLOAD_READ: FactsPairing<'static> = FactsPairing {
    label: "idle unload read ownership pairing",
    module: "rust-idle-unload-read.cjs",
    rs_feature: "rust-idle-unload-read",
    path: "/api/models/idle-unload",
    methods: "GET",
    rows_text: "GET only; PUT and other methods stay Node",
    refusal: Refusal::Owned("GET /api/models/idle-unload"),
    refusal_text: "Core must refuse exactly GET idle unload",
    checks: &[
        Check::Gone {
            file: MODELS,
            ere: "idleUnload[.]get[[:space:]]*[(]",
            text: "Core still carries the public GET handler",
        },
        Check::Kept {
            all: &[(MODELS, "idleUnload[.]save[[:space:]]*[(]")],
            text: "PUT must stay Node-owned",
        },
        Check::Kept {
            all: &[(
                "idle-unload-setting.cjs",
                "function effectiveSeconds[[:space:]]*[(]",
            )],
            text: "Core does not expose the live timer fact",
        },
    ],
    bridge_file: "rust-idle-unload-read-facts.cjs",
    bridge: "facts bridge",
    mount: "idleUnloadFacts(req, res)",
    ctor: r"createIdleUnloadFacts\(\{",
};

/// #2238 model assignments read: only the GET moves.
pub const MODEL_ASSIGNMENTS: FactsPairing<'static> = FactsPairing {
    label: "model assignments read ownership pairing",
    module: "rust-model-assignments.cjs",
    rs_feature: "rust-model-assignments",
    path: "/api/models/assignments",
    methods: "GET",
    rows_text: "GET only; other methods stay Node",
    refusal: Refusal::OnlyDeclaration(
        "const OWNED_ROUTES = Object.freeze(new Set(['GET /api/models/assignments']));",
    ),
    refusal_text: "Core must refuse exactly GET model assignments",
    checks: &[Check::NoFile {
        file: "model-assignments.cjs",
        text: "Core still carries the public GET formatter",
    }],
    bridge_file: "rust-model-assignments-facts.cjs",
    bridge: "facts bridge",
    mount: "modelAssignmentsFacts(req, res)",
    ctor: r"createModelAssignmentsFacts\(\{",
};

/// Preset read: only the GET moves; PUT keeps the snapshot owner and the conditional writer.
pub const PRESET_READ: FactsPairing<'static> = FactsPairing {
    label: "preset read ownership pairing",
    module: "rust-preset-read.cjs",
    rs_feature: "rust-preset-read",
    path: "/api/models/preset",
    methods: "GET",
    rows_text: "GET only; PUT and other methods stay Node",
    refusal: Refusal::Line(
        "^const OWNED_ROUTES = Object[.]freeze[(]new Set[(][[]'GET /api/models/preset'[]][)][)];$",
    ),
    refusal_text: "Core must refuse exactly this GET",
    checks: &[
        Check::Gone {
            file: MODELS,
            ere: "modelManager[.]getPreset[[:space:]]*[(]",
            text: "Core still carries the public GET handler",
        },
        Check::Kept {
            all: &[(MODELS, "modelManager[.]applyPreset[[:space:]]*[(]")],
            text: "PUT must stay Node-owned",
        },
        Check::Kept {
            all: &[(MANAGER, "presetFacts:.*presets[?][.]get[(]")],
            text: "missing shared preset snapshot",
        },
    ],
    bridge_file: "rust-preset-read-bridge.cjs",
    bridge: "facts bridge",
    mount: "presetReadBridge(req, res)",
    ctor: r"createPresetReadBridge\(\{",
};

/// #2293 auto-tune dismissal.
pub const AUTOTUNE_CLEAR: FactsPairing<'static> = FactsPairing {
    label: "autotune dismissal ownership pairing",
    module: "rust-autotune-clear.cjs",
    rs_feature: "rust-autotune-clear",
    path: "/api/models/autotune/clear",
    methods: "-",
    rows_text: "all methods must belong to the paired owner",
    refusal: Refusal::Owned("ALL /api/models/autotune/clear"),
    refusal_text: "Core must refuse exactly the dismissal path",
    checks: &[
        Check::Gone {
            file: MODELS,
            ere: "/api/models/autotune/clear",
            text: "Core still carries the public handler",
        },
        Check::Gone {
            file: AUTOTUNE,
            ere: r"(^|[^A-Za-z0-9_$])function[[:space:]]+clear[[:space:]]*\(",
            text: "Core still carries the public dismissal policy",
        },
        Check::Kept {
            all: &[
                (MANAGER, "clearFacts: autotuner.clearFacts"),
                (MANAGER, "compareAndClear: autotuner.compareAndClear"),
            ],
            text: "Core does not expose the live actor primitives",
        },
    ],
    bridge_file: "rust-autotune-clear-bridge.cjs",
    bridge: "state bridge",
    mount: "autotuneClearBridge(req, res)",
    ctor: r"createAutotuneClearBridge\(\{",
};

/// #2301 auto-tune resume. The refusal text says "the dismissal path" in the script (copied from #2293);
/// the port keeps it so the texts compare equal.
pub const AUTOTUNE_RESUME: FactsPairing<'static> = FactsPairing {
    label: "auto-tune resume ownership pairing",
    module: "rust-autotune-resume.cjs",
    rs_feature: "rust-autotune-resume",
    path: "/api/models/autotune/resume",
    methods: "-",
    rows_text: "all methods must belong to the paired owner",
    refusal: Refusal::Owned("ALL /api/models/autotune/resume"),
    refusal_text: "Core must refuse exactly the dismissal path",
    checks: &[
        Check::Gone {
            file: MODELS,
            ere: "/api/models/autotune/resume",
            text: "Core still carries the public handler",
        },
        Check::Gone {
            file: AUTOTUNE,
            ere: "(^|[^[:alnum:]_$])function[[:space:]]+resume[[:space:]]*[(]",
            text: "Core still carries the public resume policy",
        },
        Check::Kept {
            all: &[
                (MANAGER, "resumeFacts: autotuner.resumeFacts"),
                (MANAGER, "prepareResume: autotuner.prepareResume"),
                (MANAGER, "compareAndResume: autotuner.compareAndResume"),
            ],
            text: "Core does not expose the live actor primitives",
        },
    ],
    bridge_file: "rust-autotune-resume-bridge.cjs",
    bridge: "state bridge",
    mount: "autotuneResumeBridge(req, res)",
    ctor: r"createAutotuneResumeBridge\(\{",
};

/// #2294 auto-tune cancellation.
pub const AUTOTUNE_CANCEL: FactsPairing<'static> = FactsPairing {
    label: "auto-tune cancellation ownership pairing",
    module: "rust-autotune-cancel.cjs",
    rs_feature: "rust-autotune-cancel",
    path: "/api/models/autotune/cancel",
    methods: "-",
    rows_text: "all methods must belong to the paired owner",
    refusal: Refusal::Owned("ALL /api/models/autotune/cancel"),
    refusal_text: "Core must refuse exactly the cancellation path",
    checks: &[
        Check::Gone {
            file: MODELS,
            ere: "/api/models/autotune/cancel",
            text: "Core still carries the public handler",
        },
        Check::Gone {
            file: AUTOTUNE,
            ere: "function cancel()",
            text: "Core still carries the public cancellation policy",
        },
        Check::Kept {
            all: &[
                (MANAGER, "cancellationFacts: autotuner.cancellationFacts"),
                (MANAGER, "compareAndAbort: autotuner.compareAndAbort"),
            ],
            text: "Core does not expose the live actor primitives",
        },
    ],
    bridge_file: "rust-autotune-cancel-bridge.cjs",
    bridge: "state bridge",
    mount: "autotuneCancelBridge(req, res)",
    ctor: r"createAutotuneCancelBridge\(\{",
};

/// #2295 untuned discovery: the policy is shared Rust/WASM, Core keeps the raw facts.
pub const AUTOTUNE_UNTUNED: FactsPairing<'static> = FactsPairing {
    label: "untuned discovery ownership pairing",
    module: "rust-autotune-untuned.cjs",
    rs_feature: "rust-autotune-untuned",
    path: "/api/models/autotune/untuned",
    methods: "-",
    rows_text: "all methods must belong to the paired owner",
    refusal: Refusal::Owned("ALL /api/models/autotune/untuned"),
    refusal_text: "Core must refuse exactly the discovery path",
    checks: &[
        Check::Gone {
            file: MODELS,
            ere: "/api/models/autotune/untuned",
            text: "Core still carries the public handler",
        },
        Check::Gone {
            file: AUTOTUNE,
            ere: r"async[[:space:]]+function[[:space:]]+untuned[[:space:]]*\(",
            text: "Core still carries the public discovery policy",
        },
        Check::Kept {
            all: &[
                (MANAGER, "scanFacts: autotuner.scanFacts"),
                (AUTOTUNE, r"[.]autotuneScan[[:space:]]*\("),
                ("dav-parse-wasm.cjs", r"[.]autotune_scan[[:space:]]*\("),
                (
                    "rs/bins/dav-parse-wasm/src/lib.rs",
                    r"fn[[:space:]]+autotune_scan[[:space:]]*\(",
                ),
            ],
            text: "shared Rust/WASM policy and raw owner facts required",
        },
        Check::Gone {
            file: AUTOTUNE,
            ere: "latest?.version === VERSION",
            text: "Core still carries candidate selection policy",
        },
    ],
    bridge_file: "rust-autotune-untuned-facts.cjs",
    bridge: "state bridge",
    mount: "autotuneUntunedFacts(req, res)",
    ctor: r"createAutotuneUntunedFacts\(\{",
};

/// Every facts pairing, in the script's order, with its CLI name.
pub const ALL: &[(&str, &FactsPairing<'static>)] = &[
    ("autotune-settings", &AUTOTUNE_SETTINGS),
    ("idle-unload-read", &IDLE_UNLOAD_READ),
    ("model-assignments", &MODEL_ASSIGNMENTS),
    ("preset-read", &PRESET_READ),
    ("autotune-clear", &AUTOTUNE_CLEAR),
    ("autotune-resume", &AUTOTUNE_RESUME),
    ("autotune-cancel", &AUTOTUNE_CANCEL),
    ("autotune-untuned", &AUTOTUNE_UNTUNED),
];

/// The pairing a CLI name stands for.
pub fn by_name(name: &str) -> Option<&'static FactsPairing<'static>> {
    ALL.iter().find(|(n, _)| *n == name).map(|(_, p)| *p)
}

/// A tree file: `rs/…` from the tree root, anything else under `core/server/`.
fn tree_file(tree: &Path, rel: &str) -> std::path::PathBuf {
    if rel.starts_with("rs/") {
        tree.join(rel)
    } else {
        tree.join("core/server").join(rel)
    }
}

/// The block's awk for [`Refusal::OnlyDeclaration`] over the raw file.
pub fn only_declaration(src: Option<&[u8]>, line: &str) -> bool {
    let Some(src) = src else {
        return false;
    };
    let (mut block, mut markers, mut exact) = (false, 0usize, false);
    for l in records(src) {
        if block {
            if contains(l, b"*/") {
                block = false;
            }
            continue;
        }
        let start = l
            .iter()
            .position(|c| !c.is_ascii_whitespace() && *c != 0x0b)
            .unwrap_or(l.len());
        let rest = l.get(start..).unwrap_or_default();
        if rest.starts_with(b"//") {
            continue;
        }
        if rest.starts_with(b"/*") {
            if !contains(l, b"*/") {
                block = true;
            }
            continue;
        }
        if l.starts_with(b"const OWNED_ROUTES = ") {
            markers += 1;
            exact = l == line.as_bytes();
        }
    }
    markers == 1 && exact
}

/// The rows of `p.path` for the pair's owner: `Ok(n)` rows, `Err(())` the awk's `exit 1`. A table that
/// `routes_toml` refuses reads as empty (`|| table=""`).
fn rows(tree: &Path, p: &FactsPairing, rust: bool) -> Result<usize, ()> {
    let rt = routes_toml_file(&tree.join("rs/contracts/http/routes.toml"));
    if rt.error.is_some() {
        return Ok(0);
    }
    let mut n = 0usize;
    for r in &rt.routes {
        let [path, owner, switch, methods] = tab_fields(r);
        if path != p.path.as_bytes() {
            continue;
        }
        let ok = if rust {
            owner == b"rust" && switch == b"-" && methods == p.methods.as_bytes()
        } else {
            owner == b"node"
        };
        if !ok {
            return Err(());
        }
        n += 1;
    }
    Ok(n)
}

/// Run one facts pairing over an assembled tree. `Err` holds the `die` text without the
/// `assemble-release: ` prefix.
pub fn facts_pairing(tree: &Path, p: &FactsPairing) -> Result<(), String> {
    let die = |what: &str| Err(format!("{}: {what}", p.label));
    let core = grep_fixed(
        &tree.join("core/server/index.cjs"),
        &format!("require('./{}')", p.module),
    );
    let rs = move_capability(
        read_file(&tree.join("rs/bins/noevia-server/src/lib.rs")).as_deref(),
        "FEATURES",
        p.rs_feature,
        MoveKind::Features,
    );
    if core != rs {
        return die("Core and Rust must move together");
    }
    let mut n = 0usize;
    if tree.join("rs/contracts/http/routes.toml").is_file() {
        match rows(tree, p, rs) {
            Ok(k) => n = k,
            Err(()) => return die(p.rows_text),
        }
    }
    if !rs {
        return Ok(());
    }
    if n != 1 {
        return die("incomplete route ownership");
    }
    let module = read_file(&tree_file(tree, p.module));
    let refused = match p.refusal {
        Refusal::Owned(entry) => {
            project_routes_core(module.as_deref(), "OWNED_ROUTES") == [entry.as_bytes().to_vec()]
        }
        Refusal::Line(ere) => code_has(module.as_deref(), ere),
        Refusal::OnlyDeclaration(line) => only_declaration(module.as_deref(), line),
    };
    if !refused {
        return die(p.refusal_text);
    }
    for check in p.checks {
        match *check {
            Check::Gone { file, ere, text } => {
                if code_has(read_file(&tree_file(tree, file)).as_deref(), ere) {
                    return die(text);
                }
            }
            Check::Kept { all, text } => {
                let kept = all
                    .iter()
                    .all(|(file, ere)| code_has(read_file(&tree_file(tree, file)).as_deref(), ere));
                if !kept {
                    return die(text);
                }
            }
            Check::NoFile { file, text } => {
                if tree_file(tree, file).is_file() {
                    return die(text);
                }
            }
        }
    }
    if !tree_file(tree, p.bridge_file).is_file() {
        return die(&format!("missing {}", p.bridge));
    }
    let index = code_lines(read_file(&tree.join("core/server/index.cjs")).as_deref());
    if !contains(&index, p.mount.as_bytes()) {
        return die(&format!("missing {} mount", p.bridge));
    }
    match captured_secret(&index) {
        Some(secret) if secret_near(&index, p.ctor, &secret) => Ok(()),
        _ => die(&format!("{} needs the front captured secret", p.bridge)),
    }
}
