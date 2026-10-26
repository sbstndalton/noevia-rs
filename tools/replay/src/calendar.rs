//! Opt-in corpus calendar contract. No server clock controls or ignored response fields.
use crate::{at, corpus::Exchange, http};
use chrono::{NaiveDate, Utc};
use serde_json::Value;

pub(crate) fn send_anchored<T>(
    send: impl FnOnce() -> T,
    mut clock: impl FnMut() -> NaiveDate,
) -> (T, NaiveDate, NaiveDate) {
    let before = clock();
    let result = send();
    let after = clock();
    (result, before, after)
}
pub(crate) fn today() -> NaiveDate {
    Utc::now().date_naive()
}

fn date(text: &str) -> Result<NaiveDate, String> {
    let day = NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| "invalid calendar date")?;
    if text.len() != 10 || day.format("%Y-%m-%d").to_string() != text {
        return Err("noncanonical calendar date".into());
    }
    Ok(day)
}
fn window(body: &Value) -> Result<NaiveDate, String> {
    if at(body, "retentionDays").as_u64() != Some(365)
        || at(body, "timeZone").as_str() != Some("UTC")
    {
        return Err("calendar retention/timezone mismatch".into());
    }
    let days = at(body, "days")
        .as_array()
        .ok_or("missing calendar window")?;
    if days.len() != 365 {
        return Err("calendar window must contain 365 days".into());
    }
    let mut previous: Option<NaiveDate> = None;
    for row in days {
        let day = date(at(row, "day").as_str().ok_or("missing day label")?)?;
        if previous.is_some_and(|p| p.succ_opt() != Some(day)) {
            return Err("calendar window is not consecutive and ordered".into());
        }
        previous = Some(day);
    }
    previous.ok_or("empty calendar window".into())
}
fn anchored(day: NaiveDate, before: NaiveDate, after: NaiveDate) -> Result<(), String> {
    if before > after
        || (before != after && before.succ_opt() != Some(after))
        || (day != before && day != after)
    {
        return Err("calendar date is outside independently captured request anchors".into());
    }
    Ok(())
}
const PREFIX: &str = "attachment; filename=\"noevia-conversations-";
const SUFFIX: &str = ".zip\"";
fn export_day(header: &str) -> Result<NaiveDate, String> {
    let value = header
        .strip_prefix(PREFIX)
        .and_then(|s| s.strip_suffix(SUFFIX))
        .ok_or("export disposition/name mismatch")?;
    date(value)
}

/// Adapt only validated date labels. Every counter/other field still receives the normal diff.
pub fn expected(
    ex: &Exchange,
    res: &http::Response,
    before: NaiveDate,
    after: NaiveDate,
) -> Result<Exchange, String> {
    if ex.method() != "GET"
        || at(ex.response(), "status").as_u64() != Some(200)
        || res.status != 200
    {
        return Ok(ex.clone());
    }
    let mut adjusted = ex.clone();
    match ex.path() {
        "/api/usage" | "/api/usage/aggregate" => {
            let actual: Value =
                serde_json::from_slice(&res.body).map_err(|_| "calendar response is not JSON")?;
            let recorded = at(at(ex.response(), "body"), "json");
            window(recorded)?;
            let last = window(&actual)?;
            anchored(last, before, after)?;
            let days = adjusted
                .value
                .get_mut("response")
                .and_then(|v| v.get_mut("body"))
                .and_then(|v| v.get_mut("json"))
                .and_then(|v| v.get_mut("days"))
                .and_then(Value::as_array_mut)
                .ok_or("missing recorded calendar window")?;
            let labels = at(&actual, "days")
                .as_array()
                .ok_or("missing actual calendar window")?;
            for (row, actual_row) in days.iter_mut().zip(labels) {
                let label = at(actual_row, "day").as_str().ok_or("missing actual day")?;
                *row.get_mut("day").ok_or("missing recorded day")? = Value::String(label.into());
            }
        }
        "/api/export/conversations" => {
            let recorded = at(at(ex.response(), "headers"), "content-disposition")
                .as_str()
                .ok_or("missing recorded export disposition")?;
            export_day(recorded)?;
            let headers: Vec<&str> = res
                .headers
                .iter()
                .filter(|(name, _)| name == "content-disposition")
                .map(|(_, v)| v.as_str())
                .collect();
            let actual = match headers.as_slice() {
                [value] => *value,
                _ => return Err("export needs one disposition header".into()),
            };
            anchored(export_day(actual)?, before, after)?;
            let field = adjusted
                .value
                .get_mut("response")
                .and_then(|v| v.get_mut("headers"))
                .and_then(|v| v.get_mut("content-disposition"))
                .ok_or("missing recorded disposition")?;
            *field = Value::String(actual.into());
        }
        _ => {}
    }
    Ok(adjusted)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;
    use std::cell::RefCell;
    #[test]
    fn capture_surrounds_transport_at_midnight() {
        let events = RefCell::new(Vec::new());
        let first = date("2026-12-31").unwrap();
        let second = date("2027-01-01").unwrap();
        let (result, before, after) = send_anchored(
            || {
                events.borrow_mut().push("send");
                7
            },
            || {
                let mut e = events.borrow_mut();
                let day = if e.is_empty() { first } else { second };
                e.push("clock");
                day
            },
        );
        assert_eq!(result, 7);
        assert_eq!((before, after), (first, second));
        assert_eq!(*events.borrow(), vec!["clock", "send", "clock"]);
    }
}

