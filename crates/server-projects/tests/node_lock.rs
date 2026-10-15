//! M4: Node (core server/rust-projects.cjs, the merges it saves with) and Rust ([`store::update`],
//! [`store::update_free`]) write the same projects.json and free-chats.json at the same time, as the chat routes (Node) and the image routes
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
use std::io::Write;
use std::process::{Command, Stdio};

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
        ("LOCK_WAIT_MS", lock::WAIT_MS.to_string()),
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
      if (!blind) {
        // As the config route does: a save, then (pruneDocuments -> uploads.prune) the absent
        // list becomes [] in memory only, before the front's first image. Not a change of Node's.
        pf.save(projects);
        const p = projects.find((x) => x.id === 'p1');
        p.assets = (p.assets || []).filter(() => true);
      }
      // An independent owner retains its original base and view throughout the race.
      const finalPf = createProjectsFile(file, { atomicJson, warn: (m) => { throw new Error(m); } });
      const finalStale = finalPf.load();
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
        p.assets = (p.assets || []).filter(() => true);
      }
      finalStale.find((p) => p.id === 'p1').chats =
        Array.from({ length: Number(rounds) }, (_, i) => ({ id: 'chat-' + i }));
      // Both modes finish with the same truly stale input, after Rust's final write.
      const done = fs.readFileSync(0, 'utf8');
      if (done !== 'rust-done') throw new Error('missing Rust completion signal');
      if (!blind) {
        // Check the actual concurrent result before the final stale input could repair it.
        const assert = require('node:assert/strict');
        const actual = JSON.parse(fs.readFileSync(file, 'utf8')).projects.find((p) => p.id === 'p1');
        assert.deepEqual(actual.chats.map((c) => c.id),
          Array.from({ length: Number(rounds) }, (_, i) => 'chat-' + i));
        assert.deepEqual(actual.assets.map((a) => a.id),
          Array.from({ length: Number(rounds) }, (_, i) => 'img-' + i));
      }
      if (blind) atomicJson(file, { projects: finalStale });
      else finalPf.save(finalStale);
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
        .stdin(Stdio::piped())
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
    let mut input = node.stdin.take().unwrap();
    input.write_all(b"rust-done").unwrap();
    drop(input); // EOF releases Node's bounded-size read; no scheduling/timing assumption.
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
    assert!(dir.path().join("projects.json.lock").exists(), "long-lived");
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

// A stress writer may receive the production three-second Busy refusal: flock provides no
// fairness guarantee. Retry only Busy, under one overall test budget; never drop an increment.
fn stress_lock(file: &std::path::Path, deadline: std::time::Instant) -> lock::FileLock {
    stress_lock_with_busy(file, deadline, || {})
}

fn stress_lock_with_busy(
    file: &std::path::Path,
    deadline: std::time::Instant,
    mut busy: impl FnMut(),
) -> lock::FileLock {
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "stress lock budget exhausted"
        );
        match lock::acquire(file) {
            Ok(held) => return held,
            Err(lock::LockError::Busy) => busy(),
            Err(e) => panic!("stress lock error: {e:?}"),
        }
    }
}

#[test]
fn stress_retry_handles_busy_from_an_actual_holder() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("counter");
    let held = lock::acquire(&file).unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        stress_lock_with_busy(
            &file,
            std::time::Instant::now() + std::time::Duration::from_secs(10),
            || send.send(()).unwrap(),
        )
    });
    receive
        .recv_timeout(std::time::Duration::from_secs(6))
        .unwrap();
    drop(held);
    drop(worker.join().unwrap());
}

