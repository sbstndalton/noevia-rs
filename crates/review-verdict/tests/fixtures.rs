//! Every row of tests/fixtures/code-review-verdict.v1.json (printed by noevia-core's
//! tools/gen-code-review-verdict-fixtures.cjs from the JS itself; byte-identical to core's copy)
//! through the library and through the wasm call's wire format: the same verdict, the same
//! ReviewVerdictError message, the same bounded event (-0 included). A row may refuse as
//! `opaque` only where it is marked `stricter` (its input holds an object the port is not shown).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use prompt_framing::json::{self, Value};
use review_verdict::{
    bound_review_event, call, decode, read_verdict, Event, Fault, Finding, Js, Verdict,
};

fn fixtures() -> Value {
    let bytes = include_bytes!("fixtures/code-review-verdict.v1.json");
    json::parse_utf8(bytes, 64).expect("fixture JSON")
}

fn u(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Structural equality with numbers compared bit for bit (so -0 is not 0).
fn same(a: &Js, b: &Js) -> bool {
    match (a, b) {
        (Js::Num(x), Js::Num(y)) => x.to_bits() == y.to_bits(),
        (Js::Arr(x), Js::Arr(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q)),
        (Js::Obj(x), Js::Obj(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|((k, p), (l, q))| k == l && same(p, q))
        }
        _ => a == b,
    }
}

fn finding_js(f: &Finding) -> Js {
    let mut m = vec![(u("severity"), Js::Str(u(f.severity)))];
    if let Some(file) = &f.file {
        m.push((u("file"), Js::Str(file.clone())));
    }
    m.push((u("message"), Js::Str(f.message.clone())));
    Js::Obj(m)
}

fn verdict_pairs(v: &Verdict) -> Vec<(Vec<u16>, Js)> {
    vec![
        (u("verdict"), Js::Str(u(v.verdict))),
        (u("summary"), Js::Str(v.summary.clone())),
        (
            u("findings"),
            Js::Arr(v.findings.iter().map(finding_js).collect()),
        ),
    ]
}

fn opt(s: &Option<Vec<u16>>) -> Js {
    s.as_ref().map_or(Js::Null, |s| Js::Str(s.clone()))
}

fn event_js(e: &Event) -> Js {
    let head = |status: &str, b: &review_verdict::Base| {
        vec![
            (u("status"), Js::Str(u(status))),
            (u("reviewer"), Js::Str(u("planner"))),
            (u("baseSha"), opt(&b.base_sha)),
            (u("headSha"), opt(&b.head_sha)),
        ]
    };
    match e {
        Event::Requested { base, files } => {
            let mut m = head("pending", base);
            m.push((u("files"), files.map_or(Js::Null, Js::Num)));
            Js::Obj(m)
        }
        Event::Failed { base, code, reason } => {
            let mut m = head("failed", base);
            m.push((u("code"), Js::Str(code.clone())));
            m.push((u("reason"), Js::Str(reason.clone())));
            Js::Obj(m)
        }
        Event::Completed {
            base,
            verdict,
            corrected,
        } => {
            let mut m = head("completed", base);
            m.extend(verdict_pairs(verdict));
            m.push((u("corrected"), Js::Bool(*corrected)));
            Js::Obj(m)
        }
    }
}

/// A plain JSON reply value (no tags) as a Js value.
fn plain(v: &Value) -> Js {
    match v {
        Value::Null => Js::Null,
        Value::Bool(b) => Js::Bool(*b),
        Value::Num(n) => Js::Num(*n),
        Value::Str(s) => Js::Str(s.clone()),
        Value::Arr(xs) => Js::Arr(xs.iter().map(plain).collect()),
        Value::Obj(m) => Js::Obj(m.iter().map(|(k, x)| (k.clone(), plain(x))).collect()),
        Value::Deep => panic!("deep reply"),
    }
}

/// Re-encode a Js value in the tagged form the host sends.
fn tag(out: &mut Vec<u8>, v: &Js) {
    match v {
        Js::Undefined => out.extend_from_slice(br#"["u"]"#),
        Js::Null => out.extend_from_slice(b"null"),
        Js::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Js::Num(n) if n.is_nan() => out.extend_from_slice(br#"["n","NaN"]"#),
        Js::Num(n) if n.is_infinite() && *n > 0.0 => out.extend_from_slice(br#"["n","Infinity"]"#),
        Js::Num(n) if n.is_infinite() => out.extend_from_slice(br#"["n","-Infinity"]"#),
        Js::Num(n) if *n == 0.0 && n.is_sign_negative() => out.extend_from_slice(br#"["n","-0"]"#),
        Js::Num(n) => out.extend_from_slice(format!("{n:e}").as_bytes()),
        Js::Str(s) => json::push_str(out, s),
        Js::Func => out.extend_from_slice(br#"["f"]"#),
        Js::Opaque => out.extend_from_slice(br#"["x"]"#),
        Js::Arr(xs) => {
            out.extend_from_slice(br#"["a",["#);
            for (i, x) in xs.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                tag(out, x);
            }
            out.extend_from_slice(b"]]");
        }
        Js::Obj(m) => {
            out.extend_from_slice(br#"["o",["#);
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.push(b'[');
                json::push_str(out, k);
                out.push(b',');
                tag(out, x);
                out.push(b']');
            }
            out.extend_from_slice(b"]]");
        }
    }
}

fn wire(op: u8, body: &[u8]) -> (u32, Js) {
    let mut input = vec![op];
    input.extend_from_slice(body);
    let (status, reply) = call(&input);
    (
        status,
        plain(&json::parse_utf8(reply.as_bytes(), 64).expect("reply JSON")),
    )
}

fn member<'a>(v: &'a Js, key: &str) -> &'a Js {
    let Js::Obj(m) = v else {
        panic!("not an object")
    };
    &m.iter().find(|(k, _)| *k == u(key)).expect(key).1
}

#[test]
fn read_rows() {
    let f = fixtures();
    let Some(Value::Arr(rows)) = f.get("read") else {
        panic!()
    };
    assert!(rows.len() > 150);
    let mut refused = 0;
    for (i, row) in rows.iter().enumerate() {
        let raw = decode(row.get("raw").unwrap()).expect("tagged raw");
        let stricter = row.get("stricter") == Some(&Value::Bool(true));
        let want = row.get("want").unwrap();
        let mut body = Vec::new();
        tag(&mut body, &raw);
        let (status, reply) = wire(1, &body);
        match read_verdict(&raw) {
            Err(Fault::Opaque) => {
                assert!(stricter, "row {i}: refused a row the port is shown in full");
                assert_eq!(status, 1);
                refused += 1;
            }
            Ok(Err(why)) => {
                let Some(Value::Str(msg)) = want.get("error") else {
                    panic!("row {i}: the JS returned a verdict, the port {:?}", why)
                };
                assert_eq!(&u(why.message()), msg, "row {i}");
                assert_eq!(status, 0);
                assert!(same(member(&reply, "invalid"), &Js::Str(u(why.code()))));
            }
            Ok(Ok(v)) => {
                let Some(ok) = want.get("ok") else {
                    panic!(
                        "row {i}: the JS threw {:?}, the port accepted",
                        want.get("error")
                    )
                };
                let want = decode(ok).unwrap();
                assert!(same(&Js::Obj(verdict_pairs(&v)), &want), "row {i}");
                assert_eq!(status, 0);
                assert!(same(member(&reply, "verdict"), &want), "row {i} (wire)");
            }
        }
    }
    assert!(refused > 0);
}

#[test]
fn bound_rows() {
    let f = fixtures();
    let Some(Value::Arr(rows)) = f.get("bound") else {
        panic!()
    };
    assert!(rows.len() > 300);
    for (i, row) in rows.iter().enumerate() {
        let kind = decode(row.get("type").unwrap()).unwrap();
        let data = decode(row.get("data").unwrap()).unwrap();
        let stricter = row.get("stricter") == Some(&Value::Bool(true));
        let want = decode(row.get("want").unwrap()).unwrap();
        let mut body = b"[".to_vec();
        tag(&mut body, &kind);
        body.push(b',');
        tag(&mut body, &data);
        body.push(b']');
        let (status, reply) = wire(2, &body);
        match bound_review_event(&kind, &data) {
            Err(Fault::Opaque) => {
                assert!(stricter, "row {i}");
                assert_eq!(status, 1);
            }
            Ok(e) if stricter && !same(&event_js(&e), &want) => {
                // An opaque finding: the JS's own catch, a failed review, where the JS kept one.
                assert!(
                    matches!(&e, Event::Failed { code, .. } if *code == u("invalid")),
                    "row {i}: {e:?}"
                );
                assert_eq!(status, 0);
            }
            Ok(e) => {
                assert!(same(&event_js(&e), &want), "row {i}: {e:?}");
                assert_eq!(status, 0);
                assert!(same(member(&reply, "event"), &want), "row {i} (wire)");
            }
        }
    }
}