#[cfg(test)]
mod contract_tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;
    use crate::{bind::Bindings, corpus::Corpus, diff, Options};
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Duration,
    };
    fn body(end: &str) -> Value {
        let end = date(end).unwrap();
        let days: Vec<Value> = (0..365).rev().map(|n| json!({"day": (end - chrono::Duration::days(n)).format("%Y-%m-%d").to_string(), "calls": 0})).collect();
        json!({"retentionDays":365,"timeZone":"UTC","days":days,"calls":7,"currentStreak":0,"models":[{"model":"synthetic","calls":7}]})
    }
    fn exchange(path: &str, body: Value) -> Exchange {
        Exchange {
            file: "synthetic.json".into(),
            value: json!({"v":1,"request":{"method":"GET","path":path,"headers":{},"body":{"kind":"empty"}},"response":{"status":200,"headers":{"content-type":"application/json"},"body":{"kind":"json","json":body}}}),
        }
    }
    fn response(body: &Value) -> http::Response {
        http::Response {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: serde_json::to_vec(body).unwrap(),
        }
    }
    #[test]
    fn leap_year_rollover_and_midnight_preserve_every_nondate_value() {
        for (old, new) in [
            ("2026-10-10", "2026-10-11"),
            ("2024-02-28", "2024-02-29"),
            ("2026-12-31", "2027-01-01"),
        ] {
            for path in ["/api/usage", "/api/usage/aggregate"] {
                let ex = exchange(path, body(old));
                let actual = body(new);
                let adjusted = expected(
                    &ex,
                    &response(&actual),
                    date(old).unwrap(),
                    date(new).unwrap(),
                )
                .unwrap();
                assert_eq!(at(at(adjusted.response(), "body"), "json"), &actual);
                for key in ["calls", "currentStreak", "models"] {
                    let mut wrong = actual.clone();
                    wrong[key] = json!(42);
                    let adjusted = expected(
                        &ex,
                        &response(&wrong),
                        date(new).unwrap(),
                        date(new).unwrap(),
                    )
                    .unwrap();
                    let mut differences = Vec::new();
                    diff::json(
                        at(at(adjusted.response(), "body"), "json"),
                        &wrong,
                        "body",
                        &mut Bindings::default(),
                        &mut differences,
                    );
                    assert!(!differences.is_empty(), "nondate {key} must remain strict");
                }
            }
        }
    }
    #[test]
    fn invalid_windows_and_unanchored_dates_are_refused() {
        let ex = exchange("/api/usage", body("2026-10-10"));
        let valid = body("2026-10-11");
        let anchor = date("2026-10-11").unwrap();
        let mut bad = Vec::new();
        for label in ["2026-02-30", "2026-1-01", "2026-10-11"] {
            let mut v = valid.clone();
            v["days"][1]["day"] = json!(label);
            bad.push(v);
        }
        let mut v = valid.clone();
        v["days"].as_array_mut().unwrap().swap(1, 2);
        bad.push(v);
        let mut v = valid.clone();
        v["days"].as_array_mut().unwrap().pop();
        bad.push(v);
        let mut v = valid.clone();
        v["days"]
            .as_array_mut()
            .unwrap()
            .push(json!({"day":"2026-10-12","calls":0}));
        bad.push(v);
        for (k, val) in [
            ("retentionDays", json!(364)),
            ("timeZone", json!("Europe/Berlin")),
        ] {
            let mut v = valid.clone();
            v[k] = val;
            bad.push(v);
        }
        bad.push(body("2026-10-10"));
        bad.push(body("2026-10-12"));
        for v in bad {
            assert!(expected(&ex, &response(&v), anchor, anchor).is_err());
        }
        assert!(expected(&ex, &response(&valid), anchor, date("2026-10-13").unwrap()).is_err());
        assert!(expected(&ex, &response(&valid), date("2026-10-12").unwrap(), anchor).is_err());
        let mut wrong_recorded = ex.clone();
        wrong_recorded.value["response"]["body"]["json"]["days"][1]["day"] = json!("2026-02-30");
        assert!(expected(&wrong_recorded, &response(&valid), anchor, anchor).is_err());
    }
    #[test]
    fn export_validates_full_filename_and_exact_successful_get_path() {
        let mut ex = exchange("/api/export/conversations", Value::Null);
        ex.value["response"]["headers"]["content-disposition"] =
            json!("attachment; filename=\"noevia-conversations-2026-10-10.zip\"");
        let day = date("2026-10-11").unwrap();
        let mut res = http::Response {
            status: 200,
            headers: vec![(
                "content-disposition".into(),
                "attachment; filename=\"noevia-conversations-2026-10-11.zip\"".into(),
            )],
            body: Vec::new(),
        };
        assert!(expected(&ex, &res, day, day).is_ok());
        let adjusted = expected(&ex, &res, day, day).unwrap();
        let mut wrong_headers = res.headers.clone();
        wrong_headers.push(("content-type".into(), "application/octet-stream".into()));
        let mut differences = Vec::new();
        diff::headers(
            at(adjusted.response(), "headers").as_object().unwrap(),
            &wrong_headers,
            &[],
            &mut Bindings::default(),
            &mut differences,
        );
        assert!(differences.iter().any(|d| d.at == "header/content-type"));
        let mut duplicated = res.clone();
        duplicated.headers.push(duplicated.headers[0].clone());
        assert!(expected(&ex, &duplicated, day, day).is_err());

        for value in [
            "attachment; filename=\"noevia-conversations-2026-10-12.zip\"",
            "inline; filename=\"noevia-conversations-2026-10-11.zip\"",
            "attachment; filename=\"noevia-conversations-2026-10-11.tar\"",
            "attachment; filename=\"noevia-conversations-2026-02-30.zip\"",
        ] {
            res.headers[0].1 = value.into();
            assert!(expected(&ex, &res, day, day).is_err());
        }
        for path in ["/api/export/conversations?x=1", "/api/usage/other"] {
            let mut other = ex.clone();
            other.value["request"]["path"] = json!(path);
            assert_eq!(expected(&other, &res, day, day).unwrap().value, other.value);
        }
        ex.value["request"]["method"] = json!("POST");
        assert_eq!(expected(&ex, &res, day, day).unwrap().value, ex.value);
        ex.value["request"]["method"] = json!("GET");
        res.status = 403;
        assert_eq!(expected(&ex, &res, day, day).unwrap().value, ex.value);
    }
    // Actual transport integration: old recording vs today's response only passes with explicit opt-in.
    #[test]
    fn actual_http_wiring_requires_manifest_opt_in_and_refuses_bad_anchor() {
        for (enabled, offset, clean) in [(true, 0, true), (false, 0, false), (true, 2, false)] {
            let actual = body(
                &(today() + chrono::Duration::days(offset))
                    .format("%Y-%m-%d")
                    .to_string(),
            );
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut input = [0; 4096];
                let received = stream.read(&mut input).unwrap();
                assert!(received > 0);
                let out = serde_json::to_string(&actual).unwrap();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",out.len(),out).unwrap();
            });
            let ex = exchange(
                "/api/usage",
                body(
                    &(today() - chrono::Duration::days(1))
                        .format("%Y-%m-%d")
                        .to_string(),
                ),
            );
            let corpus = Corpus {
                utc_calendar: enabled,
                ..Corpus::default()
            };
            let opts = Options {
                native_move_state: false,
                base: http::Base {
                    host: "127.0.0.1".into(),
                    port,
                },
                timeout: Duration::from_secs(2),
                ignore_headers: vec![],
                ignore_body: vec![],
                data_dir: None,
                now: "0".into(),
            };
            let result = crate::replay_one(&ex, &corpus, &opts, &mut Bindings::default());
            server.join().unwrap();
            assert_eq!(
                result.clean(),
                clean,
                "{enabled}/{offset}: {:?}",
                result.diffs
            );
        }
    }
}