/// The two halves are one lock: Node processes (flock(1) on an inherited descriptor) and Rust
/// threads (std File::try_lock) incrementing one counter are never both inside and lose nothing.
#[test]
fn node_processes_and_rust_threads_share_one_flock() {
    let Some(core) = core() else { return };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("counter");
    std::fs::write(&file, "0").unwrap();
    let script = r#"
      const fs = require('fs'); const [core, f] = process.argv.slice(1);
      const rp = require(core + '/server/rust-projects.cjs');
      const deadline = Date.now() + 30000;
      for (let i = 0; i < 100;) {
        if (Date.now() >= deadline) throw new Error('stress lock budget exhausted');
        try { rp.withFileLock(f, () => {
          fs.mkdirSync(f + '.inside'); // EEXIST: two inside
          const n = Number(fs.readFileSync(f, 'utf8'));
          fs.writeFileSync(f, String(n + 1));
          fs.rmdirSync(f + '.inside');
        }); i++; } catch (e) { if (e.code !== 'PROJECTS_BUSY') throw e; }
      }
    "#;
    let nodes: Vec<_> = (0..2)
        .map(|_| {
            Command::new("node")
                .args(["-e", script, &core, file.to_str().unwrap()])
                .spawn()
                .expect("node on PATH")
        })
        .collect();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let threads: Vec<_> = (0..3)
        .map(|_| {
            let file = file.clone();
            std::thread::spawn(move || {
                let inside = {
                    let mut p = file.as_os_str().to_owned();
                    p.push(".inside");
                    std::path::PathBuf::from(p)
                };
                for _ in 0..100 {
                    let _held = stress_lock(&file, deadline);
                    std::fs::create_dir(&inside).expect("two inside");
                    let n: u64 = std::fs::read_to_string(&file).unwrap().parse().unwrap();
                    std::fs::write(&file, (n + 1).to_string()).unwrap();
                    std::fs::remove_dir(&inside).unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    for mut n in nodes {
        assert!(n.wait().unwrap().success(), "node side failed");
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "500");
}

/// A Node holder killed mid-hold: Rust gets the lock at once (no staleness wait), and while it
/// was alive Rust waited WAIT_MS and gave up Busy.
#[test]
fn a_killed_node_holder_releases_the_lock() {
    let Some(core) = core() else { return };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(store::PROJECTS_FILE);
    let script = r#"
      const [core, f] = process.argv.slice(1);
      require(core + '/server/rust-projects.cjs').withFileLock(f, () => {
        require('fs').writeFileSync(f + '.held', '');
        Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 60000);
      });
    "#;
    let mut node = Command::new("node")
        .args(["-e", script, &core, file.to_str().unwrap()])
        .spawn()
        .expect("node on PATH");
    let held = dir.path().join("projects.json.held");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !held.exists() {
        assert!(std::time::Instant::now() < until, "node never held");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let t0 = std::time::Instant::now();
    assert!(matches!(lock::acquire(&file), Err(lock::LockError::Busy)));
    let waited = i64::try_from(t0.elapsed().as_millis()).unwrap();
    assert!(waited >= lock::WAIT_MS - 100, "waited {waited} ms");
    node.kill().unwrap();
    node.wait().unwrap();
    let t0 = std::time::Instant::now();
    drop(lock::acquire(&file).unwrap());
    assert!(t0.elapsed() < std::time::Duration::from_secs(1));
}

/// The chat-list slice: Rust saves chat metas through the route (chats::post) while Node, through
/// its merge, adds metas and removes some of its own (a delete or a move here). `blind`: Node saves
/// its cached view whole, the control. Returns p1's chat ids afterwards.
fn chat_race(core: &str, blind: bool) -> Vec<String> {
    const USER: &str = "11111111-2222-4333-8444-555555555555";
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("users").join(USER);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(store::PROJECTS_FILE);
    std::fs::write(
        &file,
        "{\n  \"projects\": [\n    {\n      \"id\": \"p1\",\n      \"name\": \"Synthetic\",\n      \"modes\": [\n        \"chat\"\n      ],\n      \"chats\": [],\n      \"instructionSkills\": {}\n    }\n  ]\n}",
    )
    .unwrap();
    let script = r#"
      const [core, file, rounds, mode] = process.argv.slice(1);
      const fs = require('fs');
      const { createProjectsFile } = require(core + '/server/rust-projects.cjs');
      const { atomicJson } = require(core + '/server/workspace.cjs');
      const pf = createProjectsFile(file, { atomicJson, warn: (m) => { throw new Error(m); } });
      const projects = pf.load();
      fs.writeFileSync(file + '.loaded', '');
      // From Rust's first save on, this view is stale until refreshed.
      const until = Date.now() + 20000;
      while (!fs.readFileSync(file, 'utf8').includes('"rust-0"')) {
        if (Date.now() > until) throw new Error('Rust never wrote');
        Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 1);
      }
      for (let i = 0; i < Number(rounds); i++) {
        if (i % 2 && mode !== 'blind') pf.refresh(projects);
        const p = projects.find((x) => x.id === 'p1');
        // Every fifth round this request also removes its previous meta (a delete or move here).
        const kept = (p.chats || []).filter((c) => !(i % 5 === 4 && c.id === 'node-' + (i - 1)));
        p.chats = [...kept, { id: 'node-' + i, title: 'n', updatedAt: 100000 + i, preview: '', pinned: false, archived: false }];
        if (mode === 'blind') atomicJson(file, { projects });
        else pf.save(projects);
      }
    "#;
    let rounds = 120usize;
    let mut node = Command::new("node")
        .args([
            "-e",
            script,
            core,
            file.to_str().unwrap(),
            &rounds.to_string(),
            if blind { "blind" } else { "merge" },
        ])
        .spawn()
        .expect("node on PATH");
    let loaded = dir.join("projects.json.loaded");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !loaded.exists() {
        assert!(std::time::Instant::now() < until, "node never loaded");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    for i in 0..rounds {
        let ctx = server_projects::Ctx {
            data_dir: root.path(),
            user_id: USER,
            switch: RustProjects::from_env_value(Some("1")).unwrap(),
            now_ms: 1,
        };
        let body =
            format!("{{\"chats\":[{{\"id\":\"rust-{i}\",\"title\":\"r\",\"updatedAt\":{i}}}]}}");
        let r = server_projects::chats::post(ctx, "p1", body.as_bytes(), false, true);
        assert_eq!(r.status, 200, "{r:?}");
    }
    assert!(node.wait().unwrap().success(), "node side failed");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    v["projects"][0]["chats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn node_and_rust_saving_chat_lists_at_once_lose_nothing() {
    let Some(core) = core() else { return };
    let ids: std::collections::HashSet<String> = chat_race(&core, false).into_iter().collect();
    for i in 0..120 {
        assert!(ids.contains(&format!("rust-{i}")), "Rust's rust-{i} lost");
        let node_kept = !(i + 1 < 120 && (i + 1) % 5 == 4);
        assert_eq!(ids.contains(&format!("node-{i}")), node_kept, "node-{i}");
    }
    assert_eq!(
        ids.len(),
        120 + 120 - (0..119).filter(|i| (i + 1) % 5 == 4).count()
    );
}

/// The control: Node's pre-M4 whole-view save drops chat metas Rust saved meanwhile.
#[test]
fn without_the_merge_nodes_saves_drop_rusts_chat_metas() {
    let Some(core) = core() else { return };
    let ids = chat_race(&core, true);
    // Its view never had rust-0, and it saved after rust-0 was written: rust-0 is gone for good.
    assert!(!ids.contains(&"rust-0".to_string()), "{ids:?}");
}

/// sbstndalton/noevia#1410: Node moves chat `c1` out of project p1 into the free list (core
/// projects.cjs moveChat: two files) and stops after its first write, while a stale client's
/// chat-list save of p1, still listing c1, goes through Rust ([`server_projects::chats::post`]).
/// `old_order`: the control, the pre-fix sequence done by hand (projects.json first). Returns
/// whether c1 ends in p1 and whether it ends in the free list.
fn move_out_race(core: &str, old_order: bool) -> (bool, bool) {
    const USER: &str = "11111111-2222-4333-8444-555555555555";
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("users").join(USER);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(store::PROJECTS_FILE),
        "{\n  \"projects\": [\n    {\n      \"id\": \"p1\",\n      \"name\": \"Synthetic\",\n      \"modes\": [\n        \"chat\"\n      ],\n      \"chats\": [\n        {\n          \"id\": \"c1\",\n          \"title\": \"Moving\",\n          \"updatedAt\": 5\n        }\n      ],\n      \"instructionSkills\": {}\n    }\n  ]\n}",
    )
    .unwrap();
    let script = r#"
      const [core, root, user, mode] = process.argv.slice(1);
      const fs = require('fs'), path = require('path');
      const { createWorkspaceStore } = require(core + '/server/workspace.cjs');
      const { createProjectStore } = require(core + '/server/projects.cjs');
      const ws = createWorkspaceStore(root, { id: 'synthetic', label: 'Synthetic' }, null, { rustProjects: true }).get(user);
      // Stop after the first file the move writes until Rust has saved (deterministic interleaving).
      let first = true;
      const pause = () => {
        if (!first) return; first = false;
        fs.writeFileSync(path.join(ws.dir, 'move.paused'), '');
        const until = Date.now() + 20000;
        while (!fs.existsSync(path.join(ws.dir, 'move.go'))) {
          if (Date.now() > until) throw new Error('Rust never saved');
          Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 2);
        }
      };
      const saveProjects = ws.saveProjects, saveFreeChats = ws.saveFreeChats;
      ws.saveProjects = function () { saveProjects.call(this); pause(); };
      ws.saveFreeChats = function () { saveFreeChats.call(this); pause(); };
      if (mode === 'old') {
        // The pre-#1410 order: the project without c1 first, then the free list with it.
        const p = ws.projects.find((x) => x.id === 'p1');
        const meta = p.chats.find((c) => c.id === 'c1');
        p.chats = p.chats.filter((c) => c.id !== 'c1');
        ws.saveProjects();
        ws.freeChats.push(meta);
        ws.saveFreeChats();
      } else {
        const store = createProjectStore({ fs, path, currentWorkspace: () => ws, PROJECTS: ws.projects, FREE_CHATS: ws.freeChats });
        const r = store.moveChat('c1', null);
        if (r.status !== 200) throw new Error('move: ' + JSON.stringify(r));
      }
    "#;
    let mut node = Command::new("node")
        .args([
            "-e",
            script,
            core,
            root.path().to_str().unwrap(),
            USER,
            if old_order { "old" } else { "move" },
        ])
        .spawn()
        .expect("node on PATH");
    let paused = dir.join("move.paused");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !paused.exists() {
        assert!(std::time::Instant::now() < until, "node never paused");
        assert!(
            node.try_wait().unwrap().is_none(),
            "node exited before its first write"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let ctx = server_projects::Ctx {
        data_dir: root.path(),
        user_id: USER,
        switch: RustProjects::from_env_value(Some("1")).unwrap(),
        now_ms: 1,
    };
    // The stale client still lists c1 in p1 (with a newer title), next to a new chat.
    let body = br#"{"chats":[{"id":"c1","title":"Stale","updatedAt":9},{"id":"c2","title":"New","updatedAt":8}]}"#;
    let r = server_projects::chats::post(ctx, "p1", body, false, true);
    assert_eq!(r.status, 200, "{r:?}");
    std::fs::write(dir.join("move.go"), "").unwrap();
    assert!(node.wait().unwrap().success(), "node side failed");
    let projects: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join(store::PROJECTS_FILE)).unwrap())
            .unwrap();
    let free: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("free-chats.json")).unwrap())
            .unwrap();
    let has = |list: &serde_json::Value, id: &str| {
        list.as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"].as_str() == Some(id))
    };
    let p1 = &projects["projects"][0]["chats"];
    assert!(has(p1, "c2"), "Rust's new chat kept: {p1}");
    (has(p1, "c1"), has(&free, "c1"))
}

#[test]
fn a_chat_moved_out_of_a_project_during_a_rust_save_ends_in_one_list() {
    let Some(core) = core() else { return };
    assert_eq!(move_out_race(&core, false), (false, true));
}

/// The control: in the pre-#1410 order the same interleaving leaves c1 in both lists.
#[test]
fn moving_out_project_first_duplicates_the_chat() {
    let Some(core) = core() else { return };
    assert_eq!(move_out_race(&core, true), (true, true));
}

// ── free-chats.json (the free-chat slice) ──────────────────────────────────

const FREE_USER: &str = "11111111-2222-4333-8444-555555555555";

fn free_ctx(root: &std::path::Path) -> server_projects::Ctx<'_> {
    server_projects::Ctx {
        data_dir: root,
        user_id: FREE_USER,
        switch: RustProjects::from_env_value(Some("1")).unwrap(),
        now_ms: 1,
    }
}

fn free_ids(dir: &std::path::Path) -> Vec<String> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("free-chats.json")).unwrap())
            .unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect()
}

