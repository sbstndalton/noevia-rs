//! Differential replay of the frozen Node oracle (noevia-core tests/reference/code-egress-control.cjs,
//! scenarios in tests/reference/code-egress-issuer-fixtures.cjs): the native issuer sends byte-identical
//! requests and reaches identical results, including the key rotation after signed refusals.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod support;
use egress_issuer::files::{IssuerFiles, KeyKeeper};
use egress_issuer::{Client, ClientConfig, Handle};
use serde_json::Value;
use std::collections::HashMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::Arc;
use support::{Exchange, FixtureClock, Script, ScriptedTransport};

const FIXTURE: &str = include_str!("fixtures/oracle.json");

fn exchanges(step: &Value) -> Vec<Exchange> {
    step["exchanges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let pairs = |v: &Value| -> Vec<(String, String)> {
                v.as_array()
                    .unwrap()
                    .iter()
                    .map(|p| {
                        (
                            p[0].as_str().unwrap().to_string(),
                            p[1].as_str().unwrap().to_string(),
                        )
                    })
                    .collect()
            };
            let response = &e["response"];
            let response = if response.get("throw").is_some() {
                Err(())
            } else {
                // The oracle recorded rawHeaders as a flat [name, value, ...] list.
                let flat: Vec<String> = response["rawHeaders"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_string())
                    .collect();
                let raw = flat
                    .chunks(2)
                    .map(|c| (c[0].clone(), c[1].clone()))
                    .collect();
                Ok((
                    response["status"].as_u64().unwrap() as u16,
                    raw,
                    response["body"].as_str().unwrap().to_string(),
                ))
            };
            Exchange {
                route: e["route"].as_str().unwrap().into(),
                headers: pairs(&e["headers"]),
                body: e["body"].as_str().unwrap().into(),
                response,
            }
        })
        .collect()
}

fn reason<T>(r: Result<T, egress_issuer::ControlError>) -> Value {
    match r {
        Ok(_) => Value::Null,
        Err(e) => Value::String(e.0.as_str().to_string()),
    }
}

