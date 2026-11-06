//! The production [`Env`]: `Maint` for the exclusion and the reload, the model management service
//! for the scan and the registration, and [`super::files`] for "is this file complete".
//!
//! Replacement points when the in-flight neighbours merge (docs/model-folder-sync.md):
//! - `take` -> the maintenance lease authority (row J) instead of `Maint::acquire`;
//! - `reload` -> the load path's reload under a hold (row A);
//! - `busy` -> the native auto-tune state (row K / K5), which today is visible here only as the
//!   exclusion being unavailable (`local_held`, or the gate / Node lease being taken).
use super::{files, Env, Fault, Hold, Refusal, Registered, Reloaded, Settled};
use crate::model_maint::{self, HoldError, Maint};
use crate::model_manager_scan::{Loader, Sent};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// The longest the exclusion is kept for one run (it is renewed until then, and given back on drop).
const HOLD_MAX: Duration = Duration::from_secs(600);
/// A file younger than this, by its modification time, may still be written.
pub const SETTLE: Duration = Duration::from_secs(120);
/// The most the original receipt of a registration is waited for (the loader call times out at 120 s).
const SETTLE_WAIT: Duration = Duration::from_secs(130);
/// The loader's answer to a permit it no longer honours.
const STALE: &str = "Stale maintenance operation.";
/// The wait between the two looks at a file.
pub const PROBE: Duration = Duration::from_secs(2);

impl Hold for model_maint::Hold {
    fn excluded(&self) -> bool {
        model_maint::Hold::excluded(self)
    }
    fn may_start(&self, step: Duration) -> bool {
        model_maint::Hold::may_start(self, step)
    }
    async fn release(self) {
        model_maint::Hold::release(self).await;
    }
}

pub struct Live {
    maint: Arc<Maint>,
    loader: Loader,
    root: PathBuf,
    settle: Duration,
    probe: Duration,
    settle_wait: Duration,
}

impl Live {
    /// `None` unless this front can do the work: a llama.cpp router, the model management service
    /// and a models folder mounted at `LLAMACPP_MODELS_PATH`. Nothing outside that root is read.
    pub fn from_lookup(
        maint: &Arc<Maint>,
        loader: &Loader,
        get: &impl Fn(&str) -> Option<String>,
    ) -> Option<Live> {
        if !loader.configured() || maint.router.is_none() || maint.ini.is_none() {
            return None;
        }
        let models = get("LLAMACPP_MODELS_PATH").filter(|v| !v.is_empty())?;
        let root = std::fs::canonicalize(models).ok()?;
        Some(Live {
            maint: Arc::clone(maint),
            loader: loader.clone(),
            root,
            settle: SETTLE,
            probe: PROBE,
            settle_wait: SETTLE_WAIT,
        })
    }

    pub fn with_timing(mut self, settle: Duration, probe: Duration) -> Live {
        self.settle = settle;
        self.probe = probe;
        self
    }

    /// How long a registration's original receipt is waited for (tests shorten it).
    pub fn with_settle_wait(mut self, wait: Duration) -> Live {
        self.settle_wait = wait;
        self
    }
}

fn is_revision(r: &str) -> bool {
    r.len() == 64 && r.bytes().all(|b| b.is_ascii_hexdigit())
}

/// JavaScript `encodeURIComponent`.
pub fn encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

type Answer = Option<(axum::http::StatusCode, Option<js_json::JValue>)>;

fn fault(answer: Answer) -> Fault {
    match answer {
        None => Fault::refused(None, "model manager HTTP unreachable"),
        Some((status, body)) => {
            let text = body
                .as_ref()
                .and_then(|b| b.get("error").as_str().filter(|s| !s.is_empty()));
            Fault::refused(
                Some(status.as_u16()),
                text.map_or_else(
                    || format!("model manager HTTP {}", status.as_u16()),
                    str::to_owned,
                ),
            )
        }
    }
}

impl Env for Live {
    type Hold = model_maint::Hold;

    fn busy(&self) -> bool {
        self.maint.local_held()
    }

