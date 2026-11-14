//! core's `server/mcp-internal.test.cjs`, case by case, plus the listener rule and fixed token bytes
//! (recorded independently: HMAC-SHA256 and unpadded base64url over the payload JSON core writes).
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use js_json::JValue;
use mcp_internal::*;

const KEY: &[u8] = b"qa-key-0123456789abcdef0123456789";
const OTHER_KEY: &[u8] = b"qa-key-other-0123456789abcdef0123";
const NOW: f64 = 1_700_000_000_000.0;

fn defs() -> Vec<Definition> {
    vec![
        Definition {
            name: "demo_read".into(),
            description: Some("Read something.".into()),
            schema: Some(
                js_json::parse(
                    r#"{"type":"object","properties":{"q":{"type":"string"}},"required":[]}"#,
                )
                .unwrap(),
            ),
            write: false,
        },
        Definition {
            name: "demo_write".into(),
            description: Some("Write something.".into()),
            schema: Some(
                js_json::parse(
                    r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#,
                )
                .unwrap(),
            ),
            write: true,
        },
    ]
}

fn s(v: &JValue) -> String {
    js_json::to_js_string(v).unwrap()
}

/// The two DEFS handlers: `read ${args.q || ''} as ${ctx.userId}/${ctx.projectId}`, `wrote ${args.text} as ${ctx.userId}`.
fn demo(inv: &Invoke) -> Result<Option<String>, String> {
    Ok(Some(match inv.name.as_str() {
        "demo_read" => {
            let q = inv.args.get("q");
            let q = if q.truthy() { s(q) } else { String::new() };
            format!(
                "read {q} as {}/{}",
                s(&inv.ctx.user_id),
                s(&inv.ctx.project_id)
            )
        }
        "demo_write" => format!(
            "wrote {} as {}",
            s(inv.args.get("text")),
            s(&inv.ctx.user_id)
        ),
        other => panic!("{other}"),
    }))
}

fn finish(step: Step, tool: impl Fn(&Invoke) -> Result<Option<String>, String>) -> Reply {
    match step {
        Step::Reply(r) => r,
        Step::Invoke(inv) => inv.finish(tool(&inv)),
    }
}

fn call(name: &str, args: &str) -> JValue {
    js_json::parse(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#
    ))
    .unwrap()
}

fn token(m: Mint<'_>, jti: &str) -> String {
    mint(KEY, &m, jti, NOW)
}

fn user(uid: &str) -> Mint<'_> {
    Mint {
        uid: Some(uid),
        ..Mint::default()
    }
}

fn text(r: &Reply) -> String {
    r.body
        .as_ref()
        .unwrap()
        .get("result")
        .get("content")
        .clone_first_text()
}

trait FirstText {
    fn clone_first_text(&self) -> String;
}
impl FirstText for JValue {
    fn clone_first_text(&self) -> String {
        match self {
            JValue::Arr(items) => items[0].get("text").as_str().unwrap().to_owned(),
            other => panic!("{other:?}"),
        }
    }
}

