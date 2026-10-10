//! M4: Node (core server/rust-projects.cjs, the merge it saves with) and Rust ([`store::update`])
//! write the same projects.json at the same time, as the chat routes (Node) and the image routes
//! (Rust) do once NOEVIA_RUST_PROJECTS is on. Neither may lose the other's changes, the lock
//! constants must be equal, and the file must stay Node's bytes. Runs when NOEVIA_CORE_CHECKOUT
//! points at a core checkout and `node` is on PATH (CI); skipped otherwise.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::JValue;
use server_projects::{lock, store};
use server_store::RustProjects;
use std::process::Command;

const ROUNDS: usize = 150;

fn core() -> Option<String> {
    match std::env::var("NOEVIA_CORE_CHECKOUT") {
        Ok(c) => Some(c),
        Err(_) => {
            eprintln!("NOEVIA_CORE_CHECKOUT not set: skipped");
            None
        }
    }
}

#[test]
fn lock_constants_match_node() {
    let Some(core) = core() else { return };
    let src = std::fs::read_to_string(std::path::Path::new(&core).join("server/rust-projects.cjs"))
        .unwrap();
    for (name, value) in [
        ("LOCK_SUFFIX", format!("'{}'", lock::SUFFIX)),
        ("LOCK_STALE_MS", lock::STALE_MS.to_string()),
        ("LOCK_WAIT_MS", lock::WAIT_MS.to_string()),
        ("LOCK_RETRY_MS", lock::RETRY_MS.to_string()),
    ] {
        let line = format!("const {name} = {value};");
        assert!(src.contains(&line), "rust-projects.cjs lacks `{line}`");
    }
}

/// Node's saves (`blind`: the pre-M4 atomicJson of its cached view, the negative control) racing
/// Rust's; returns the chat ids and image ids of p1 afterwards.
fn race(core: &str, blind: bool) -> (Vec<String>, Vec<String>, String) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(store::PROJECTS_FILE);
    std::fs::write(
        &file,
        "{\n  \"projects\": [\n    {\n      \"id\": \"p1\",\n      \"name\": \"Synthetic\",\n      \"chats\": []\n    },\n    {\n      \"id\": \"p2\",\n      \"name\": \"Other\"\n    }\n  ]\n}",
    )
    .unwrap();
    // Node: a long-lived workspace view, loaded before Rust's first write, that adds one chat meta
    // per save, refreshing first every other round (a request start) so both the refresh and the
    // stale-base merge paths run. `blind`: never refreshed, saved whole, as before M4.
    let script = r#"
      const [core, file, rounds] = process.argv.slice(1);
      const { createProjectsFile } = require(core + '/server/rust-projects.cjs');
      const { atomicJson } = require(core + '/server/workspace.cjs');
      const pf = createProjectsFile(file, { atomicJson, warn: (m) => { throw new Error(m); } });
      const projects = pf.load();
      const blind = process.argv[4] === 'blind';
      // Loaded before Rust's first image (Rust waits for this marker): from here on this view is
      // stale until refreshed.
      const fs = require('fs');
      fs.writeFileSync(file + '.loaded', '');
      const until = Date.now() + 20000;
      while (!fs.readFileSync(file, 'utf8').includes('"img-0"')) {
        if (Date.now() > until) throw new Error('Rust never wrote');
        Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 1);
      }
      for (let i = 0; i < Number(rounds); i++) {
        if (i % 2 && !blind) pf.refresh(projects);
        const p = projects.find((x) => x.id === 'p1');
        p.chats = [...(p.chats || []), { id: 'chat-' + i }];
        if (blind) atomicJson(file, { projects });
        else pf.save(projects);
      }
    "#;
    let mut node = Command::new("node")
        .args([
            "-e",
            script,
            core,
            file.to_str().unwrap(),
            &ROUNDS.to_string(),
            if blind { "blind" } else { "merge" },
        ])
        .spawn()
        .expect("node on PATH");
    let switch = RustProjects::from_env_value(Some("1")).unwrap();
    let loaded = dir.path().join("projects.json.loaded");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !loaded.exists() {
        assert!(std::time::Instant::now() < until, "node never loaded");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    for i in 0..ROUNDS {
        store::update(dir.path(), switch, |projects| {
            let p = store::find_mut(projects, "p1").unwrap();
            let mut assets = match p.get("assets") {
                JValue::Arr(a) => a.clone(),
                _ => Vec::new(),
            };
            assets.push(JValue::obj([("id", JValue::Str(format!("img-{i}")))]));
            store::set(p, "assets", JValue::Arr(assets));
            ((), true)
        })
        .unwrap();
    }
    assert!(node.wait().unwrap().success(), "node side failed");
    let text = std::fs::read_to_string(&file).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let p1 = &v["projects"][0];
    let ids = |k: &str| -> Vec<String> {
        p1[k]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|c| c["id"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    };
    assert_eq!(v["projects"][1]["name"], "Other");
    assert!(!dir.path().join("projects.json.lock").exists());
    (ids("chats"), ids("assets"), text)
}

#[test]
fn node_and_rust_writing_at_once_lose_nothing() {
    let Some(core) = core() else { return };
    let (chats, assets, text) = race(&core, false);
    assert_eq!(
        chats,
        (0..ROUNDS).map(|i| format!("chat-{i}")).collect::<Vec<_>>(),
        "Node's chats"
    );
    assert_eq!(
        assets,
        (0..ROUNDS).map(|i| format!("img-{i}")).collect::<Vec<_>>(),
        "Rust's images"
    );
    // Whoever wrote last, the bytes are atomicJson's.
    let reparsed = js_json::parse(&text).unwrap();
    assert_eq!(js_json::stringify_pretty(&reparsed).unwrap(), text);
}

/// The control: Node saving its cached view as before M4 drops what Rust wrote, so the test above
/// would catch a merge or lock that stopped working.
#[test]
fn without_the_merge_nodes_saves_drop_rusts_images() {
    let Some(core) = core() else { return };
    // (Node's unlocked writes also race Rust's read-modify-write, so chats may be lost too.)
    let (_chats, assets, _) = race(&core, true);
    // Its view never had img-0, and it saved after img-0 was written: img-0 is gone for good.
    assert!(!assets.contains(&"img-0".to_string()), "{assets:?}");
}