    async fn take(&self) -> Result<model_maint::Hold, Refusal> {
        // Dropping this waiter detaches acquisition. A late grant is dropped by its owning
        // task and releases normally; local reservation remains owned until the answer settles.
        let maint = Arc::clone(&self.maint);
        match tokio::spawn(async move { maint.acquire("model folder sync", HOLD_MAX, false).await })
            .await
        {
            Ok(Ok(h)) => Ok(h),
            Ok(Err(HoldError::Busy)) => Err(Refusal::Busy),
            _ => Err(Refusal::Unavailable),
        }
    }

    async fn list_unregistered(&self) -> Result<Vec<String>, Fault> {
        match self.loader.manager_get("models").await {
            Some((status, Some(body))) if status.is_success() => {
                Ok(match body.get("unregistered") {
                    js_json::JValue::Arr(items) => items
                        .iter()
                        .filter_map(|i| i.as_str().map(str::to_owned))
                        .collect(),
                    _ => Vec::new(),
                })
            }
            Some((status, None)) if status.is_success() => Ok(Vec::new()),
            other => Err(fault(other)),
        }
    }

    async fn settled(&self, stem: &str) -> Settled {
        let (root, settle, name) = (self.root.clone(), self.settle, stem.to_owned());
        let look = |root: PathBuf, name: String| async move {
            tokio::task::spawn_blocking(move || {
                files::probe(&root, &name, SystemTime::now(), settle)
            })
            .await
            .unwrap_or_else(|_| Err("the check did not finish".into()))
        };
        let first = look(root.clone(), name.clone()).await;
        if first.is_err() {
            return files::stable(&first, &first);
        }
        tokio::time::sleep(self.probe).await;
        let second = look(root, name).await;
        files::stable(&first, &second)
    }

    async fn register(&self, hold: &model_maint::Hold, stem: &str) -> Result<Registered, Fault> {
        let Some(store) = self.maint.ini.clone() else {
            return Err(Fault::refused(None, "no preset file"));
        };
        // R: the revision this registration is allowed to replace, read under the shared
        // publication transaction while this exact hold still covers it.
        let snapshot = store
            .snapshot_covered(hold.write_fence())
            .await
            .map_err(|_| Fault::refused(None, "maintenance coverage was lost"))?;
        let Some(mut operation) = hold.begin_operation("register", Some(&snapshot.revision)) else {
            return Err(Fault::refused(None, "maintenance coverage was lost"));
        };
        let rest = format!("sections/{}/safe-defaults", encode_component(stem));
        let answer = self
            .loader
            .manager_post_fenced(&rest, &operation.fence)
            .await;
        if matches!(answer, Err(Sent::NotSent)) {
            // The call provably never left: nothing was done, nothing is owed. Bookkeeping only.
            operation.settled = operation.authority.settle_operation(&operation.fence);
            return Err(Fault::refused(None, "model manager HTTP unreachable"));
        }
        let answer = answer.ok();
        let authority = operation.authority.clone();
        let fence = operation.fence.clone();
        match answer {
            Some((status, body)) if status.is_success() => {
                // The loader recorded the terminal state in its publication transaction; this
                // reads the ORIGINAL receipt (never a new permit) and takes the revision from it.
                let Some(receipt) = authority
                    .wait_for_settlement(&fence, self.settle_wait)
                    .await
                else {
                    return Err(Fault {
                        status: Some(status.as_u16()),
                        message: "registration receipt is not settled".into(),
                        uncertain: true,
                    });
                };
                operation.settled = true;
                let valid = is_revision;
                let from_receipt = receipt.revision.filter(|r| valid(r));
                let from_body = body.as_ref().and_then(|b| {
                    b.get("revision")
                        .as_str()
                        .filter(|r| valid(r))
                        .map(str::to_owned)
                });
                if receipt.state != "settled" || from_receipt.is_none() {
                    // A conflict receipt on a 2xx, or no revision: unproven, fail closed.
                    return Err(Fault {
                        status: Some(status.as_u16()),
                        message: "registration receipt has no settled revision".into(),
                        uncertain: true,
                    });
                }
                if from_body.is_some() && from_body != from_receipt {
                    return Err(Fault {
                        status: Some(status.as_u16()),
                        message: "registration receipt and answer disagree".into(),
                        uncertain: true,
                    });
                }
                Ok(Registered {
                    revision: from_receipt,
                })
            }
            // The loader answered and did not publish: a refusal of the file (400/404/409) or of
            // the permit (409 "Stale maintenance operation."). Bookkeeping only.
            Some((status, body)) if matches!(status.as_u16(), 400 | 404 | 409) => {
                operation.settled = authority.settle_operation(&fence);
                let stale = status.as_u16() == 409
                    && body
                        .as_ref()
                        .is_some_and(|b| b.get("detail").as_str() == Some(STALE));
                if stale {
                    // Not a verdict on the file: models.ini moved or the permit is no longer ours.
                    return Err(Fault::refused(None, STALE));
                }
                Err(fault(Some((status, body))))
            }
            // Anything else (a 5xx, a dropped or timed-out call, a cut body) may have happened
            // inside the loader: wait for the ORIGINAL receipt; never repeat the effect.
            other => {
                let message = fault(other).message;
                match authority
                    .wait_for_settlement(&fence, self.settle_wait)
                    .await
                {
                    Some(receipt) if receipt.state == "conflict" => {
                        operation.settled = true;
                        Err(Fault::refused(
                            None,
                            "registration conflicted with a newer write",
                        ))
                    }
                    Some(receipt) => {
                        // Settled after all: the loader's own transaction recorded it, and its
                        // receipt (never this caller's bookkeeping) names the revision it wrote.
                        operation.settled = true;
                        match receipt.revision.filter(|r| is_revision(r)) {
                            Some(revision) if receipt.state == "settled" => Ok(Registered {
                                revision: Some(revision),
                            }),
                            _ => Err(Fault {
                                status: None,
                                message: format!("{message}; settled without a proven revision"),
                                uncertain: true,
                            }),
                        }
                    }
                    None => Err(Fault {
                        status: None,
                        message,
                        uncertain: true,
                    }),
                }
            }
        }
    }

