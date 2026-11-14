//! Replays tests/fixtures/current-time-node.v1.json (tools/gen-fixtures.cjs, noevia-core's own code
//! under Node 22): every tool call, every calendarFacts/factsLine row and the zone table's offsets.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::JValue;

const FIXTURE: &str = include_str!("fixtures/current-time-node.v1.json");
const ZONES_RS: &str = include_str!("../src/zones.rs");

/// Zones Node accepts that have no chrono-tz zone: this port refuses them.
const NODE_ONLY: [&str; 13] = [
    "SystemV/AST4",
    "SystemV/AST4ADT",
    "SystemV/CST6",
    "SystemV/CST6CDT",
    "SystemV/EST5",
    "SystemV/EST5EDT",
    "SystemV/HST10",
    "SystemV/MST7",
    "SystemV/MST7MDT",
    "SystemV/PST8",
    "SystemV/PST8PDT",
    "SystemV/YST9",
    "SystemV/YST9YDT",
];
/// chrono-tz zones Node refuses: none (`Etc/Unknown` and `Factory`, which Node refuses, are not chrono-tz zones).
const CHRONO_ONLY: [&str; 0] = [];
/// Tool calls where this port's text is not Node's: "<process TZ> <timezone argument> <now>", by
/// reason (see the crate's documentation).
const CALLS_DIFFER: &[&str] = &[
    // The tz releases: Morocco at +00 from 2026-09-20T01:00Z in Node, +01 here...
    "UTC Africa/Casablanca 1789866000000",
    "UTC Africa/Casablanca 1793523599999",
    "UTC Africa/Casablanca 1793523600000",
    "UTC Africa/Casablanca 1794636000000",
    "UTC Africa/El_Aaiun 1789866000000",
    "UTC Africa/El_Aaiun 1793523599999",
    "UTC Africa/El_Aaiun 1793523600000",
    "UTC Africa/El_Aaiun 1794636000000",
    // ...and British Columbia and Alberta on daylight time after 2026-11-01 (standard time here).
    "UTC America/Vancouver 1793523600000",
    "UTC America/Vancouver 1794636000000",
    "UTC America/Edmonton 1793523599999",
    "UTC America/Edmonton 1793523600000",
    "UTC America/Edmonton 1794636000000",
    "UTC Canada/Pacific 1793523600000",
    "UTC Canada/Pacific 1794636000000",
    "UTC Canada/Mountain 1793523599999",
    "UTC Canada/Mountain 1793523600000",
    "UTC Canada/Mountain 1794636000000",
    // Zone names in history (1943, 1970) and past ICU's metazone data (year 275000): Node prints the
    // GMT form where this port prints the 2026 name for the offset, or the reverse (London 1943 BST).
    "UTC Europe/Berlin -838987200000",
    "UTC Europe/London -838987200000",
    "UTC Europe/London 13089600000",
    "UTC Europe/Berlin 8615994667200000",
    "UTC Europe/London 8615994667200000",
    "UTC Europe/Berlin 8640000000000000",
    // Date#toString's daylight name for a zone without daylight time in 2026 (Moscow, 1995 and the
    // equivalent year 2008-2010 V8 uses for year 275760): Node "Moscow Summer Time", here the
    // standard name.
    "Europe/Moscow - 804600000000",
    "Europe/Moscow - 8640000000000000",
];
/// (zone, first day, last day) at 00:00 UTC in 2026-2027 where chrono-tz's offset is not Node's.
const DAILY_DIFFER: &[(&str, &str, &str)] = &[
    ("Africa/Casablanca", "2026-09-21", "2027-02-07"),
    ("Africa/Casablanca", "2027-03-15", "2027-12-31"),
    ("Africa/El_Aaiun", "2026-09-21", "2027-02-07"),
    ("Africa/El_Aaiun", "2027-03-15", "2027-12-31"),
    ("America/Edmonton", "2026-11-02", "2027-03-14"),
    ("America/Edmonton", "2027-11-08", "2027-12-31"),
    ("America/Vancouver", "2026-11-02", "2027-03-14"),
    ("America/Vancouver", "2027-11-08", "2027-12-31"),
    ("America/Yellowknife", "2026-11-02", "2027-03-14"),
    ("America/Yellowknife", "2027-11-08", "2027-12-31"),
    ("Canada/Mountain", "2026-11-02", "2027-03-14"),
    ("Canada/Mountain", "2027-11-08", "2027-12-31"),
    ("Canada/Pacific", "2026-11-02", "2027-03-14"),
    ("Canada/Pacific", "2027-11-08", "2027-12-31"),
    // Moldova's transition hour: on these days 00:00 UTC falls between the two releases' times.
    ("Europe/Chisinau", "2026-03-29", "2026-03-29"),
    ("Europe/Chisinau", "2026-10-25", "2026-10-25"),
    ("Europe/Chisinau", "2027-03-28", "2027-03-28"),
    ("Europe/Chisinau", "2027-10-31", "2027-10-31"),
    ("Europe/Tiraspol", "2026-03-29", "2026-03-29"),
    ("Europe/Tiraspol", "2026-10-25", "2026-10-25"),
    ("Europe/Tiraspol", "2027-03-28", "2027-03-28"),
    ("Europe/Tiraspol", "2027-10-31", "2027-10-31"),
];

