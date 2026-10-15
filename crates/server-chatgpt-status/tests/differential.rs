//! Node differential (sbstndalton/noevia#1373): every case tests/oracle.cjs answered with core's
//! own route, secret store, feature flags and schema is rebuilt here (same synthetic keys and
//! plaintexts; this side seals with its own nonce) and must give Node's exact status and bytes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;
use support::*;

#[test]
fn every_oracle_case_matches_node_byte_for_byte() {
    let doc = fixture();
    let owner = doc["owner"].as_str().unwrap();
    let foreign = doc["foreign"].as_str().unwrap();
    let key = |name: &str| hex(doc["keys"][name].as_str().unwrap());
    let ddl = doc["chatgptDdl"].as_str().unwrap();
    let cases = doc["cases"].as_array().unwrap();
    assert!(cases.len() >= 40, "the oracle lost cases");
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let (d, db) = data_dir(Some(ddl));
        write_key(d.path(), "secrets.key", &key("current"));
        let previous_hex = doc["keys"]["previous"].as_str().unwrap();
        let previous_env = match c["previous"].as_str() {
            Some("file") => {
                write_key(d.path(), "secrets.key.previous", &key("previous"));
                None
            }
            Some("env") => Some(previous_hex),
            _ => None,
        };
        if let Some(v) = c["setting"].as_str() {
            set_flag(&db, v);
        }
        for row in c["rows"].as_array().unwrap() {
            let user = row["user"].as_str().unwrap();
            let data = &row["data"];
            let data_enc = if let Some(raw) = data["raw"].as_str() {
                raw.to_string()
            } else {
                let plain = match data["seal"].as_str() {
                    Some(text) => text.as_bytes().to_vec(),
                    None => hex(data["seal_hex"].as_str().unwrap()),
                };
                let aad = match data["aad"].as_str() {
                    Some("owner") => Some(user),
                    Some("A") => Some(owner),
                    Some("B") => Some(foreign),
                    _ => None,
                };
                seal(&key(data["key"].as_str().unwrap()), &plain, aad)
            };
            put_row(&db, user, row["state"].as_str().unwrap(), &data_enc);
        }
        let env = server_routing_mode::feature_env(c["flagEnv"].as_str()).unwrap();
        let keys = keys(d.path(), previous_env);
        let before = dump(&db);
        let store = server_store::Store::open(d.path()).unwrap();
        let got = server_chatgpt_status::answer(&store, owner, env, &|| Some(keys.clone()));
        assert_eq!(
            got.status,
            c["expected"]["status"].as_u64().unwrap() as u16,
            "{name}"
        );
        assert_eq!(got.body, c["expected"]["body"].as_str().unwrap(), "{name}");
        assert!(!got.body.contains("LEAK"), "{name}");
        assert_eq!(dump(&db), before, "{name}: the status read wrote");
    }
}