/// Rust saves free-chat metas through the route (free_chats::post) while Node, through its
/// coordinated free list (core rust-projects.cjs createFreeChatsFile), adds metas and removes some
/// of its own (a delete or a move here). `blind`: Node saves its cached list whole, as before the
/// slice (the control). Returns the free list's ids afterwards.
fn free_race(core: &str, blind: bool) -> Vec<String> {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("users").join(FREE_USER);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("free-chats.json");
    std::fs::write(&file, "[]").unwrap();
    let script = r#"
      const [core, file, rounds, mode] = process.argv.slice(1);
      const fs = require('fs');
      const { createFreeChatsFile } = require(core + '/server/rust-projects.cjs');
      const { atomicJson } = require(core + '/server/workspace.cjs');
      const ff = createFreeChatsFile(file, { atomicJson, warn: (m) => { throw new Error(m); } });
      const ws = { freeChats: ff.load() };
      fs.writeFileSync(file + '.loaded', '');
      // From Rust's first save on, this view is stale until refreshed.
      const until = Date.now() + 20000;
      while (!fs.readFileSync(file, 'utf8').includes('"rust-0"')) {
        if (Date.now() > until) throw new Error('Rust never wrote');
        Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 1);
      }
      for (let i = 0; i < Number(rounds); i++) {
        if (i % 2 && mode !== 'blind') ff.refresh(ws);
        // Every fifth round this request also removes its previous meta (a delete or move here).
        const kept = ws.freeChats.filter((c) => !(i % 5 === 4 && c.id === 'node-' + (i - 1)));
        ws.freeChats = [{ id: 'node-' + i, title: 'n', updatedAt: 100000 + i, preview: '', pinned: false, archived: false }, ...kept];
        if (mode === 'blind') atomicJson(file, ws.freeChats);
        else ff.save(ws);
      }
    "#;
    let rounds = 120usize;
    let mut node = Command::new("node")
        .args([
            "-e",
            script,
            core,
            file.to_str().unwrap(),
            &rounds.to_string(),
            if blind { "blind" } else { "merge" },
        ])
        .spawn()
        .expect("node on PATH");
    let loaded = dir.join("free-chats.json.loaded");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !loaded.exists() {
        assert!(std::time::Instant::now() < until, "node never loaded");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    for i in 0..rounds {
        let body =
            format!("{{\"chats\":[{{\"id\":\"rust-{i}\",\"title\":\"r\",\"updatedAt\":{i}}}]}}");
        let r =
            server_projects::free_chats::post(free_ctx(root.path()), body.as_bytes(), false, true);
        assert_eq!(r.status, 200, "{r:?}");
    }
    assert!(node.wait().unwrap().success(), "node side failed");
    assert!(dir.join("free-chats.json.lock").exists(), "long-lived");
    free_ids(&dir)
}