fn fixture() -> JValue {
    js_json::parse(FIXTURE).expect("fixture parses")
}

fn strs(v: &JValue) -> Vec<String> {
    match v {
        JValue::Arr(a) => a.iter().map(|x| x.as_str().unwrap().to_string()).collect(),
        _ => panic!("not an array"),
    }
}

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        _ => panic!("not an array"),
    }
}

#[test]
fn every_tool_call_matches_node() {
    let f = fixture();
    let (mut same, mut refused, mut wrong) = (0, 0, Vec::new());
    for c in arr(f.get("cases")) {
        let args = c.get("args");
        let now = c.get("now").as_num().unwrap();
        let server = c.get("serverZone").as_str().unwrap();
        let got = current_time::run(args, now, server);
        let zone = args.get("timezone").as_str().unwrap_or("");
        if NODE_ONLY.iter().any(|z| z.eq_ignore_ascii_case(zone)) {
            assert_eq!(got, Err(format!("ERROR: unknown IANA timezone \"{zone}\"")));
            refused += 1;
            continue;
        }
        let failed = c.get("failed").as_bool().unwrap();
        let (ok, text) = match &got {
            Ok(t) => (true, t),
            Err(t) => (false, t),
        };
        let matches = ok != failed
            && match c.get("text").as_str() {
                Some(want) => text == want,
                // Node's text holds a lone surrogate (kept as JSON): this port drops it.
                None => {
                    let want = c.get("textJson").as_str().unwrap().replace("\\ud83d", "");
                    js_json::stringify(&JValue::Str(text.clone())).unwrap() == want
                }
            };
        if matches {
            same += 1;
        } else {
            let process = c.get("processTZ").as_str().unwrap();
            let shown = if zone.is_empty() { "-" } else { zone };
            wrong.push(format!("{process} {shown} {now}"));
        }
    }
    assert_eq!(wrong, CALLS_DIFFER, "tool calls that differ from Node");
    assert!(same > 2500, "{same} cases");
    assert!(refused > 0);
}

#[test]
fn calendar_facts_and_facts_line_match_node() {
    let f = fixture();
    let rows = arr(f.get("facts"));
    assert!(rows.len() > 200);
    for r in rows {
        let (now, zone) = (
            r.get("now").as_num().unwrap(),
            r.get("zone").as_str().unwrap(),
        );
        let got = current_time::calendar_facts(now, zone).unwrap();
        let w = r.get("facts");
        let n = |k: &str| w.get(k).as_num().unwrap() as i64;
        let want = current_time::CalendarFacts {
            year: n("year"),
            month: n("month"),
            day: n("day"),
            weekday_number: n("weekdayNumber"),
            iso_year: n("isoYear"),
            iso_week: n("isoWeek"),
            iso_week1_starts: w.get("isoWeek1Starts").as_str().unwrap().to_string(),
            day_of_year: n("dayOfYear"),
            days_in_year: n("daysInYear"),
            days_left_in_year: n("daysLeftInYear"),
        };
        assert_eq!(got, want, "{zone} at {now}");
        assert_eq!(
            current_time::facts_line(&got),
            r.get("line").as_str().unwrap()
        );
    }
    assert_eq!(current_time::calendar_facts(0.0, "Mars/Olympus"), None);
    assert_eq!(current_time::calendar_facts(f64::NAN, "UTC"), None);
}

