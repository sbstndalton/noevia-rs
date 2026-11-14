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
/// (zone, recorded instant) where chrono-tz's offset is not Node's: the two tz releases differ.
const OFFSETS_DIFFER: [(&str, &str); 7] = [
    ("Africa/Casablanca", DEC),
    ("Africa/El_Aaiun", DEC),
    ("America/Edmonton", DEC),
    ("America/Vancouver", DEC),
    ("America/Yellowknife", DEC),
    ("Canada/Mountain", DEC),
    ("Canada/Pacific", DEC),
];
const DEC: &str = "2026-12-15T12:00:00.000Z";

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
        let want_text = c.get("text").as_str().unwrap().to_string();
        let want = if c.get("failed").as_bool().unwrap() {
            Err(want_text)
        } else {
            Ok(want_text)
        };
        if got == want {
            same += 1;
        } else {
            wrong.push(format!(
                "{} {server:?} at {now}:\n  node: {want:?}\n  rust: {got:?}",
                js_json::stringify(args).unwrap()
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} differ:\n{}",
        wrong.len(),
        same + wrong.len(),
        wrong
            .iter()
            .take(15)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(same > 2000, "{same} cases");
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
    let instants: Vec<f64> = strs(f.get("zoneInstants"))
        .iter()
        .map(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .timestamp_millis() as f64
        })
        .collect();
    let labels = strs(f.get("zoneInstants"));
    let (mut node_only, mut differ) = (Vec::new(), Vec::new());
    for z in arr(f.get("zones")) {
        let id = z.get("id").as_str().unwrap();
        for (i, want) in arr(z.get("offsets")).iter().enumerate() {
            let want = want.as_num().unwrap() as i64;
            match current_time::utc_offset_minutes(id, instants[i]) {
                None if i == 0 => node_only.push(id.to_string()),
                None => {}
                Some(got) if got != want => differ.push((id.to_string(), labels[i].clone())),
                Some(_) => {}
            }
        }
    }
    assert_eq!(node_only, NODE_ONLY);
    assert_eq!(
        differ,
        OFFSETS_DIFFER.map(|(a, b)| (a.to_string(), b.to_string()))
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
    assert_eq!(sources.len(), 3);
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