#[test]
fn node_and_rust_saving_the_free_list_at_once_lose_nothing() {
    let Some(core) = core() else { return };
    let ids: std::collections::HashSet<String> = free_race(&core, false).into_iter().collect();
    for i in 0..120 {
        assert!(ids.contains(&format!("rust-{i}")), "Rust's rust-{i} lost");
        let node_kept = !(i + 1 < 120 && (i + 1) % 5 == 4);
        assert_eq!(ids.contains(&format!("node-{i}")), node_kept, "node-{i}");
    }
    assert_eq!(
        ids.len(),
        120 + 120 - (0..119).filter(|i| (i + 1) % 5 == 4).count()
    );
}

/// The control: Node's whole-list save (before the slice) drops metas Rust saved meanwhile.
#[test]
fn without_the_merge_nodes_free_saves_drop_rusts_metas() {
    let Some(core) = core() else { return };
    let ids = free_race(&core, true);
    assert!(!ids.contains(&"rust-0".to_string()), "{ids:?}");
}

/// Node (core projects.cjs `moveChat` into project p1, or `deleteFreeChat`, through the workspace
/// store with the slice on) stops at the first file it writes until a stale client's free-list
/// save, still listing the chat `f1` and adding `n1`, has gone through Rust. `variant`:
///   "move"            moveChat as core does it (projects.json first, then free-chats.json)
///   "move-free-first" the control: the free list written first by hand
///   "delete"          deleteFreeChat (tombstone, then the free list)
///   "delete-blind"    the control: the same delete with Node's whole-list save
/// Returns (f1 in the free list, f1 in p1, n1 in the free list).
fn free_interleaving(core: &str, variant: &str) -> (bool, bool, bool) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("users").join(FREE_USER);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(store::PROJECTS_FILE),
        "{\n  \"projects\": [\n    {\n      \"id\": \"p1\",\n      \"name\": \"Synthetic\",\n      \"modes\": [\n        \"chat\"\n      ],\n      \"chats\": [],\n      \"instructionSkills\": {}\n    }\n  ]\n}",
    )
    .unwrap();
    std::fs::write(
        dir.join("free-chats.json"),
        "[\n  {\n    \"id\": \"f1\",\n    \"title\": \"Moving\",\n    \"updatedAt\": 5\n  }\n]",
    )
    .unwrap();
    let script = r#"
      const [core, root, user, variant] = process.argv.slice(1);
      const fs = require('fs'), path = require('path');
      const { createWorkspaceStore, atomicJson } = require(core + '/server/workspace.cjs');
      const { createProjectStore } = require(core + '/server/projects.cjs');
      const ws = createWorkspaceStore(root, { id: 'synthetic', label: 'Synthetic' }, null, { rustProjects: true, rustFreeChats: true }).get(user);
      if (!ws.freeChatsFile) throw new Error('the free-chat slice is not on');
      // Stop at the first file this request writes until Rust has saved (deterministic interleaving):
      // a move right after its first write; a delete after its tombstone, before its list write.
      let first = true;
      const pause = () => {
        if (!first) return; first = false;
        fs.writeFileSync(path.join(ws.dir, 'node.paused'), '');
        const until = Date.now() + 20000;
        while (!fs.existsSync(path.join(ws.dir, 'node.go'))) {
          if (Date.now() > until) throw new Error('Rust never saved');
          Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 2);
        }
      };
      const saveProjects = ws.saveProjects, saveFreeChats = ws.saveFreeChats;
      ws.saveProjects = function () { saveProjects.call(this); pause(); };
      const deleting = variant.startsWith('delete');
      ws.saveFreeChats = function () {
        if (deleting) pause();
        if (variant === 'delete-blind') atomicJson(path.join(ws.dir, 'free-chats.json'), this.freeChats);
        else saveFreeChats.call(this);
        pause();
      };
      const proxy = (field) => new Proxy([], {
        get(_t, prop) { const v = ws[field][prop]; return typeof v === 'function' ? v.bind(ws[field]) : v; },
        set(_t, prop, v) { ws[field][prop] = v; return true; },
        ownKeys() { return Reflect.ownKeys(ws[field]); },
        getOwnPropertyDescriptor(_t, prop) { return Reflect.getOwnPropertyDescriptor(ws[field], prop); },
      });
      const store = createProjectStore({ fs, path, currentWorkspace: () => ws, PROJECTS: proxy('projects'), FREE_CHATS: proxy('freeChats') });
      if (variant === 'move') {
        const r = store.moveChat('f1', 'p1');
        if (r.status !== 200) throw new Error('move: ' + JSON.stringify(r));
      } else if (variant === 'move-free-first') {
        const meta = ws.freeChats.find((c) => c.id === 'f1');
        ws.freeChats = ws.freeChats.filter((c) => c.id !== 'f1');
        ws.saveFreeChats();
        ws.projects[0].chats = [meta];
        ws.saveProjects();
      } else {
        if (!store.deleteFreeChat('f1')) throw new Error('delete refused');
      }
    "#;
    let mut node = Command::new("node")
        .args([
            "-e",
            script,
            core,
            root.path().to_str().unwrap(),
            FREE_USER,
            variant,
        ])
        .spawn()
        .expect("node on PATH");
    let paused = dir.join("node.paused");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !paused.exists() {
        assert!(std::time::Instant::now() < until, "node never paused");
        assert!(
            node.try_wait().unwrap().is_none(),
            "node exited before its first write"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    // The stale client still lists f1 (with a newer title), next to a new chat.
    let body = br#"{"chats":[{"id":"f1","title":"Stale","updatedAt":9},{"id":"n1","title":"New","updatedAt":8}]}"#;
    let r = server_projects::free_chats::post(free_ctx(root.path()), body, false, true);
    assert_eq!(r.status, 200, "{r:?}");
    std::fs::write(dir.join("node.go"), "").unwrap();
    assert!(node.wait().unwrap().success(), "node side failed");
    let projects: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join(store::PROJECTS_FILE)).unwrap())
            .unwrap();
    let in_p1 = projects["projects"][0]["chats"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["id"].as_str() == Some("f1"));
    let free = free_ids(&dir);
    (
        free.contains(&"f1".to_string()),
        in_p1,
        free.contains(&"n1".to_string()),
    )
}