#[tokio::test]
async fn native_issuer_replays_every_oracle_scenario() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let master: [u8; 32] = [7; 32];
    assert_eq!(fixture["masterHex"].as_str().unwrap(), "07".repeat(32));
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    for scenario in fixture["scenarios"].as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap();
        let clock = FixtureClock::new(
            scenario["wall0"].as_u64().unwrap(),
            scenario["mono0"].as_f64().unwrap(),
        );
        let script = Arc::new(Script::default());
        let dir = tempfile::tempdir().unwrap();
        let (key, keeper, persisted) = if scenario["keyed"].as_bool().unwrap() {
            if gid == 0 {
                eprintln!("skipping {name}: needs a non-root group for the 0440 key");
                continue;
            }
            let root = std::fs::canonicalize(dir.path()).unwrap();
            let keydir = root.join("key");
            std::fs::create_dir(&keydir).unwrap();
            std::os::unix::fs::chown(&keydir, Some(uid), Some(gid)).unwrap();
            std::fs::set_permissions(&keydir, std::fs::Permissions::from_mode(0o750)).unwrap();
            let files = IssuerFiles {
                key_file: keydir.join("current.key"),
                uid,
                gid,
                state_file: root.join("issuer.json"),
            };
            let c = Arc::clone(&clock);
            let (keeper, _how) = KeyKeeper::open(
                files,
                Arc::new(move || Ok(master)),
                Arc::new(move |b: &mut [u8]| c.draw(b)),
            )
            .unwrap();
            let key = keeper.key();
            assert_eq!(
                egress_issuer::wire::hex(&key),
                scenario["initialKeyHex"].as_str().unwrap(),
                "{name}: first key"
            );
            let persisted = keeper.persisted_epoch();
            (key, Some(keeper), persisted)
        } else {
            ([9u8; 32], None, None)
        };
        let mut config = ClientConfig::new(
            scenario["endpoint"].as_str().unwrap(),
            scenario["ttlMs"].as_u64().unwrap(),
            scenario["lifetimeMs"].as_u64().unwrap(),
        );
        config.auto_renew = false;
        let client = Client::new(
            config,
            key,
            keeper,
            persisted,
            clock.clone(),
            Arc::new(ScriptedTransport(Arc::clone(&script))),
            Arc::new(|_, _| {}),
        )
        .unwrap();
        let mut handles: HashMap<String, Handle> = HashMap::new();
        for (index, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
            let here = format!("{name} / step {index} ({})", step["op"]);
            script.load(exchanges(step));
            let expected = &step["result"];
            let as_name = step["as"].as_str().map(str::to_string);
            let budget = step["budget"].as_f64();
            match step["op"].as_str().unwrap() {
                "set" => {
                    if let Some(w) = step["wall"].as_u64() {
                        *clock.wall.lock().unwrap() = w;
                    }
                    if let Some(m) = step["mono"].as_f64() {
                        *clock.mono.lock().unwrap() = m;
                    }
                }
                "mode" => {}
                "ready" | "renew" => {
                    let got = if step["op"] == "ready" {
                        client.ready().await
                    } else {
                        client.renew().await
                    };
                    match (expected.get("ok"), expected.get("error")) {
                        (Some(_), _) => assert!(got.is_ok(), "{here}: {got:?}"),
                        (_, Some(e)) => assert_eq!(reason(got), *e, "{here}"),
                        _ => unreachable!(),
                    }
                }
                "grant" => {
                    let domains: Vec<String> = step["domains"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|d| d.as_str().unwrap().to_string())
                        .collect();
                    let got = client
                        .grant(step["taskId"].as_str().unwrap(), &domains)
                        .await;
                    match (expected.get("ok"), expected.get("error")) {
                        (Some(ok), _) => {
                            let h = got.unwrap_or_else(|e| panic!("{here}: {e:?}"));
                            assert_eq!(h.task_id, ok["taskId"].as_str().unwrap(), "{here}");
                            assert_eq!(
                                serde_json::to_value(&h.domains).unwrap(),
                                ok["domains"],
                                "{here}"
                            );
                            assert_eq!(
                                h.token,
                                ok["token"].as_str().unwrap(),
                                "{here}: token bytes"
                            );
                            handles.insert(as_name.unwrap(), h);
                        }
                        (_, Some(e)) => assert_eq!(reason(got), *e, "{here}"),
                        _ => unreachable!(),
                    }
                }
                "revoke" | "activity" => {
                    let id = &handles[as_name.as_deref().unwrap()].id;
                    if step["op"] == "revoke" {
                        let got = client.revoke(id, budget).await;
                        let ok = got.unwrap_or_else(|e| panic!("{here}: {e:?}"));
                        let want = &expected["ok"];
                        assert_eq!(ok.confirmed, want["confirmed"].as_bool().unwrap(), "{here}");
                        assert_eq!(ok.report.to_json(), want["report"], "{here}");
                    } else {
                        let got = client.activity(id, budget).await;
                        match (expected.get("ok"), expected.get("error")) {
                            (Some(ok), _) => assert_eq!(got.unwrap().to_json(), *ok, "{here}"),
                            (_, Some(e)) => assert_eq!(reason(got), *e, "{here}"),
                            _ => unreachable!(),
                        }
                    }
                }
                "forget" => {
                    let id = &handles[as_name.as_deref().unwrap()].id;
                    let got = client
                        .forget(id, step["snapshotId"].as_str().unwrap())
                        .await;
                    match (expected.get("ok"), expected.get("error")) {
                        (Some(ok), _) => {
                            assert_eq!(got.unwrap(), ok["confirmed"].as_bool().unwrap(), "{here}")
                        }
                        (_, Some(e)) => assert_eq!(reason(got), *e, "{here}"),
                        _ => unreachable!(),
                    }
                }
                "available" => assert_eq!(
                    client.available(),
                    expected["ok"].as_bool().unwrap(),
                    "{here}"
                ),
                "keyFile" => {
                    let want = &expected["ok"];
                    let path = dir.path().join("key").join("current.key");
                    let text = std::fs::read_to_string(&path).unwrap();
                    assert_eq!(
                        text.trim(),
                        want["hex"].as_str().unwrap(),
                        "{here}: rotated key bytes"
                    );
                    let meta = std::fs::metadata(&path).unwrap();
                    assert_eq!(
                        format!("{:o}", meta.mode() & 0o7777),
                        want["mode"].as_str().unwrap(),
                        "{here}"
                    );
                }
                other => panic!("unknown op {other}"),
            }
            let mismatches = script.mismatches.lock().unwrap().clone();
            assert!(mismatches.is_empty(), "{here}: {}", mismatches.join("\n"));
            assert!(
                script.drained(),
                "{here}: the oracle sent a request the native issuer did not"
            );
        }
    }
}
