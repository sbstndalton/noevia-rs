//! Track R11's three model pairings of assemble-release.sh (council p12): the inference budget and idle unload
//! PUTs with the preset save, MTP check and pull (`NOEVIA_RUST_MODEL_WRITES`), the model manager proxy
//! (`NOEVIA_RUST_MODEL_MANAGER`) and `/api/embedding` (`NOEVIA_RUST_EMBEDDING_ADMIN`), with the primitives they
//! need beyond [`crate::guard`] and [`crate::bridged`]: `code_body`, `ex_front_named` and the Core list reader
//! `mw_core_has`. Each pairing checks in the script's order and fails with the script's text. Verbatim blocks:
//! `fixtures/assemble-release-model-pairings-excerpt.sh` (noevia f23eb731); `tests/model_pairings.rs` runs them
//! as the oracle.

use crate::bridged::{code_has, ere};
use crate::core_requires::{core_requires_file, Requires};
use crate::guard::{contains, read_file, records};
use std::fs;
use std::path::{Path, PathBuf};

/// `"$(code_body FILE)"`: the code lines the way `code_has` reads them (no `//` lines, no `/* … */` blocks), up
/// to a Rust file's first top-level `#[cfg(test)]` module (a `#[cfg(test)]` on anything else does not end it),
/// with the trailing newlines the command substitution drops. A missing file gives nothing.
pub fn code_body(src: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(src) = src else {
        return out;
    };
    // awk reads `\t` in the bracket as a tab; POSIX brackets have no escapes, so the tab is written as one.
    let module = ere("^(pub(\\([a-z]+\\))? )?mod[ \t]");
    let mut block = false;
    let mut pend = false;
    for raw in records(src) {
        if block {
            if contains(raw, b"*/") {
                block = false;
            }
            continue;
        }
        if raw.starts_with(b"#[cfg(test)]") {
            pend = true;
            continue;
        }
        if pend {
            pend = false;
            if module.as_ref().is_some_and(|re| re.is_match(raw)) {
                break;
            }
        }
        let start = raw
            .iter()
            .position(|c| *c != b' ' && *c != b'\t')
            .unwrap_or(raw.len());
        let line = raw.get(start..).unwrap_or_default();
        if line.starts_with(b"//") {
            continue;
        }
        if line.starts_with(b"/*") {
            if !contains(line, b"*/") {
                block = true;
            }
            continue;
        }
        out.extend_from_slice(raw);
        out.push(b'\n');
    }
    while out.last() == Some(&b'\n') {
        out.pop();
    }
    out
}

/// `grep -qF NEEDLE <<<"$text"`: some line holds the needle (a needle without a newline).
pub fn text_has_fixed(text: &[u8], needle: &str) -> bool {
    text.split(|c| *c == b'\n')
        .any(|l| contains(l, needle.as_bytes()))
}

/// `grep -qE ERE <<<"$text"` (C locale). A pattern grep refuses is false.
pub fn text_has(text: &[u8], pattern: &str) -> bool {
    match ere(pattern) {
        Some(re) => text.split(|c| *c == b'\n').any(|l| re.is_match(l)),
        None => false,
    }
}

/// `ex_front_named NEEDLE`: `grep -rqsF --include='*.rs' -e NEEDLE "$ex_rs_src"`. The directory itself is
/// followed when it is a link; links met inside it are skipped, as `grep -r` does. A missing directory is false.
pub fn front_named(tree: &Path, needle: &str) -> bool {
    fn walk(dir: &Path, needle: &[u8]) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        paths.into_iter().any(|p| {
            let Ok(meta) = fs::symlink_metadata(&p) else {
                return false;
            };
            if meta.file_type().is_symlink() {
                return false;
            }
            if meta.is_dir() {
                return walk(&p, needle);
            }
            let rs = p
                .file_name()
                .is_some_and(|n| n.as_encoded_bytes().ends_with(b".rs"));
            rs && meta.is_file() && fs::read(&p).is_ok_and(|b| contains(&b, needle))
        })
    }
    walk(&rs_src(tree), needle.as_bytes())
}

fn rs_src(tree: &Path) -> PathBuf {
    tree.join("rs/bins/noevia-server/src")
}

fn core(tree: &Path, file: &str) -> PathBuf {
    tree.join("core/server").join(file)
}

fn read(path: &Path) -> Option<Vec<u8>> {
    read_file(path)
}

