//! Replays `fixtures/node-upstream.v1.json` (written by `record_upstream.cjs` from noevia-core's own
//! chat.cjs, ssrf.cjs, providers.cjs, chat-machines.cjs and rust-chat-native.cjs): every recorded
//! call target, bearer header and provider refusal must come out of this crate exactly.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::LazyLock;

use chat_upstream::binding::{self, Recheck};
use chat_upstream::cut::{self, NotCut};
use chat_upstream::host::Unsupported;
use chat_upstream::refusal::{self, Facts};
use chat_upstream::target::{self, Connection, Fetch, KeyUse, Refused, Target, Turn, User};
use js_json::JValue;

const FIXTURE: &str = include_str!("fixtures/node-upstream.v1.json");
static RECORDED: LazyLock<JValue> = LazyLock::new(|| js_json::parse(FIXTURE).expect("fixture"));

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        other => panic!("not an array: {other:?}"),
    }
}

fn opt_text(v: &JValue) -> Option<&str> {
    v.as_str()
}

fn static_text(s: &str) -> &'static str {
    [
        target::ORIGIN_NOT_APPROVED,
        target::SIGN_IN_OFF,
        target::SIGN_IN_FOREIGN,
        target::SIGN_IN_RECONNECT,
        target::SIGN_IN_MISSING,
    ]
    .into_iter()
    .find(|t| *t == s)
    .unwrap_or_else(|| panic!("an answer text the port does not have: {s}"))
}

#[test]
fn sources_are_pinned() {
    let JValue::Obj(sources) = RECORDED.get("sources") else {
        panic!("no sources")
    };
    assert_eq!(sources.len(), 5);
    for (_, sha) in sources {
        assert_eq!(sha.as_str().unwrap().len(), 64);
    }
}

#[test]
fn every_recorded_target() {
    let targets = arr(RECORDED.get("targets"));
    assert!(targets.len() > 3000, "{} targets", targets.len());
    let (mut ok, mut refused, mut thrown) = (0, 0, 0);
    for (i, t) in targets.iter().enumerate() {
        let c = t.get("case");
        let node = t.get("node");
        let user = match c.get("user") {
            JValue::Obj(_) => Some(User {
                id: opt_text(c.get("user").get("id")).map(str::to_string),
                admin: c.get("user").get("admin").truthy(),
            }),
            _ => None,
        };
        let machine = opt_text(c.get("machine"));
        let row = if machine.is_some() {
            node.get("row")
        } else {
            c.get("row")
        };
        let connection = match c.get("connection").as_str() {
            Some("connected") => Connection::Connected,
            Some("reconnect") => Connection::Reconnect,
            _ => Connection::Missing,
        };
        let turn = Turn {
            row,
            default_id: "default",
            user: user.as_ref(),
            member_origins: c.get("origins").as_str().unwrap_or(""),
            machine,
            sign_in: c.get("signIn").truthy(),
            sign_in_enabled: c.get("signInEnabled").truthy(),
            connection,
            workspace_user: opt_text(c.get("workspaceUser")),
            chat_id: opt_text(c.get("chatId")),
            space_id: opt_text(c.get("spaceId")),
        };
        let got = target::resolve(&turn);
        if node.get("throws").truthy() {
            assert_eq!(got, None, "case {i}: Node throws");
            thrown += 1;
            continue;
        }
        if let JValue::Obj(_) = node.get("refused") {
            let r = node.get("refused");
            let want = Refused {
                status: r.get("status").as_num().unwrap() as u16,
                error: static_text(r.get("error").as_str().unwrap()),
            };
            assert_eq!(got, Some(Err(want)), "case {i}: {c:?}");
            refused += 1;
            continue;
        }
        let fetch = match node.get("fetch") {
            JValue::Str(s) if s == "direct" => Fetch::Direct,
            f if f.get("machine").as_str().is_some() => Fetch::Machine {
                id: f.get("machine").as_str().unwrap().to_string(),
            },
            f => Fetch::Broker {
                owner: f.get("broker").as_str().unwrap().to_string(),
                conversation: opt_text(f.get("conversation")).map(str::to_string),
            },
        };
        let key = if matches!(fetch, Fetch::Direct) {
            KeyUse::RowKey
        } else {
            KeyUse::None
        };
        let want = Target {
            provider_id: node.get("providerId").as_str().unwrap().to_string(),
            label: node.get("label").as_str().unwrap().to_string(),
            url: node.get("url").as_str().unwrap().to_string(),
            fetch,
            key,
            member_own: node.get("memberOwn").truthy(),
        };
        assert_eq!(got, Some(Ok(want)), "case {i}: {c:?}");
        ok += 1;
    }
    // Every kind of answer is exercised.
    assert!(
        ok > 1000 && refused > 1000 && thrown == 3,
        "{ok} {refused} {thrown}"
    );
}

#[test]
fn every_recorded_bearer() {
    for h in arr(RECORDED.get("headers")) {
        let key = h.get("apiKey");
        let got = target::authorization(key.as_str());
        assert_eq!(got.as_deref(), h.get("authorization").as_str(), "{key:?}");
    }
}

