//! The front's start-up preparation before the pair starts, behind `NOEVIA_SUPERVISOR_ENSURE=1`
//! (off by default: unset or any other value starts the pair exactly as the script does).
//!
//! Node made `UI_DATA_DIR/secrets.key` and booted `cowork.db` on its own start. Once Node no longer
//! runs first, the front does both from the command line (track R12: `noevia-server
//! --ensure-secrets-key` and `--ensure-schema`, before any listener). With the switch on, the
//! supervisor runs those two steps in that order, under the pair lock (an old pair still holding
//! the data directory cannot race the schema boot), each bounded in time; a failed step refuses the
//! start and neither child runs.
//!
//! The steps run only when the front advertises [`CAPABILITY`] in `--features`: an older
//! noevia-server does not know these arguments and would start serving instead of preparing.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::env::Env;
use crate::pairing::{Features, Refusal};
use crate::plan::ChildSpec;
use crate::supervise::{signal, spawn_failure_code, status_code};

/// The switch. Only `1` turns the steps on.
pub const SWITCH: &str = "NOEVIA_SUPERVISOR_ENSURE";
/// The `--features` line of a front that prepares from the command line.
pub const CAPABILITY: &str = "boot-ensure-v1";
/// The steps, in order: the key first (the schema boot may encrypt nothing yet, but Node made the
/// key before it opened the database, and a bad key must refuse before anything is written).
pub const STEPS: &[&str] = &["--ensure-secrets-key", "--ensure-schema"];
/// One step's limit: a schema boot on a large database may take a while, a hung one may not hold
/// the start forever.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(300);
const KILL_AFTER: Duration = Duration::from_secs(5);

/// Whether the switch is on.
pub fn enabled(env: &Env) -> bool {
    env.get("NOEVIA_SUPERVISOR_ENSURE") == "1"
}

/// The steps to run before the pair, or none when the switch is off. `data` is the data directory
/// both children use; it is passed explicitly because the front's steps require `UI_DATA_DIR` where
/// Node fell back to its own `server/ui-data`.
pub fn plan(
    env: &Env,
    bin: &str,
    features: &Features,
    data: &Path,
) -> Result<Vec<ChildSpec>, Refusal> {
    if !enabled(env) {
        return Ok(Vec::new());
    }
    if !features.has(CAPABILITY) {
        return Err(Refusal(format!(
            "web-supervisor: {SWITCH}=1 but this noevia-server does not advertise {CAPABILITY} (an older front would start serving instead of preparing); unset {SWITCH} or update the image"
        )));
    }
    let data = data.to_string_lossy().into_owned();
    Ok(STEPS
        .iter()
        .map(|step| ChildSpec {
            program: bin.to_owned(),
            args: vec![(*step).to_owned()],
            set: vec![("UI_DATA_DIR".to_owned(), data.clone())],
        })
        .collect())
}

/// Runs the steps in order, stdout and stderr passed through (the key notice and the first-run
/// setup code belong in the container log, as Node printed them). The first failure is the
/// refusal message.
pub fn run(steps: &[ChildSpec], env: &Env, cwd: &Path, limit: Duration) -> Result<(), String> {
    for step in steps {
        run_step(step, env, cwd, limit)?;
    }
    Ok(())
}

fn label(step: &ChildSpec) -> String {
    format!("{} {}", step.program, step.args.join(" "))
}