/// `code_body` over a path, `Err` where the script's awk fails on a file that exists but cannot be read.
fn body_of(path: &Path) -> Result<Vec<u8>, ()> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    fs::read(path).map(|b| code_body(Some(&b))).map_err(|_| ())
}

/// The front's switch declaration and its `rust` reading, both in the front module's body.
fn front_reads_switch(front: &[u8], switch: &str) -> bool {
    text_has_fixed(front, &format!("pub const SWITCH: &str = \"{switch}\";"))
        && text_has_fixed(front, "get(SWITCH).as_deref() == Some(\"rust\")")
}

/// Core's refusal module declaring the same switch and comparing it with `'rust'`.
fn core_reads_switch(module: Option<&[u8]>, switch: &str) -> bool {
    code_has(module, &format!("^const SWITCH = '{switch}';"))
        && code_has(module, r"env\[SWITCH\][[:space:]]*===[[:space:]]*'rust'")
}

/// `mw_core_has LIST PATH` over `rust-model-writes.cjs`'s code body: the list is read from its `const LIST = `
/// line to the line that closes it (`];` or `]);`), every `//` comment cut first. `Ok(true)` = status 0,
/// `Ok(false)` = 1 (no such list, or the path not in it), `Err` = 2 (unreadable, or the list not closed before
/// the next declaration or the end).
fn mw_core_has(module: &Path, list: &str, path: &str) -> Result<bool, ()> {
    let body = body_of(module)?;
    let start = format!("const {list} = ");
    let decl = ere(r"^(const|let|var|function|module[.]exports) ");
    let mut on = false;
    let mut done = false;
    let mut taken: Vec<Vec<u8>> = Vec::new();
    for line in body.split(|c| *c == b'\n') {
        if on && decl.as_ref().is_some_and(|re| re.is_match(line)) {
            return Err(());
        }
        if !on && line.starts_with(start.as_bytes()) {
            on = true;
        }
        if on {
            let cut = match line.windows(2).position(|w| w == b"//") {
                Some(k) => line.get(..k).unwrap_or_default(),
                None => line,
            };
            taken.push(cut.to_vec());
            if contains(cut, b"];") || contains(cut, b"]);") {
                done = true;
                break;
            }
        }
    }
    if on && !done {
        return Err(());
    }
    let quoted = format!("'{path}'");
    Ok(taken.iter().any(|l| contains(l, quoted.as_bytes())))
}

fn loaded(tree: &Path, module: &str) -> Requires {
    core_requires_file(&core(tree, "index.cjs"), &format!("./{module}"))
}

const MW: &str = "model writes pairing";

/// One of the later model write requests: Core refuses it under the switch exactly when the front answers it.
fn mw_pair(
    tree: &Path,
    what: &str,
    front_answers: bool,
    list: &str,
    path: &str,
) -> Result<(), String> {
    let rc = mw_core_has(&core(tree, "rust-model-writes.cjs"), list, path).map_err(|()| {
        format!("{MW}: Core's rust-model-writes.cjs could not be read for {what} (a list not closed before the next declaration)")
    })?;
    if front_answers && !rc {
        return Err(format!(
            "{MW}: the Rust front answers {what} but Core's rust-model-writes.cjs does not refuse it (both would answer it)"
        ));
    }
    if !front_answers && rc {
        return Err(format!(
            "{MW}: Core's rust-model-writes.cjs refuses {what} but the Rust front does not answer it (every such request would be refused once the switch is set)"
        ));
    }
    Ok(())
}