#[test]
fn every_recorded_refusal() {
    let rows = arr(RECORDED.get("refusals"));
    assert_eq!(rows.len(), 96);
    for r in rows {
        let f = r.get("facts");
        let facts = Facts {
            provider_id: opt_text(f.get("providerId")).map(str::to_string),
            default_provider_id: opt_text(f.get("defaultProviderId")).map(str::to_string),
            machine: f.get("machine").truthy(),
            external: f.get("external").truthy(),
            sign_in: f.get("chatgpt").truthy(),
            route_target: f.get("routeTarget").truthy(),
            authorized: f.get("authorized").truthy(),
        };
        let want = r.get("node").as_str();
        assert_eq!(refusal::node_reason(&facts), want, "{f:?}");
        // Without the caps token a front gets exactly Node's answer.
        assert_eq!(
            refusal::with_caps(&facts, &["skills", "once"]),
            want,
            "{f:?}"
        );
        // With it, only machines, the sign-in provider and route targets stay refused.
        let lifted = refusal::with_caps(&facts, &refusal::caps("skills, providers"));
        let stays = facts.machine || facts.sign_in || facts.route_target;
        assert_eq!(lifted, stays.then_some("provider"), "{f:?}");
    }
}

/// Every recorded target through [`cut::cut`]: Node's refusals and throws are kept, only direct keyed non-default
/// rows of a signed-in user are cut, and a binding made at the cut passes its first re-check under the same
/// facts ([`cut::rows_now`]), while the same turn with no origin approved any more is refused exactly when the
/// row is a member's own (the rule `resolve` applied).
#[test]
fn every_recorded_target_cut_and_rechecked() {
    let targets = arr(RECORDED.get("targets"));
    let (mut cuts, mut origin_refused) = (0, 0);
    for (i, t) in targets.iter().enumerate() {
        let c = t.get("case");
        let node = t.get("node");
        let user = match c.get("user") {
            JValue::Obj(_) => Some(User {
                id: opt_text(c.get("user").get("id")).map(str::to_string),
                admin: c.get("user").get("admin").truthy(),
            }),
            _ => None,
        };
        let machine = opt_text(c.get("machine"));
        let row = if machine.is_some() { node.get("row") } else { c.get("row") };
        let connection = match c.get("connection").as_str() {
            Some("connected") => Connection::Connected,
            Some("reconnect") => Connection::Reconnect,
            _ => Connection::Missing,
        };
        let origins = c.get("origins").as_str().unwrap_or("");
        let turn = Turn {
            row,
            default_id: "default",
            user: user.as_ref(),
            member_origins: origins,
            machine,
            sign_in: c.get("signIn").truthy(),
            sign_in_enabled: c.get("signInEnabled").truthy(),
            connection,
            workspace_user: opt_text(c.get("workspaceUser")),
            chat_id: opt_text(c.get("chatId")),
            space_id: opt_text(c.get("spaceId")),
        };
        let got = cut::cut(&turn, "qa-model", Some("qa-chat"), 1.0e15);
        let resolved = target::resolve(&turn);
        let target = match resolved {
            None => {
                assert_eq!(got, Err(NotCut::Throws), "case {i}");
                continue;
            }
            Some(Err(r)) => {
                assert_eq!(got, Err(NotCut::Node(r)), "case {i}");
                continue;
            }
            Some(Ok(t)) => t,
        };
        let has_user = user.as_ref().and_then(|u| u.id.as_deref()).is_some_and(|s| !s.is_empty());
        if target.provider_id == "default" {
            assert_eq!(got, Err(NotCut::Unsupported(Unsupported::DefaultProvider)), "case {i}");
            continue;
        }
        if target.fetch != Fetch::Direct {
            assert_eq!(got, Err(NotCut::Unsupported(Unsupported::Transport)), "case {i}");
            continue;
        }
        if !has_user {
            assert_eq!(got, Err(NotCut::NoUser), "case {i}");
            continue;
        }
        let made = got.unwrap_or_else(|e| panic!("case {i}: {e:?}"));
        assert_eq!(made.target, target, "case {i}");
        assert_eq!(made.binding.url, target.url);
        assert_eq!(made.binding.fingerprint, binding::fingerprint(row));
        cuts += 1;
        let now = cut::rows_now(user.as_ref(), true, Some(row), "default", origins, &made.binding);
        assert_eq!(now.member_own, target.member_own, "case {i}");
        let check = |n: &chat_upstream::host::RowsNow| {
            binding::recheck(
                &made.binding,
                &binding::Now {
                    user: &n.user,
                    user_active: n.user_active,
                    row: n.row.as_ref(),
                    member_own: n.member_own,
                    origin_approved: n.origin_approved,
                },
            )
        };
        assert_eq!(check(&now), Recheck::Ok, "case {i}: {c:?}");
        let none = cut::rows_now(user.as_ref(), true, Some(row), "default", "", &made.binding);
        let want = if target.member_own { Recheck::OriginRefused } else { Recheck::Ok };
        assert_eq!(check(&none), want, "case {i}");
        if want == Recheck::OriginRefused {
            origin_refused += 1;
        }
        let gone = cut::rows_now(user.as_ref(), true, None, "default", origins, &made.binding);
        assert_eq!(check(&gone), Recheck::RowGone, "case {i}");
        let inactive = cut::rows_now(user.as_ref(), false, Some(row), "default", origins, &made.binding);
        assert_eq!(check(&inactive), Recheck::UserChanged, "case {i}");
    }
    assert!(cuts > 100 && origin_refused > 10, "{cuts} cut, {origin_refused} origin-refused");
}