fn run_step(step: &ChildSpec, env: &Env, cwd: &Path, limit: Duration) -> Result<(), String> {
    let mut cmd = Command::new(&step.program);
    cmd.args(&step.args)
        .env_clear()
        .envs(env.vars())
        .envs(step.set.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| {
        format!(
            "web-supervisor: cannot start {} ({}: {e}); the pair was not started",
            label(step),
            spawn_failure_code(&e)
        )
    })?;
    let started = Instant::now();
    let mut termed = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if termed => {
                return Err(format!(
                    "web-supervisor: {} did not finish within {} s (stopped, {}); the pair was not started",
                    label(step),
                    limit.as_secs(),
                    status_code(status)
                ))
            }
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(format!(
                    "web-supervisor: {} failed ({}); the pair was not started",
                    label(step),
                    status_code(status)
                ))
            }
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "web-supervisor: cannot wait for {}: {e}; the pair was not started",
                    label(step)
                ));
            }
        }
        let elapsed = started.elapsed();
        if !termed && elapsed >= limit {
            signal(child.id(), nix::sys::signal::Signal::SIGTERM);
            termed = true;
        } else if termed && elapsed >= limit + KILL_AFTER {
            let _ = child.kill();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn features(lines: &str) -> Features {
        Features::parse(lines)
    }

    #[test]
    fn off_unless_exactly_one() {
        for value in [None, Some(""), Some("0"), Some("true"), Some("yes"), Some(" 1")] {
            let env = match value {
                None => Env::default(),
                Some(v) => Env::from_pairs([(SWITCH, v)]),
            };
            let steps = plan(&env, "/bin/front", &features(""), Path::new("/data")).unwrap();
            assert!(steps.is_empty(), "{value:?}");
        }
    }

    #[test]
    fn on_needs_the_capability_line() {
        let env = Env::from_pairs([(SWITCH, "1")]);
        let err = plan(&env, "/bin/front", &features("rust-auth"), Path::new("/d")).unwrap_err();
        assert!(err.0.contains("does not advertise boot-ensure-v1"), "{}", err.0);
        // A whole line only, as every capability.
        let err = plan(
            &env,
            "/bin/front",
            &features("boot-ensure-v1x"),
            Path::new("/d"),
        )
        .unwrap_err();
        assert!(err.0.contains("does not advertise"), "{}", err.0);
    }

    #[test]
    fn on_runs_key_then_schema_with_the_data_directory() {
        let env = Env::from_pairs([(SWITCH, "1")]);
        let steps = plan(
            &env,
            "/bin/front",
            &features("rust-auth\nboot-ensure-v1"),
            Path::new("/qa/ui-data"),
        )
        .unwrap();
        let args: Vec<_> = steps.iter().map(|s| s.args.join(" ")).collect();
        assert_eq!(args, ["--ensure-secrets-key", "--ensure-schema"]);
        for s in &steps {
            assert_eq!(s.program, "/bin/front");
            assert_eq!(
                s.set,
                [("UI_DATA_DIR".to_owned(), "/qa/ui-data".to_owned())]
            );
        }
    }

    fn sh(script: &str) -> ChildSpec {
        ChildSpec {
            program: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), script.to_owned()],
            set: vec![("QA_STEP".to_owned(), "qa".to_owned())],
        }
    }

    #[test]
    fn steps_stop_at_the_first_failure_and_name_it() {
        let dir = tempfile::tempdir().unwrap();
        let mark = dir.path().join("second-ran");
        let steps = [
            sh("[ \"$QA_STEP\" = qa ] && exit 3"),
            sh(&format!("touch '{}'", mark.display())),
        ];
        let err = run(&steps, &Env::default(), dir.path(), STEP_TIMEOUT).unwrap_err();
        assert!(err.contains("failed (3)"), "{err}");
        assert!(err.contains("the pair was not started"), "{err}");
        assert!(!mark.exists());
        let ok = [sh("exit 0"), sh(&format!("touch '{}'", mark.display()))];
        run(&ok, &Env::default(), dir.path(), STEP_TIMEOUT).unwrap();
        assert!(mark.exists());
    }

    #[test]
    fn a_hung_step_is_stopped_at_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let err = run(
            &[sh("sleep 30")],
            &Env::default(),
            dir.path(),
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(err.contains("did not finish within 0 s"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_missing_program_refuses_with_bash_s_code() {
        let step = ChildSpec {
            program: "/nonexistent/qa-front".to_owned(),
            args: vec!["--ensure-schema".to_owned()],
            set: Vec::new(),
        };
        let err = run(&[step], &Env::default(), Path::new("/"), STEP_TIMEOUT).unwrap_err();
        assert!(err.contains("(127:"), "{err}");
    }
}