/// The model writes pairing (`NOEVIA_RUST_MODEL_WRITES`, noevia#2628/#2648). `Err` holds the `die` text.
pub fn model_writes_pairing(tree: &Path) -> Result<(), String> {
    let src = rs_src(tree);
    if !src.join("model_writes_routes.rs").is_file() {
        if front_named(tree, "NOEVIA_RUST_MODEL_WRITES") {
            return Err(format!("{MW}: the Rust front names NOEVIA_RUST_MODEL_WRITES but has no bins/noevia-server/src/model_writes_routes.rs (the writer moved: update this guard)"));
        }
        return Ok(());
    }
    let front = code_body(read(&src.join("model_writes_routes.rs")).as_deref());
    let module = core(tree, "rust-model-writes.cjs");
    match loaded(tree, "rust-model-writes.cjs") {
        Requires::Invalid => return Err(format!("{MW}: Core's index.cjs could not be read")),
        Requires::Yes if module.is_file() => {}
        _ => return Err(format!("{MW}: the Rust front answers the inference budget and idle unload PUTs (model_writes_routes.rs, NOEVIA_RUST_MODEL_WRITES=rust) but Core does not load server/rust-model-writes.cjs (Core would go on answering them)")),
    }
    if !(front_reads_switch(&front, "NOEVIA_RUST_MODEL_WRITES")
        && core_reads_switch(read(&module).as_deref(), "NOEVIA_RUST_MODEL_WRITES"))
    {
        return Err(format!("{MW}: the Rust front's NOEVIA_RUST_MODEL_WRITES=rust and Core's rust-model-writes.cjs do not read the same switch value (the PUTs would be answered by both, or by neither)"));
    }
    if !code_has(
        read(&core(tree, "inference-budget.cjs")).as_deref(),
        r"^[[:space:]]*function savedGib[(]",
    ) {
        return Err(format!("{MW}: the Rust front saves the inference budget row but Core's inference-budget.cjs does not read it on every use (it would enforce the figure it read at its start until it restarts)"));
    }
    let preset = text_has(&front, "^pub const PRESET: &str = ");
    let pull_routes = src.join("model_pull_routes.rs");
    let mtp = pull_routes.is_file();
    let pull = text_has(
        &code_body(read(&pull_routes).as_deref()),
        "^pub const PULL: &str = ",
    );
    mw_pair(
        tree,
        "PUT /api/models/preset",
        preset,
        "OWNED_PATHS",
        "/api/models/preset",
    )?;
    mw_pair(
        tree,
        "GET /api/models/mtp-artifact",
        mtp,
        "OWNED_GETS",
        "/api/models/mtp-artifact",
    )?;
    mw_pair(
        tree,
        "POST /api/models/pull",
        pull,
        "OWNED_POSTS",
        "/api/models/pull",
    )
}

const MM: &str = "model manager proxy pairing";

/// The model manager proxy pairing (`NOEVIA_RUST_MODEL_MANAGER`, noevia#2648, #2550's scan cache clear).
pub fn model_manager_pairing(tree: &Path) -> Result<(), String> {
    let src = rs_src(tree);
    if !src.join("model_manager_routes.rs").is_file() {
        if front_named(tree, "NOEVIA_RUST_MODEL_MANAGER") {
            return Err(format!("{MM}: the Rust front names NOEVIA_RUST_MODEL_MANAGER but has no bins/noevia-server/src/model_manager_routes.rs (the proxy moved: update this guard)"));
        }
        return Ok(());
    }
    let front = code_body(read(&src.join("model_manager_routes.rs")).as_deref());
    let module = core(tree, "rust-model-manager-proxy.cjs");
    match loaded(tree, "rust-model-manager-proxy.cjs") {
        Requires::Invalid => return Err(format!("{MM}: Core's index.cjs could not be read")),
        Requires::Yes if module.is_file() => {}
        _ => return Err(format!("{MM}: the Rust front answers /api/model-manager/* (model_manager_routes.rs, NOEVIA_RUST_MODEL_MANAGER=rust) but Core does not load server/rust-model-manager-proxy.cjs (Core would go on answering it)")),
    }
    if !(front_reads_switch(&front, "NOEVIA_RUST_MODEL_MANAGER")
        && core_reads_switch(read(&module).as_deref(), "NOEVIA_RUST_MODEL_MANAGER"))
    {
        return Err(format!("{MM}: the Rust front's NOEVIA_RUST_MODEL_MANAGER=rust and Core's rust-model-manager-proxy.cjs do not read the same switch value (the proxy would be answered by both, or by neither)"));
    }
    if !code_has(
        read(&core(tree, "rust-model-manager-scan-cache.cjs")).as_deref(),
        "'clear'",
    ) {
        return Err(format!("{MM}: the Rust front clears Core's folder scan cache but Core's rust-model-manager-scan-cache.cjs has no clear op (a Core from before noevia#2550)"));
    }
    Ok(())
}

const EA: &str = "embedding admin pairing";

