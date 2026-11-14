//! The launch plan: ports, the per-start secrets and what each child is told. Pure, so the
//! environment each process receives is tested without starting anything.

use crate::env::Env;
use crate::pairing::{Paired, Refusal};

/// One child: its program, arguments and the values set on top of the scrubbed environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSpec {
    pub program: String,
    pub args: Vec<String>,
    pub set: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub node: ChildSpec,
    pub front: ChildSpec,
    pub legacy_port: u32,
    pub store_port: u32,
    /// Notes printed before the launch (stderr).
    pub warnings: Vec<String>,
}

/// `UI_PORT` as the script reads it: digits only, 1..=64535 so UI_PORT+1000 fits. Leading zeros
/// read as decimal (the script's `$((port + 1000))` read them as octal; noevia issue filed).
pub fn ui_port(env: &Env) -> Result<(String, u32), Refusal> {
    let raw = env.get_or("UI_PORT", "8021").to_owned();
    let bad = || {
        Refusal(format!(
            "web-supervisor: UI_PORT '{raw}' leaves no room for the legacy port UI_PORT+1000"
        ))
    };
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let digits = raw.trim_start_matches('0');
    let port = if digits.len() > 6 {
        u32::MAX
    } else {
        digits.parse::<u32>().unwrap_or(0)
    };
    if !(1..=64535).contains(&port) {
        return Err(bad());
    }
    Ok((raw, port))
}

