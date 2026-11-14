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

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::time::Duration;

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
    // The children get the directory's own bytes; a lossy copy would prepare another one.
    let Some(data) = data.to_str().map(str::to_owned) else {
        return Err(Refusal(format!(
            "web-supervisor: {SWITCH}=1 but the data directory {} is not valid UTF-8; set UI_DATA_DIR to a plain path",
            data.display()
        )));
    };
    Ok(STEPS
        .iter()
        .map(|step| ChildSpec {
            program: bin.to_owned(),
            args: vec![(*step).to_owned()],
            set: vec![("UI_DATA_DIR".to_owned(), data.clone())],
        })
        .collect())
}

/// How the preparation ended when it did not fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Every step succeeded: start the pair.
    Done,
    /// A stop signal arrived during a step: the step was stopped and nothing else starts (exit 0,
    /// as a docker stop of the pair).
    Stopped,
}

/// Runs the steps in order, stdout and stderr passed through (the key notice and the first-run
/// setup code belong in the container log, as Node printed them). The first failure is the
/// refusal message. `stop` resolves on TERM/INT: the running step is stopped (TERM, then KILL
/// after five seconds) and the rest is skipped.
pub async fn run<F: Future<Output = ()>>(
    steps: &[ChildSpec],
    env: &Env,
    cwd: &Path,
    limit: Duration,
    mut stop: Pin<&mut F>,
) -> Result<Outcome, String> {
    for step in steps {
        if run_step(step, env, cwd, limit, stop.as_mut()).await? == Outcome::Stopped {
            return Ok(Outcome::Stopped);
        }
    }
    Ok(Outcome::Done)
}

fn label(step: &ChildSpec) -> String {
    format!("{} {}", step.program, step.args.join(" "))
}

async fn run_step<F: Future<Output = ()>>(
    step: &ChildSpec,
    env: &Env,
    cwd: &Path,
    limit: Duration,
    stop: Pin<&mut F>,
) -> Result<Outcome, String> {
    let mut cmd = Command::new(&step.program);
    cmd.args(&step.args)
        .env_clear()
        .envs(env.vars())
        .envs(step.set.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(Stdio::null());
    let mut child = tokio::process::Command::from(cmd).spawn().map_err(|e| {
        format!(
            "web-supervisor: cannot start {} ({}: {e}); the pair was not started",
            label(step),
            spawn_failure_code(&e)
        )
    })?;
    let stopped = tokio::select! {
        waited = child.wait() => {
            return match waited {
                Ok(status) if status.success() => Ok(Outcome::Done),
                Ok(status) => Err(format!(
                    "web-supervisor: {} failed ({}); the pair was not started",
                    label(step),
                    status_code(status)
                )),
                Err(e) => Err(format!(
                    "web-supervisor: cannot wait for {}: {e}; the pair was not started",
                    label(step)
                )),
            };
        }
        () = tokio::time::sleep(limit) => false,
        () = stop => true,
    };
    if let Some(pid) = child.id() {
        signal(pid, nix::sys::signal::Signal::SIGTERM);
    }
    let code = match tokio::time::timeout(KILL_AFTER, child.wait()).await {
        Ok(Ok(status)) => status_code(status),
        _ => {
            let _ = child.kill().await;
            child.wait().await.map(status_code).unwrap_or(137)
        }
    };
    if stopped {
        eprintln!(
            "web-supervisor: stop signal during {}; stopped it ({code}); nothing started",
            label(step)
        );
        return Ok(Outcome::Stopped);
    }
    Err(format!(
        "web-supervisor: {} did not finish within {} s (stopped, {code}); the pair was not started",
        label(step),
        limit.as_secs()
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn features(lines: &str) -> Features {
        Features::parse(lines)
    }

    #[test]
    fn off_unless_exactly_one() {
        for value in [
            None,
            Some(""),
            Some("0"),
            Some("true"),
            Some("yes"),
            Some(" 1"),
        ] {
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
        assert!(
            err.0.contains("does not advertise boot-ensure-v1"),
            "{}",
            err.0
        );
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

    fn never() -> Pin<Box<std::future::Pending<()>>> {
        Box::pin(std::future::pending())
    }

    fn block<T>(f: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn steps_stop_at_the_first_failure_and_name_it() {
        let dir = tempfile::tempdir().unwrap();
        let mark = dir.path().join("second-ran");
        let steps = [
            sh("[ \"$QA_STEP\" = qa ] && exit 3"),
            sh(&format!("touch '{}'", mark.display())),
        ];
        let err = block(run(
            &steps,
            &Env::default(),
            dir.path(),
            STEP_TIMEOUT,
            never().as_mut(),
        ))
        .unwrap_err();
        assert!(err.contains("failed (3)"), "{err}");
        assert!(err.contains("the pair was not started"), "{err}");
        assert!(!mark.exists());
        let ok = [sh("exit 0"), sh(&format!("touch '{}'", mark.display()))];
        let done = block(run(
            &ok,
            &Env::default(),
            dir.path(),
            STEP_TIMEOUT,
            never().as_mut(),
        ));
        assert_eq!(done, Ok(Outcome::Done));
        assert!(mark.exists());
    }

    #[test]
    fn a_hung_step_is_stopped_at_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let err = block(run(
            &[sh("sleep 30")],
            &Env::default(),
            dir.path(),
            Duration::from_millis(300),
            never().as_mut(),
        ))
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
        let err = block(run(
            &[step],
            &Env::default(),
            Path::new("/"),
            STEP_TIMEOUT,
            never().as_mut(),
        ))
        .unwrap_err();
        assert!(err.contains("(127:"), "{err}");
    }

    #[test]
    fn a_stop_signal_stops_the_running_step_and_skips_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let mark = dir.path().join("second-ran");
        let steps = [sh("sleep 30"), sh(&format!("touch '{}'", mark.display()))];
        let started = Instant::now();
        let outcome = block(async {
            let stop = tokio::time::sleep(Duration::from_millis(200));
            tokio::pin!(stop);
            run(
                &steps,
                &Env::default(),
                dir.path(),
                STEP_TIMEOUT,
                stop.as_mut(),
            )
            .await
        });
        assert_eq!(outcome, Ok(Outcome::Stopped));
        assert!(!mark.exists());
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
