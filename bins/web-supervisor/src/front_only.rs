//! The front alone, without Core, behind `NOEVIA_SUPERVISOR_FRONT_ONLY=1` (off by default: unset or
//! any other value starts the pair exactly as the script does).
//!
//! Once every route is native, the image no longer needs Node: the container is noevia-server and
//! the work Node did before it listened (the secrets key and the database's boot schema, track
//! R12). With the switch on, the supervisor
//! - skips every pairing check (there is no Core to pair with; `node` need not be installed),
//! - always runs the front's two preparation steps (ensure.rs) first, under the pair lock,
//!   whatever `NOEVIA_SUPERVISOR_ENSURE` says, because nothing else would make the key or the schema,
//! - starts only the front, told `NOEVIA_FRONT_ONLY=1` so it answers `/api/ready` itself and refuses
//!   what is still Node's instead of proxying into a port nobody listens on,
//! - keeps the pair lock, the per-start store secret and start id, the stop signals and the exit
//!   rules of the pair (the front's exit ends the container non-zero; TERM/INT exit 0).
//!
//! The mode runs only when the front advertises both [`CAPABILITY`] and the ensure capability in
//! `--features`: an older front, told nothing about the missing Node, would report itself not
//! ready forever and proxy every unported route into a closed port.

use std::path::Path;
use std::time::Duration;

use crate::env::Env;
use crate::pairing::{Features, Refusal};
use crate::plan::{hex, ui_port, ChildSpec};

/// The switch. Only `1` turns the mode on.
pub const SWITCH: &str = "NOEVIA_SUPERVISOR_FRONT_ONLY";
/// The `--features` line of a front that serves without Node.
pub const CAPABILITY: &str = "front-only-v1";
/// What the front is told (and what the supervisor scrubs from an inherited environment, so a pair
/// front never believes it is alone).
pub const FRONT_FLAG: &str = "NOEVIA_FRONT_ONLY";
/// The front alone keeps most of Docker's default 10 s stop: no Core waits behind it.
pub const STOP_DEADLINE: Duration = Duration::from_secs(8);

/// Whether the switch is on.
pub fn enabled(env: &Env) -> bool {
    env.get("NOEVIA_SUPERVISOR_FRONT_ONLY") == "1"
}

/// The front's start without Core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Solo {
    pub front: ChildSpec,
    /// The front's preparation, in order (never empty: the key and the schema have no other maker).
    pub ensure: Vec<ChildSpec>,
    /// UI_PORT+1000: reserved and named to the front (its configuration requires an upstream), but
    /// nothing listens there; the front, told it is alone, never connects to it.
    pub legacy_port: u32,
    pub store_port: u32,
    pub warnings: Vec<String>,
}