/// The embedding admin pairing (`NOEVIA_RUST_EMBEDDING_ADMIN`, noevia#2679): the refusal, the switch value,
/// Core's re-read of embedding-config.json and the announced service changes on both sides.
pub fn embedding_admin_pairing(tree: &Path) -> Result<(), String> {
    let src = rs_src(tree);
    if !src.join("embedding_admin_routes.rs").is_file() {
        if front_named(tree, "NOEVIA_RUST_EMBEDDING_ADMIN") {
            return Err(format!("{EA}: the Rust front names NOEVIA_RUST_EMBEDDING_ADMIN but has no bins/noevia-server/src/embedding_admin_routes.rs (the route moved: update this guard)"));
        }
        return Ok(());
    }
    let front = code_body(read(&src.join("embedding_admin_routes.rs")).as_deref());
    let module = core(tree, "rust-embedding-admin.cjs");
    let index = code_body(read(&core(tree, "index.cjs")).as_deref());
    match loaded(tree, "rust-embedding-admin.cjs") {
        Requires::Invalid => return Err(format!("{EA}: Core's index.cjs could not be read")),
        Requires::Yes
            if module.is_file()
                && text_has_fixed(
                    &index,
                    "require('./rust-embedding-admin.cjs').refuseOwned(req, res",
                ) => {}
        _ => return Err(format!("{EA}: the Rust front answers /api/embedding (embedding_admin_routes.rs, NOEVIA_RUST_EMBEDDING_ADMIN=rust) but Core does not refuse it through server/rust-embedding-admin.cjs (Core would go on answering it)")),
    }
    if !(front_reads_switch(&front, "NOEVIA_RUST_EMBEDDING_ADMIN")
        && core_reads_switch(read(&module).as_deref(), "NOEVIA_RUST_EMBEDDING_ADMIN"))
    {
        return Err(format!("{EA}: the Rust front's NOEVIA_RUST_EMBEDDING_ADMIN=rust and Core's rust-embedding-admin.cjs do not read the same switch value (the path would be answered by both, or by neither)"));
    }
    let rereads = code_has(
        read(&core(tree, "embedding-config.cjs")).as_deref(),
        r"^[[:space:]]*function refresh[(][)]",
    ) && text_has_fixed(
        &index,
        "reread: require('./rust-embedding-admin.cjs').frontOwns()",
    );
    if !rereads {
        return Err(format!("{EA}: the Rust front writes embedding-config.json but Core's embedding configuration does not read it again under the switch (embedding-config.cjs reread from index.cjs): Core would search and guard deletes with the model it read at its start"));
    }
    if !text_has_fixed(&front, "\"rust-embedding-service-intent-v1\"") {
        return Err(format!("{EA}: the Rust front changes the embedding service without announcing it (rust-embedding-service-intent-v1): Core's delete guard would not protect the model being loaded"));
    }
    let reads_intent = code_has(
        read(&core(tree, "embedding-service.cjs")).as_deref(),
        "^const INTENT_PROTOCOL = 'rust-embedding-service-intent-v1';",
    ) && text_has_fixed(
        &index,
        "intentFile: require('./rust-embedding-admin.cjs').frontOwns() ?",
    );
    if !reads_intent {
        return Err(format!("{EA}: the Rust front announces its embedding service changes but Core does not read them (embedding-service.cjs intent, from index.cjs): Core's delete guard would not protect the model being loaded"));
    }
    Ok(())
}

/// The CLI names of this module's pairings, in the script's order.
pub const ALL: [(&str, fn(&Path) -> Result<(), String>); 3] = [
    ("model-writes", model_writes_pairing),
    ("model-manager", model_manager_pairing),
    ("embedding-admin", embedding_admin_pairing),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_body_cuts_comments_and_the_test_module() {
        let src = b"a\n  // c\n/* x\n y */\nb\n#[cfg(test)]\nfn helper() {}\n#[cfg(test)]\nmod tests {\nz\n";
        assert_eq!(code_body(Some(src)), b"a\nb\nfn helper() {}".to_vec());
        assert_eq!(
            code_body(Some(b"#[cfg(test)]\npub(crate) mod t;\nq\n")),
            Vec::<u8>::new()
        );
        assert_eq!(code_body(Some(b"x\n\n\n")), b"x".to_vec());
        assert_eq!(code_body(None), Vec::<u8>::new());
    }

    #[test]
    fn text_matches_lines() {
        assert!(text_has_fixed(b"a\npub const X", "pub const X"));
        assert!(!text_has_fixed(b"", "x"));
        assert!(text_has(
            b"z\npub const PRESET: &str = \"p\";",
            "^pub const PRESET: &str = "
        ));
        assert!(!text_has(
            b"  pub const PRESET: &str = ",
            "^pub const PRESET: &str = "
        ));
    }
}