#[test]
fn a_chat_moved_into_a_project_during_a_rust_free_save_ends_in_one_list() {
    let Some(core) = core() else { return };
    assert_eq!(free_interleaving(&core, "move"), (false, true, true));
}

/// The control: written free list first, the same interleaving leaves f1 in both lists, which is
/// why moveChat writes the project first when a chat goes into one.
#[test]
fn moving_in_free_list_first_duplicates_the_chat() {
    let Some(core) = core() else { return };
    assert_eq!(
        free_interleaving(&core, "move-free-first"),
        (true, true, true)
    );
}

#[test]
fn a_free_chat_deleted_during_a_rust_free_save_stays_deleted_and_the_save_stands() {
    let Some(core) = core() else { return };
    assert_eq!(free_interleaving(&core, "delete"), (false, false, true));
}

/// The control: the same delete saved whole (before the slice) drops the meta Rust saved.
#[test]
fn deleting_with_a_whole_list_save_drops_rusts_meta() {
    let Some(core) = core() else { return };
    assert_eq!(
        free_interleaving(&core, "delete-blind"),
        (false, false, false)
    );
}

/// sbstndalton/noevia#1426: a chat turn sets the chat's routing flags (core projects.cjs
/// `setChatFlags`) after its route choice, which can wait long; the workspace was read when the
/// turn began. Rust saves the chat retitled in between (`list`: "free" for the free list, "project"
/// for p1's chat list). `stale`: the control, the flags set on the turn's own view as index.cjs did
/// before. Returns the chat's title and forceLocal flag on disk afterwards.
fn flags_interleaving(core: &str, list: &str, stale: bool) -> (String, Option<bool>) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("users").join(FREE_USER);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(store::PROJECTS_FILE),
        "{\n  \"projects\": [\n    {\n      \"id\": \"p1\",\n      \"name\": \"Synthetic\",\n      \"modes\": [\n        \"chat\"\n      ],\n      \"chats\": [\n        {\n          \"id\": \"c1\",\n          \"title\": \"Old\",\n          \"updatedAt\": 5\n        }\n      ],\n      \"instructionSkills\": {}\n    }\n  ]\n}",
    )
    .unwrap();
    std::fs::write(
        dir.join("free-chats.json"),
        "[\n  {\n    \"id\": \"c1\",\n    \"title\": \"Old\",\n    \"updatedAt\": 5\n  }\n]",
    )
    .unwrap();
    // The same id in both lists keeps the two variants alike; each touches only its own list.
    if list == "project" {
        std::fs::write(dir.join("free-chats.json"), "[]").unwrap();
    } else {
        std::fs::write(
            dir.join(store::PROJECTS_FILE),
            "{\n  \"projects\": [\n    {\n      \"id\": \"p1\",\n      \"name\": \"Synthetic\",\n      \"modes\": [\n        \"chat\"\n      ],\n      \"chats\": [],\n      \"instructionSkills\": {}\n    }\n  ]\n}",
        )
        .unwrap();
    }
    let script = r#"
      const [core, root, user, list, stale] = process.argv.slice(1);
      const fs = require('fs'), path = require('path');
      const { createWorkspaceStore } = require(core + '/server/workspace.cjs');
      const { createProjectStore } = require(core + '/server/projects.cjs');
      const ws = createWorkspaceStore(root, { id: 'synthetic', label: 'Synthetic' }, null, { rustProjects: true, rustFreeChats: true }).get(user);
      if (!ws.freeChatsFile || !ws.projectsFile) throw new Error('the slices are not on');
      const proxy = (field) => new Proxy([], {
        get(_t, prop) { const v = ws[field][prop]; return typeof v === 'function' ? v.bind(ws[field]) : v; },
        set(_t, prop, v) { ws[field][prop] = v; return true; },
        ownKeys() { return Reflect.ownKeys(ws[field]); },
        getOwnPropertyDescriptor(_t, prop) { return Reflect.getOwnPropertyDescriptor(ws[field], prop); },
      });
      const PROJECTS = proxy('projects'), FREE_CHATS = proxy('freeChats');
      const store = createProjectStore({ fs, path, currentWorkspace: () => ws, PROJECTS, FREE_CHATS });
      const projectId = list === 'project' ? 'p1' : null;
      // The turn waits for its route choice; Rust saves meanwhile.
      fs.writeFileSync(path.join(ws.dir, 'node.paused'), '');
      const until = Date.now() + 20000;
      while (!fs.existsSync(path.join(ws.dir, 'node.go'))) {
        if (Date.now() > until) throw new Error('Rust never saved');
        Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 2);
      }
      const patch = { forceLocal: true };
      if (stale === '1') {
        // index.cjs before #1426: the flags set on the turn's own view, saved.
        const chats = projectId ? store.getProject(projectId)?.chats : FREE_CHATS;
        const meta = chats.find((c) => c && c.id === 'c1');
        if (!meta) throw new Error('no meta');
        Object.assign(meta, patch);
        if (projectId) store.saveProjects(PROJECTS); else store.saveFreeChats(FREE_CHATS);
      } else if (!store.setChatFlags(projectId, 'c1', patch)) {
        throw new Error('setChatFlags found no meta');
      }
    "#;
    let mut node = Command::new("node")
        .args([
            "-e",
            script,
            core,
            root.path().to_str().unwrap(),
            FREE_USER,
            list,
            if stale { "1" } else { "0" },
        ])
        .spawn()
        .expect("node on PATH");
    let paused = dir.join("node.paused");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !paused.exists() {
        assert!(std::time::Instant::now() < until, "node never paused");
        assert!(node.try_wait().unwrap().is_none(), "node exited early");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let body = br#"{"chats":[{"id":"c1","title":"Retitled","updatedAt":9}]}"#;
    let r = if list == "project" {
        server_projects::chats::post(free_ctx(root.path()), "p1", body, false, true)
    } else {
        server_projects::free_chats::post(free_ctx(root.path()), body, false, true)
    };
    assert_eq!(r.status, 200, "{r:?}");
    std::fs::write(dir.join("node.go"), "").unwrap();
    assert!(node.wait().unwrap().success(), "node side failed");
    let meta = if list == "project" {
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(store::PROJECTS_FILE)).unwrap())
                .unwrap();
        v["projects"][0]["chats"][0].clone()
    } else {
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("free-chats.json")).unwrap())
                .unwrap();
        v[0].clone()
    };
    assert_eq!(meta["id"], "c1", "{meta}");
    (
        meta["title"].as_str().unwrap().to_string(),
        meta["forceLocal"].as_bool(),
    )
}