/// Plans the front alone. `store_token` is 32 random bytes and `start_id` 16, as for the pair.
pub fn plan(
    env: &Env,
    bin: &str,
    features: &Features,
    data: &Path,
    store_token: &[u8],
    start_id: &[u8],
) -> Result<Solo, Refusal> {
    for capability in [CAPABILITY, crate::ensure::CAPABILITY] {
        if !features.has(capability) {
            return Err(Refusal(format!(
                "web-supervisor: {SWITCH}=1 but this noevia-server does not advertise {capability} (an older front cannot run without Core); unset {SWITCH} or update the image"
            )));
        }
    }
    let (raw, port) = ui_port(env)?;
    let legacy = port + 1000;
    let store_port = port + 2000;
    if store_port > 65535 {
        return Err(Refusal(format!(
            "web-supervisor: UI_PORT '{raw}' leaves no room for the project store port UI_PORT+2000"
        )));
    }
    let token = hex(store_token);
    if token.len() != 64 {
        return Err(Refusal(
            "web-supervisor: could not make the project store secret".to_owned(),
        ));
    }
    let start = hex(start_id);
    if start.len() != 32 {
        return Err(Refusal(
            "web-supervisor: could not make the start id".to_owned(),
        ));
    }
    let ensure = crate::ensure::steps(bin, data)?;
    // Valid UTF-8 by now (`steps` refused otherwise). The front gets the directory its preparation
    // used: without Core there is no `server/ui-data` fallback of Node's, and the front itself
    // requires UI_DATA_DIR.
    let data = data.to_str().unwrap_or_default().to_owned();
    let mut warnings = Vec::new();
    let dav = env.get("COWORK_DAV_PORT");
    if !dav.is_empty() && dav != "0" {
        // Only the old Node listener served DAV on its own port; the native front serves sharing
        // on its own listener, so the value is only noted.
        warnings.push(format!(
            "web-supervisor: COWORK_DAV_PORT is set but {SWITCH}=1 starts no Core; Node's DAV listener does not run"
        ));
    }
    let set = vec![
        (
            "NOEVIA_PROJECT_STORE_PORT".to_owned(),
            store_port.to_string(),
        ),
        ("NOEVIA_PROJECT_STORE_TOKEN".to_owned(), token),
        ("NOEVIA_START_ID".to_owned(), start),
        (
            "NOEVIA_LEGACY_UPSTREAM".to_owned(),
            format!("http://127.0.0.1:{legacy}"),
        ),
        ("UI_DATA_DIR".to_owned(), data),
        (FRONT_FLAG.to_owned(), "1".to_owned()),
    ];
    Ok(Solo {
        front: ChildSpec {
            program: bin.to_owned(),
            args: Vec::new(),
            set,
        },
        ensure,
        legacy_port: legacy,
        store_port,
        warnings,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn both() -> Features {
        Features::parse("rust-auth\nfront-only-v1\nboot-ensure-v1\n")
    }

    fn get<'a>(set: &'a [(String, String)], key: &str) -> Option<&'a str> {
        set.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn solo(env: &Env) -> Solo {
        plan(
            env,
            "/bin/front",
            &both(),
            Path::new("/data"),
            &[0xab; 32],
            &[0x01; 16],
        )
        .unwrap_or_else(|r| unreachable!("{r:?}"))
    }

    #[test]
    fn only_the_exact_value_one_turns_it_on() {
        assert!(!enabled(&Env::default()));
        for off in ["", "0", "true", "yes", " 1", "1 ", "01"] {
            assert!(!enabled(&Env::from_pairs([(SWITCH, off)])), "{off:?}");
        }
        assert!(enabled(&Env::from_pairs([(SWITCH, "1")])));
    }

    #[test]
    fn the_front_alone_is_told_it_is_alone_and_gets_the_pair_values() {
        let s = solo(&Env::default());
        assert_eq!(s.front.program, "/bin/front");
        assert!(s.front.args.is_empty());
        assert_eq!(get(&s.front.set, FRONT_FLAG), Some("1"));
        assert_eq!(
            get(&s.front.set, "NOEVIA_PROJECT_STORE_PORT"),
            Some("10021")
        );
        assert_eq!(
            get(&s.front.set, "NOEVIA_PROJECT_STORE_TOKEN").map(str::len),
            Some(64)
        );
        assert_eq!(
            get(&s.front.set, "NOEVIA_START_ID"),
            Some("01010101010101010101010101010101")
        );
        assert_eq!(
            get(&s.front.set, "NOEVIA_LEGACY_UPSTREAM"),
            Some("http://127.0.0.1:9021")
        );
        assert_eq!(get(&s.front.set, "UI_DATA_DIR"), Some("/data"));
        assert_eq!(s.legacy_port, 9021);
        assert_eq!(s.store_port, 10021);
        // Nothing Node-only reaches the front.
        for node_only in [
            "UI_HOST",
            "UI_PORT",
            "NOEVIA_FRONT_CLIENT_IP",
            "NOEVIA_LEGACY_CLAIM_BIN",
        ] {
            assert_eq!(get(&s.front.set, node_only), None, "{node_only}");
        }
        assert!(s.warnings.is_empty());
    }

    #[test]
    fn the_preparation_always_runs_key_first_whatever_the_ensure_switch_says() {
        for ensure in ["", "0", "1"] {
            let s = solo(&Env::from_pairs([("NOEVIA_SUPERVISOR_ENSURE", ensure)]));
            let args: Vec<_> = s.ensure.iter().map(|c| c.args.join(" ")).collect();
            assert_eq!(
                args,
                ["--ensure-secrets-key", "--ensure-schema"],
                "{ensure:?}"
            );
            for step in &s.ensure {
                assert_eq!(step.program, "/bin/front");
                assert_eq!(get(&step.set, "UI_DATA_DIR"), Some("/data"));
            }
        }
    }

    #[test]
    fn a_front_missing_either_capability_is_refused_naming_it() {
        for (lines, missing) in [
            ("boot-ensure-v1\n", CAPABILITY),
            ("front-only-v1\n", crate::ensure::CAPABILITY),
            ("", CAPABILITY),
            // grep -qx: a line that only contains the name does not count.
            ("front-only-v1x\nboot-ensure-v1\n", CAPABILITY),
        ] {
            let err = plan(
                &Env::default(),
                "/f",
                &Features::parse(lines),
                Path::new("/d"),
                &[0; 32],
                &[0; 16],
            )
            .err()
            .map(|r| r.0)
            .unwrap_or_default();
            assert!(
                err.contains(&format!("does not advertise {missing}")),
                "{lines:?}: {err}"
            );
            assert!(err.contains(SWITCH), "{err}");
        }
    }

    #[test]
    fn ports_and_random_input_are_checked_as_for_the_pair() {
        let refuse = |env: Env, token: &[u8], start: &[u8]| {
            plan(&env, "/f", &both(), Path::new("/d"), token, start)
                .err()
                .map(|r| r.0)
                .unwrap_or_default()
        };
        for bad in ["0", "64536", "abc", ""] {
            let env = Env::from_pairs([("UI_PORT", bad)]);
            let msg = refuse(env, &[0; 32], &[0; 16]);
            // An empty UI_PORT is the default, 8021.
            if bad.is_empty() {
                assert_eq!(msg, "");
            } else {
                assert!(msg.contains("no room for the legacy port"), "{bad}: {msg}");
            }
        }
        let msg = refuse(Env::from_pairs([("UI_PORT", "64000")]), &[0; 32], &[0; 16]);
        assert!(msg.contains("no room for the project store port"), "{msg}");
        assert!(refuse(Env::default(), &[0; 31], &[0; 16]).contains("project store secret"));
        assert!(refuse(Env::default(), &[0; 32], &[0; 15]).contains("start id"));
    }

    #[test]
    fn a_dav_port_is_only_noted() {
        let s = solo(&Env::from_pairs([("COWORK_DAV_PORT", "8040")]));
        assert_eq!(s.warnings.len(), 1);
        assert!(s.warnings[0].contains("Node's DAV listener does not run"));
        assert!(solo(&Env::from_pairs([("COWORK_DAV_PORT", "0")]))
            .warnings
            .is_empty());
    }

    #[test]
    fn a_non_utf8_data_directory_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let dir = std::ffi::OsStr::from_bytes(b"/data/\xff");
        let err = plan(
            &Env::default(),
            "/f",
            &both(),
            Path::new(dir),
            &[0; 32],
            &[0; 16],
        )
        .err()
        .map(|r| r.0)
        .unwrap_or_default();
        assert!(err.contains("valid UTF-8"), "{err}");
    }
}