fn error_message(r: &Reply) -> String {
    r.body
        .as_ref()
        .unwrap()
        .get("error")
        .get("message")
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn a_token_proves_who_the_call_acts_for_and_nothing_else_can_set_that() {
    let h = Handler::new(KEY, defs());
    let t = token(
        Mint {
            uid: Some("alice"),
            pid: Some("proj-a"),
            ..Mint::default()
        },
        "j1",
    );
    let out = finish(
        h.handle(
            &call(
                "demo_read",
                r#"{"q":"x","userId":"bob","projectId":"proj-b","tenant":"bob"}"#,
            ),
            &t,
            NOW,
        ),
        demo,
    );
    assert_eq!(out.status, 200);
    assert!(text(&out).ends_with("as alice/proj-a"), "{}", text(&out));
}

#[test]
fn tampered_wrong_key_expired_and_malformed_tokens_are_all_rejected() {
    let h = Handler::new(KEY, defs());
    let good = token(user("alice"), "j-good");
    let (body, sig) = good.split_once('.').unwrap();
    let claims = js_json::parse(&String::from_utf8(b64url_decode_lenient(body)).unwrap()).unwrap();
    let JValue::Obj(mut pairs) = claims else {
        panic!()
    };
    for (k, v) in pairs.iter_mut() {
        if k == "uid" {
            *v = JValue::from("bob");
        }
    }
    let swapped = b64url(js_json::stringify(&JValue::Obj(pairs)).unwrap().as_bytes());
    let bad = [
        format!("{swapped}.{sig}"),
        mint(OTHER_KEY, &user("alice"), "j-other", NOW),
        mint(
            KEY,
            &Mint {
                uid: Some("alice"),
                ttl_ms: Some(-1.0),
                ..Mint::default()
            },
            "j-expired",
            NOW,
        ),
        "not-a-token".into(),
        String::new(),
        format!("{body}."),
        format!("{body}.{sig}.{sig}"),
    ];
    let reasons: Vec<_> = bad
        .iter()
        .map(|b| {
            let r = finish(h.handle(&call("demo_read", "{}"), b, NOW), demo);
            assert_eq!(r.status, 401, "accepted {b:.24}");
            error_message(&r)
        })
        .collect();
    assert_eq!(
        reasons,
        [
            "unauthorized: bad signature",
            "unauthorized: bad signature",
            "unauthorized: expired",
            "unauthorized: malformed",
            "unauthorized: malformed",
            "unauthorized: bad signature",
            "unauthorized: malformed",
        ]
    );
    // ...and the genuine one still works, so the test is not passing vacuously.
    assert_eq!(
        finish(h.handle(&call("demo_read", "{}"), &good, NOW), demo).status,
        200
    );
}

#[test]
fn a_write_presented_without_the_approved_capability_fails_closed() {
    let h = Handler::new(KEY, defs());
    let out = finish(
        h.handle(
            &call("demo_write", r#"{"text":"hi"}"#),
            &token(user("alice"), "j1"),
            NOW,
        ),
        demo,
    );
    assert_eq!(out.status, 403);
    assert_eq!(
        error_message(&out),
        "\"demo_write\" changes data and was not approved"
    );
    let approved = token(
        Mint {
            uid: Some("alice"),
            write: true,
            ..Mint::default()
        },
        "j2",
    );
    let ok = finish(
        h.handle(&call("demo_write", r#"{"text":"hi"}"#), &approved, NOW),
        demo,
    );
    assert_eq!(ok.status, 200);
    assert_eq!(text(&ok), "wrote hi as alice");
}

#[test]
fn a_discovery_token_lists_tools_but_can_never_call_one() {
    let h = Handler::new(KEY, defs());
    let discovery = token(
        Mint {
            discovery: true,
            ..Mint::default()
        },
        "j1",
    );
    let listed = finish(
        h.handle(
            &js_json::parse(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap(),
            &discovery,
            NOW,
        ),
        demo,
    );
    assert_eq!(listed.status, 200);
    assert_eq!(
        listed.payload(),
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":["#,
            r#"{"name":"demo_read","description":"Read something.","inputSchema":{"type":"object","properties":{"q":{"type":"string"}},"required":[]},"annotations":{"readOnlyHint":true}},"#,
            r#"{"name":"demo_write","description":"Write something.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]},"annotations":{"readOnlyHint":false}}"#,
            "]}}"
        )
    );
    let denied = finish(h.handle(&call("demo_read", "{}"), &discovery, NOW), demo);
    assert_eq!(denied.status, 403);
    assert_eq!(
        error_message(&denied),
        "this token may list tools but not call them"
    );
    // Even a write capability on a discovery token does not unlock calling.
    let armed = token(
        Mint {
            discovery: true,
            write: true,
            ..Mint::default()
        },
        "j2",
    );
    assert_eq!(
        finish(
            h.handle(&call("demo_write", r#"{"text":"x"}"#), &armed, NOW),
            demo
        )
        .status,
        403
    );
}

#[test]
fn a_captured_token_cannot_repeat_the_call_but_still_completes_its_own_session() {
    let h = Handler::new(KEY, defs());
    let t = token(
        Mint {
            uid: Some("alice"),
            write: true,
            ..Mint::default()
        },
        "j-once",
    );
    let init = finish(
        h.handle(
            &js_json::parse(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#).unwrap(),
            &t,
            NOW,
        ),
        demo,
    );
    assert_eq!(init.status, 200);
    assert_eq!(
        init.payload(),
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"noevia-internal","version":"1"}}}"#
    );
    let note = h.handle(
        &js_json::parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap(),
        &t,
        NOW,
    );
    assert_eq!(
        note,
        Step::Reply(Reply {
            status: 202,
            body: None
        })
    );
    assert_eq!(
        finish(
            h.handle(
                &js_json::parse(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap(),
                &t,
                NOW
            ),
            demo
        )
        .status,
        200
    );
    assert_eq!(h.guard().size(), 0);
    assert_eq!(
        finish(
            h.handle(&call("demo_write", r#"{"text":"once"}"#), &t, NOW),
            demo
        )
        .status,
        200
    );
    let replay = finish(
        h.handle(&call("demo_write", r#"{"text":"twice"}"#), &t, NOW),
        demo,
    );
    assert_eq!(replay.status, 401);
    assert_eq!(
        error_message(&replay),
        "this authorization has already been used"
    );
}

#[test]
fn a_handler_that_throws_becomes_a_tool_error_not_a_dead_stream() {
    let h = Handler::new(
        KEY,
        vec![Definition {
            name: "boom".into(),
            description: Some("x".into()),
            schema: None,
            write: false,
        }],
    );
    let out = finish(
        h.handle(&call("boom", "{}"), &token(user("alice"), "j1"), NOW),
        |_| Err("nope".into()),
    );
    assert_eq!(out.status, 200);
    assert_eq!(
        out.body.as_ref().unwrap().get("result").get("isError"),
        &JValue::Bool(true)
    );
    assert_eq!(text(&out), "ERROR: nope");
    // The message is cut to 300 units, as `.slice(0, 300)`.
    let long = finish(
        h.handle(&call("boom", "{}"), &token(user("alice"), "j2"), NOW),
        |_| Err("x".repeat(400)),
    );
    assert_eq!(text(&long), format!("ERROR: {}", "x".repeat(300)));
    // A handler answering null or undefined is an empty text, not an error.
    let empty = finish(
        h.handle(&call("boom", "{}"), &token(user("alice"), "j3"), NOW),
        |_| Ok(None),
    );
    assert_eq!(
        empty.payload(),
        r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":""}]}}"#
    );
}

#[test]
fn an_unknown_tool_is_a_tool_error_the_model_can_read_not_a_protocol_failure() {
    let h = Handler::new(KEY, defs());
    let out = finish(
        h.handle(
            &call("no_such_tool", "{}"),
            &token(user("alice"), "j1"),
            NOW,
        ),
        demo,
    );
    assert_eq!(out.status, 200);
    assert_eq!(
        out.payload(),
        r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"ERROR: unknown tool \"no_such_tool\""}],"isError":true}}"#
    );
    // An unknown tool does not burn the token (the guard runs after the definition is found).
    assert_eq!(h.guard().size(), 0);
}

#[test]
fn every_catalogue_entry_reports_read_only_as_the_inverse_of_write_and_a_default_schema() {
    // mcp.convertTool/readOnlyHint are core's mcp.cjs (mcp-client in Rust); here the entry shape.
    let mut d = defs();
    d.push(Definition {
        name: "bare".into(),
        description: None,
        schema: None,
        write: false,
    });
    let cat = catalogue_of(&d);
    for (entry, def) in cat.iter().zip(&d) {
        assert_eq!(
            entry.get("annotations").get("readOnlyHint").as_bool(),
            Some(!def.write)
        );
    }
    assert_eq!(
        js_json::stringify(&cat[2]).unwrap(),
        r#"{"name":"bare","inputSchema":{"type":"object","properties":{},"required":[]},"annotations":{"readOnlyHint":true}}"#
    );
}

#[test]
fn the_scope_a_handler_runs_in_comes_from_the_token_not_from_ambient_state() {
    // runAs(uid, fn) is the caller's: the Invoke carries the verified uid, never an argument.
    let h = Handler::new(KEY, defs());
    let Step::Invoke(inv) = h.handle(
        &call("demo_read", r#"{"userId":"mallory"}"#),
        &token(
            Mint {
                uid: Some("alice"),
                pid: Some("proj-a"),
                cid: Some("chat-a"),
                ..Mint::default()
            },
            "j1",
        ),
        NOW,
    ) else {
        panic!()
    };
    assert_eq!(
        inv.ctx,
        Ctx {
            user_id: JValue::from("alice"),
            project_id: JValue::from("proj-a"),
            chat_id: JValue::from("chat-a"),
            edit_target: None,
        }
    );
    assert_eq!(inv.args, js_json::parse(r#"{"userId":"mallory"}"#).unwrap());
}

#[test]
fn a_token_for_an_account_that_no_longer_exists_cannot_act() {
    let h = Handler::new(KEY, defs());
    let out = finish(
        h.handle(&call("demo_read", "{}"), &token(user("deleted"), "j1"), NOW),
        |_| Err("that account no longer exists".into()),
    );
    assert_eq!(
        out.body.as_ref().unwrap().get("result").get("isError"),
        &JValue::Bool(true)
    );
    assert_eq!(text(&out), "ERROR: that account no longer exists");
}

// ── beyond the Node test file: the token bytes, the claims rules, the listener ──

#[test]
fn minted_tokens_are_byte_equal_to_independently_recorded_ones() {
    let key: Vec<u8> = (0u8..32).collect();
    assert_eq!(
        mint(
            &key,
            &Mint {
                uid: Some("alice"),
                pid: Some("proj-a"),
                write: true,
                target: Some("abc"),
                ..Mint::default()
            },
            "qa-jti-1",
            NOW
        ),
        "eyJ2IjoxLCJ1aWQiOiJhbGljZSIsInBpZCI6InByb2otYSIsImNpZCI6bnVsbCwianRpIjoicWEtanRpLTEiLCJleHAiOjE3MDAwMDAwMzAwMDAsInciOjEsImQiOjAsInQiOiJhYmMifQ.AXogm2bxoM9ltTdy7tKOE3Z4o8yrj74rGPbesCL5nmQ"
    );
    assert_eq!(
        mint(
            &key,
            &Mint {
                uid: Some(""),
                discovery: true,
                target: Some(""),
                ..Mint::default()
            },
            "qa-jti-2",
            NOW
        ),
        "eyJ2IjoxLCJ1aWQiOm51bGwsInBpZCI6bnVsbCwiY2lkIjpudWxsLCJqdGkiOiJxYS1qdGktMiIsImV4cCI6MTcwMDAwMDAzMDAwMCwidyI6MCwiZCI6MX0.CpWr9aQXFRn53ri2YO4wX_Dm-2bM1X55WyzJeFwiB0E"
    );
}

/// A token over a hand-written payload, signed with KEY.
fn signed(payload: &str) -> String {
    let body = b64url(payload.as_bytes());
    format!("{body}.{}", b64url(&hmac_sha256(KEY, body.as_bytes())))
}

#[test]
fn verify_applies_cores_claims_rules_in_order() {
    let cases: [(&str, Result<(), Refused>); 10] = [
        (r#"{"v":1,"exp":1700000000001,"jti":"a"}"#, Ok(())),
        (
            r#"{"v":"1","exp":1700000000001,"jti":"a"}"#,
            Err(Refused::Version),
        ),
        (
            r#"{"v":2,"exp":1700000000001,"jti":"a"}"#,
            Err(Refused::Version),
        ),
        ("0", Err(Refused::Version)),
        ("[1]", Err(Refused::Version)),
        (
            r#"{"v":1,"exp":1700000000000,"jti":"a"}"#,
            Err(Refused::Expired),
        ),
        (
            r#"{"v":1,"exp":"9999999999999","jti":"a"}"#,
            Err(Refused::Expired),
        ),
        (
            r#"{"v":1,"exp":1700000000001,"jti":""}"#,
            Err(Refused::Malformed),
        ),
        (
            r#"{"v":1,"exp":1700000000001,"jti":7}"#,
            Err(Refused::Malformed),
        ),
        ("{nope", Err(Refused::Malformed)),
    ];
    for (payload, want) in cases {
        assert_eq!(
            verify(KEY, &signed(payload), NOW).map(|_| ()),
            want,
            "{payload}"
        );
    }
    // Over 4096 units is malformed before anything else.
    let long = format!("{}.{}", "a".repeat(4000), "b".repeat(97));
    assert_eq!(verify(KEY, &long, NOW), Err(Refused::Malformed));
    // Node's base64url reading: a padded or standard-alphabet copy of the signature still verifies.
    let good = signed(r#"{"v":1,"exp":1700000000001,"jti":"a"}"#);
    let (body, sig) = good.split_once('.').unwrap();
    for alt in [format!("{sig}="), sig.replace('-', "+").replace('_', "/")] {
        assert!(verify(KEY, &format!("{body}.{alt}"), NOW).is_ok());
    }
}

#[test]
fn the_guard_forgets_expired_entries_only_past_4096() {
    let g = ReplayGuard::new();
    let claims = |jti: &str, exp: f64| {
        Claims(JValue::obj([
            ("jti", JValue::from(jti)),
            ("exp", JValue::Num(exp)),
        ]))
    };
    for i in 0..=4096 {
        assert!(g.consume(&claims(&format!("j{i}"), NOW - 1.0), NOW));
    }
    assert_eq!(g.size(), 4097);
    // 4097 > 4096: the expired ones go before this one is checked, so an old jti could run again.
    assert!(g.consume(&claims("fresh", NOW + 30_000.0), NOW));
    assert_eq!(g.size(), 1);
    assert!(!g.consume(&claims("fresh", NOW + 30_000.0), NOW));
}

#[test]
fn ids_methods_params_and_the_edit_target_are_read_as_core_reads_them() {
    let h = Handler::new(KEY, defs());
    let t = || {
        token(
            Mint {
                uid: Some("alice"),
                write: true,
                target: Some("digest-1"),
                ..Mint::default()
            },
            "j-x",
        )
    };
    // A missing id is null; an unknown or non-string method is "method not found" with String(method).
    for (msg, want) in [
        (
            r#"{"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32601,"message":"method not found: ping"}}"#,
        ),
        (
            r#"{"id":"a","method":["x","y"]}"#,
            r#"{"jsonrpc":"2.0","id":"a","error":{"code":-32601,"message":"method not found: x,y"}}"#,
        ),
        (
            r#"{"id":3}"#,
            r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"method not found: undefined"}}"#,
        ),
        (
            "\"tools/list\"",
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32601,"message":"method not found: undefined"}}"#,
        ),
    ] {
        let r = finish(h.handle(&js_json::parse(msg).unwrap(), &t(), NOW), demo);
        assert_eq!((r.status, r.payload().as_str()), (200, want), "{msg}");
    }
    // params falsy or not an object: no name, so an unknown tool "".
    for msg in [
        r#"{"id":1,"method":"tools/call"}"#,
        r#"{"id":1,"method":"tools/call","params":0}"#,
        r#"{"id":1,"method":"tools/call","params":{"name":7}}"#,
    ] {
        let r = finish(h.handle(&js_json::parse(msg).unwrap(), &t(), NOW), demo);
        assert_eq!(text(&r), "ERROR: unknown tool \"\"", "{msg}");
    }
    // Arguments that are not an object are {}; t reaches the call as its edit target.
    let Step::Invoke(inv) = h.handle(
        &js_json::parse(
            r#"{"id":1,"method":"tools/call","params":{"name":"demo_write","arguments":[1]}}"#,
        )
        .unwrap(),
        &t(),
        NOW,
    ) else {
        panic!()
    };
    assert_eq!(inv.args, JValue::Obj(vec![]));
    assert_eq!(inv.ctx.edit_target.as_deref(), Some("digest-1"));
    assert_eq!(inv.ctx.project_id, JValue::Null);
    // A token with no user may not call.
    let r = finish(
        h.handle(
            &call("demo_read", "{}"),
            &token(Mint::default(), "j-n"),
            NOW,
        ),
        demo,
    );
    assert_eq!(
        (r.status, error_message(&r).as_str()),
        (403, "no user is in scope for this call")
    );
}

#[test]
fn the_listener_takes_one_post_path_a_capped_json_body_and_a_bearer_token() {
    let h = Handler::new(KEY, defs());
    let t = format!("Bearer {}", token(user("alice"), "j-l"));
    let init = br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
    let not_found = r#"{"error":"not found"}"#;
    for (method, url) in [("GET", "/mcp"), ("POST", "/other"), ("POST", "/mcp/")] {
        let Step::Reply(r) = h.listen(method, url, RPC_PATH, Some(init), Some(&t), NOW) else {
            panic!()
        };
        assert_eq!((r.status, r.payload().as_str()), (404, not_found));
    }
    // The query is ignored.
    let Step::Reply(r) = h.listen("POST", "/mcp?x=1", RPC_PATH, Some(init), Some(&t), NOW) else {
        panic!()
    };
    assert_eq!(r.status, 200);
    // Not JSON: 400 parse error; too large or unreadable: 400 invalid request.
    let Step::Reply(r) = h.listen("POST", "/mcp", RPC_PATH, Some(b"{"), Some(&t), NOW) else {
        panic!()
    };
    assert_eq!(
        (r.status, r.payload().as_str()),
        (
            400,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#
        )
    );
    let big = vec![b' '; MAX_BODY_BYTES + 1];
    for body in [Some(big.as_slice()), None] {
        let Step::Reply(r) = h.listen("POST", "/mcp", RPC_PATH, body, Some(&t), NOW) else {
            panic!()
        };
        assert_eq!(
            (r.status, r.payload().as_str()),
            (
                400,
                r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"invalid request"}}"#
            )
        );
    }
    // Exactly the cap is read.
    let mut at_cap = init.to_vec();
    at_cap.resize(MAX_BODY_BYTES, b' ');
    let Step::Reply(r) = h.listen("POST", "/mcp", RPC_PATH, Some(&at_cap), Some(&t), NOW) else {
        panic!()
    };
    assert_eq!(r.status, 200);
    // No "Bearer " prefix: no token.
    for auth in [
        None,
        Some("bearer x"),
        Some(t.trim_start_matches("Bearer ")),
    ] {
        let Step::Reply(r) = h.listen("POST", "/mcp", RPC_PATH, Some(init), auth, NOW) else {
            panic!()
        };
        assert_eq!(r.status, 401, "{auth:?}");
    }
}