/// Lower-case hex of the bytes (`od -An -tx1 | tr -d ' \n'`).
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Builds both children. `store_token` is 32 random bytes and `start_id` 16, made for this start
/// only and never taken from the environment.
pub fn plan(
    env: &Env,
    bin: &str,
    paired: &Paired,
    store_token: &[u8],
    start_id: &[u8],
) -> Result<Launch, Refusal> {
    let (_, port) = ui_port(env)?;
    let legacy = port + 1000;
    // The project store (#1264): permanent, at UI_PORT+2000, with a secret for this start.
    let store_port = port + 2000;
    if store_port > 65535 {
        return Err(Refusal(format!(
            "web-supervisor: UI_PORT '{}' leaves no room for the project store port UI_PORT+2000",
            env.get_or("UI_PORT", "8021")
        )));
    }
    let token = hex(store_token);
    if token.len() != 64 {
        return Err(Refusal(
            "web-supervisor: could not make the project store secret".to_owned(),
        ));
    }
    // The start id (#1071): Node marks a running reply with it so the front can tell this start's
    // replies from ones a crash or restart cut off.
    let start = hex(start_id);
    if start.len() != 32 {
        return Err(Refusal(
            "web-supervisor: could not make the start id".to_owned(),
        ));
    }
    let mut warnings = Vec::new();
    // Only a rollback pair with the old listener needs Node on UI_HOST for DAV; the native pair
    // keeps the legacy listener loopback-only even with sharing configured.
    let mut node_host = "127.0.0.1".to_owned();
    let dav = env.get("COWORK_DAV_PORT");
    if paired.dav_listener_core != "v1" && !dav.is_empty() && dav != "0" {
        node_host = env.get_or("UI_HOST", "0.0.0.0").to_owned();
        warnings.push(format!(
            "web-supervisor: COWORK_DAV_PORT is set; Node's legacy port {legacy} listens on {node_host}, not loopback only"
        ));
    }
    let store = |set: &mut Vec<(String, String)>| {
        set.push((
            "NOEVIA_PROJECT_STORE_PORT".to_owned(),
            store_port.to_string(),
        ));
        set.push(("NOEVIA_PROJECT_STORE_TOKEN".to_owned(), token.clone()));
        set.push(("NOEVIA_START_ID".to_owned(), start.clone()));
    };
    let mut node_set = vec![
        ("UI_HOST".to_owned(), node_host),
        ("UI_PORT".to_owned(), legacy.to_string()),
        (
            "NOEVIA_FRONT_CLIENT_IP".to_owned(),
            if paired.client_ip_confirmed { "1" } else { "" }.to_owned(),
        ),
        (
            "NOEVIA_LEGACY_CLAIM_BIN".to_owned(),
            paired.legacy_claim_bin.clone().unwrap_or_default(),
        ),
    ];
    store(&mut node_set);
    let mut front_set = Vec::new();
    store(&mut front_set);
    front_set.push((
        "NOEVIA_LEGACY_UPSTREAM".to_owned(),
        format!("http://127.0.0.1:{legacy}"),
    ));
    Ok(Launch {
        node: ChildSpec {
            program: "node".to_owned(),
            args: vec!["server/index.cjs".to_owned()],
            set: node_set,
        },
        front: ChildSpec {
            program: bin.to_owned(),
            args: Vec::new(),
            set: front_set,
        },
        legacy_port: legacy,
        store_port,
        warnings,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn paired() -> Paired {
        Paired {
            client_ip_confirmed: true,
            legacy_claim_bin: None,
            dav_listener_core: "none".to_owned(),
        }
    }

    fn get<'a>(set: &'a [(String, String)], key: &str) -> Option<&'a str> {
        set.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn default_port_puts_node_on_loopback_9021_and_the_store_on_10021() {
        let env = Env::default();
        let l = plan(&env, "/bin/front", &paired(), &[0xab; 32], &[0x01; 16])
            .unwrap_or_else(|r| unreachable!("{r:?}"));
        assert_eq!(l.legacy_port, 9021);
        assert_eq!(l.store_port, 10021);
        assert_eq!(get(&l.node.set, "UI_HOST"), Some("127.0.0.1"));
        assert_eq!(get(&l.node.set, "UI_PORT"), Some("9021"));
        assert_eq!(get(&l.node.set, "NOEVIA_FRONT_CLIENT_IP"), Some("1"));
        assert_eq!(get(&l.node.set, "NOEVIA_LEGACY_CLAIM_BIN"), Some(""));
        assert_eq!(
            get(&l.front.set, "NOEVIA_LEGACY_UPSTREAM"),
            Some("http://127.0.0.1:9021")
        );
        assert_eq!(
            get(&l.front.set, "UI_PORT"),
            None,
            "the front reads its own UI_PORT"
        );
        assert_eq!(get(&l.front.set, "NOEVIA_FRONT_CLIENT_IP"), None);
        let token = get(&l.front.set, "NOEVIA_PROJECT_STORE_TOKEN");
        assert_eq!(token, get(&l.node.set, "NOEVIA_PROJECT_STORE_TOKEN"));
        assert_eq!(token.map(str::len), Some(64));
        assert_eq!(
            get(&l.node.set, "NOEVIA_START_ID"),
            Some("01010101010101010101010101010101")
        );
        assert!(l.warnings.is_empty());
        assert_eq!(l.node.args, vec!["server/index.cjs"]);
    }

    #[test]
    fn ports_outside_the_room_for_both_offsets_are_refused() {
        for bad in ["0", "64536", "abc", "-1", "80 21", "99999999999999999999"] {
            let env = Env::from_pairs([("UI_PORT", bad)]);
            let err = plan(&env, "/f", &paired(), &[0; 32], &[0; 16])
                .err()
                .map(|r| r.0)
                .unwrap_or_default();
            assert!(err.contains("no room for the legacy port"), "{bad}: {err}");
        }
        let env = Env::from_pairs([("UI_PORT", "64000")]);
        let err = plan(&env, "/f", &paired(), &[0; 32], &[0; 16])
            .err()
            .map(|r| r.0)
            .unwrap_or_default();
        assert!(err.contains("no room for the project store port"), "{err}");
        let env = Env::from_pairs([("UI_PORT", "63535")]);
        assert!(plan(&env, "/f", &paired(), &[0; 32], &[0; 16]).is_ok());
    }

    #[test]
    fn leading_zeros_are_decimal() {
        let env = Env::from_pairs([("UI_PORT", "0100")]);
        let l = plan(&env, "/f", &paired(), &[0; 32], &[0; 16])
            .unwrap_or_else(|r| unreachable!("{r:?}"));
        assert_eq!(l.legacy_port, 1100);
    }

    #[test]
    fn short_random_input_is_refused() {
        let env = Env::default();
        let err = plan(&env, "/f", &paired(), &[0; 31], &[0; 16])
            .err()
            .map(|r| r.0)
            .unwrap_or_default();
        assert!(err.contains("project store secret"));
        let err = plan(&env, "/f", &paired(), &[0; 32], &[0; 15])
            .err()
            .map(|r| r.0)
            .unwrap_or_default();
        assert!(err.contains("start id"));
    }

    #[test]
    fn dav_moves_node_to_ui_host_only_beside_an_old_listener() {
        let env = Env::from_pairs([("COWORK_DAV_PORT", "8040")]);
        let l = plan(&env, "/f", &paired(), &[0; 32], &[0; 16])
            .unwrap_or_else(|r| unreachable!("{r:?}"));
        assert_eq!(get(&l.node.set, "UI_HOST"), Some("0.0.0.0"));
        assert_eq!(l.warnings.len(), 1);
        assert!(l.warnings[0].contains("legacy port 9021 listens on 0.0.0.0, not loopback only"));
        let env = Env::from_pairs([("COWORK_DAV_PORT", "8040"), ("UI_HOST", "192.0.2.7")]);
        let mut native = paired();
        native.dav_listener_core = "v1".to_owned();
        let l =
            plan(&env, "/f", &native, &[0; 32], &[0; 16]).unwrap_or_else(|r| unreachable!("{r:?}"));
        assert_eq!(get(&l.node.set, "UI_HOST"), Some("127.0.0.1"));
        assert!(l.warnings.is_empty());
        let env = Env::from_pairs([("COWORK_DAV_PORT", "0")]);
        let l = plan(&env, "/f", &paired(), &[0; 32], &[0; 16])
            .unwrap_or_else(|r| unreachable!("{r:?}"));
        assert_eq!(get(&l.node.set, "UI_HOST"), Some("127.0.0.1"));
    }

    #[test]
    fn legacy_claim_and_unconfirmed_client_ip_reach_node() {
        let p = Paired {
            client_ip_confirmed: false,
            legacy_claim_bin: Some("/usr/local/bin/noevia-server".to_owned()),
            dav_listener_core: "none".to_owned(),
        };
        let l = plan(
            &Env::default(),
            "/usr/local/bin/noevia-server",
            &p,
            &[0; 32],
            &[0; 16],
        )
        .unwrap_or_else(|r| unreachable!("{r:?}"));
        assert_eq!(get(&l.node.set, "NOEVIA_FRONT_CLIENT_IP"), Some(""));
        assert_eq!(
            get(&l.node.set, "NOEVIA_LEGACY_CLAIM_BIN"),
            Some("/usr/local/bin/noevia-server")
        );
    }
}