#[test]
fn routing_flags_set_after_a_rust_free_save_keep_its_title() {
    let Some(core) = core() else { return };
    assert_eq!(
        flags_interleaving(&core, "free", false),
        ("Retitled".to_string(), Some(true))
    );
}

#[test]
fn routing_flags_set_after_a_rust_chat_list_save_keep_its_title() {
    let Some(core) = core() else { return };
    assert_eq!(
        flags_interleaving(&core, "project", false),
        ("Retitled".to_string(), Some(true))
    );
}

/// Formerly the control (flags set on the turn's stale view put the old title back). Since
/// sbstndalton/noevia#1801 the save's three-way merge takes a chat meta both writers changed per
/// field (core rust-projects.cjs mergeIdList, both lists), so even a save from the stale view keeps
/// Rust's title; core main shows this with or without #1806. setChatFlags re-reading first (#1426)
/// stays covered by the test above.
#[test]
fn routing_flags_on_a_stale_view_keep_rusts_title_since_1801() {
    let Some(core) = core() else { return };
    assert_eq!(
        flags_interleaving(&core, "free", true),
        ("Retitled".to_string(), Some(true))
    );
    assert_eq!(
        flags_interleaving(&core, "project", true),
        ("Retitled".to_string(), Some(true))
    );
}