#[test]
fn zone_table_matches_node_and_chrono_tz() {
    let f = fixture();
    let from = chrono::NaiveDate::parse_from_str(f.get("dailyFrom").as_str().unwrap(), "%Y-%m-%d")
        .unwrap();
    let day_ms = |i: i64| {
        (from + chrono::Days::new(i as u64))
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis() as f64
    };
    let (mut node_only, mut differ, mut days) = (Vec::new(), Vec::new(), 0);
    for z in arr(f.get("zones")) {
        let id = z.get("id").as_str().unwrap();
        if current_time::utc_offset_minutes(id, 0.0).is_none() {
            node_only.push(id.to_string());
            continue;
        }
        let mut i = 0i64;
        let mut open: Option<(i64, i64)> = None;
        for run in z.get("daily").as_str().unwrap().split(' ') {
            let (off, n) = run.split_once('*').unwrap();
            let (off, n): (i64, i64) = (off.parse().unwrap(), n.parse().unwrap());
            for _ in 0..n {
                let same = current_time::utc_offset_minutes(id, day_ms(i)) == Some(off);
                match (same, open) {
                    (false, None) => open = Some((i, i)),
                    (false, Some((a, _))) => open = Some((a, i)),
                    (true, Some((a, b))) => {
                        differ.push((id.to_string(), a, b));
                        open = None;
                    }
                    (true, None) => {}
                }
                i += 1;
            }
        }
        if let Some((a, b)) = open {
            differ.push((id.to_string(), a, b));
        }
        days = i;
    }
    assert_eq!(days, 730);
    assert_eq!(node_only, NODE_ONLY);
    let date = |i: i64| (from + chrono::Days::new(i as u64)).to_string();
    let differ: Vec<(String, String, String)> = differ
        .into_iter()
        .map(|(z, a, b)| (z, date(a), date(b)))
        .collect();
    let want: Vec<(String, String, String)> = DAILY_DIFFER
        .iter()
        .map(|(z, a, b)| (z.to_string(), a.to_string(), b.to_string()))
        .collect();
    assert_eq!(
        differ, want,
        "zone days where chrono-tz's offset is not Node's"
    );
    let rejected = strs(f.get("rejected"));
    for id in &rejected {
        assert_eq!(current_time::utc_offset_minutes(id, 0.0), None, "{id}");
    }
    let names: Vec<&str> = include_str!("../tools/chrono-tz-names.txt")
        .lines()
        .collect();
    let chrono_only: Vec<&str> = rejected
        .iter()
        .map(String::as_str)
        .filter(|r| names.contains(r))
        .collect();
    assert_eq!(chrono_only, CHRONO_ONLY);
    assert!(
        strs(f.get("caseExceptions")).is_empty(),
        "Intl matched every ID case-insensitively"
    );
}

#[test]
fn zone_table_and_fixture_come_from_the_same_sources() {
    let f = fixture();
    let JValue::Obj(sources) = f.get("sources") else {
        panic!("sources")
    };
    assert_eq!(sources.len(), 4);
    for (file, sha) in sources {
        let line = format!("//   {file} {}", sha.as_str().unwrap());
        assert!(
            ZONES_RS.lines().any(|l| l == line),
            "zones.rs was generated from another {file}"
        );
    }
    let v = f.get("versions");
    let head = format!(
        "// Node {}, ICU {}, tz {}.",
        v.get("node").as_str().unwrap(),
        v.get("icu").as_str().unwrap(),
        v.get("tz").as_str().unwrap()
    );
    assert!(ZONES_RS.lines().any(|l| l.starts_with(&head)));
}