    async fn revision(&self, hold: &model_maint::Hold) -> Option<String> {
        let store = self.maint.ini.clone()?;
        store
            .snapshot_covered(hold.write_fence())
            .await
            .ok()
            .map(|s| s.revision)
    }

    async fn reload(&self, hold: &model_maint::Hold, revision: &str) -> Reloaded {
        let Some(router) = self.maint.router.as_ref() else {
            return Reloaded::No;
        };
        let Some(store) = self.maint.ini.clone() else {
            return Reloaded::No;
        };
        // A new permit with the NEW revision under the same continuously covered hold: the covered
        // read here, and the reload's own covered read + permit (router_reload), both refuse a file
        // that is not the registration's.
        let Ok(snapshot) = store.snapshot_covered(hold.write_fence()).await else {
            return Reloaded::No;
        };
        if snapshot.revision != revision {
            return Reloaded::No;
        }
        // Guarded, nothing unloaded: a loaded model whose preset the reload would change refuses.
        let outcome = self
            .maint
            .reload_expected(router, hold, false, Some(&snapshot.text))
            .await;
        if outcome.status == 200 {
            Reloaded::Yes
        } else {
            Reloaded::No
        }
    }
}

/// Starts the worker when this front can run it. `stop` ends the timer; a scan in flight is
/// drained under its exclusion.
pub fn spawn(
    maint: &Arc<Maint>,
    loader: &Loader,
    get: &impl Fn(&str) -> Option<String>,
    stop: tokio::sync::watch::Receiver<bool>,
) -> Option<tokio::task::JoinHandle<()>> {
    let Some(env) = Live::from_lookup(maint, loader, get) else {
        if loader.configured() && maint.router.is_some() {
            eprintln!(
                "[models] new files in the models folder are not set up on their own: LLAMACPP_MODELS_PATH is not a readable folder"
            );
        }
        return None;
    };
    let state = get("UI_DATA_DIR")
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join("model-folder-sync.json"));
    let worker = Arc::new(super::Worker::new(
        env,
        state,
        Arc::new(|m: &str| println!("{m}")),
    ));
    Some(super::spawn(worker, super::Schedule::default(), stop))
}
