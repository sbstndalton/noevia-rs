use chrono::{Datelike, NaiveDate};
use diary_temporal_focus::{apply_cutoff, time_anchor, Candidate, TimeAnchor};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use std::collections::HashSet;

struct StrictValue(Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = StrictValue;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON without duplicate object keys")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Bool(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Number(v.into())))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Number(v.into())))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| StrictValue(Value::Number(n)))
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::String(v.to_owned())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::String(v)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(v) = seq.next_element::<StrictValue>()? {
                    values.push(v.0);
                }
                Ok(StrictValue(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                let mut keys = HashSet::new();
                while let Some(k) = map.next_key::<String>()? {
                    if !keys.insert(k.clone()) {
                        return Err(de::Error::custom("duplicate JSON key"));
                    }
                    let v = map.next_value::<StrictValue>()?;
                    values.insert(k, v.0);
                }
                Ok(StrictValue(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(V)
    }
}

use std::io::{self, Read, Write};
const R: &[u8; 4] = b"DTA1";
const S: &[u8; 4] = b"DTR1";
const E: &[u8; 4] = b"END1";
const MAX: usize = 2 * 1024 * 1024;
const Q: usize = 1024 * 1024;
const N: usize = 10000;
#[derive(Debug, PartialEq, Eq)]
enum F {
    Protocol,
    Capacity,
    Internal,
}
impl F {
    fn code(&self) -> i32 {
        match self {
            F::Protocol => 2,
            F::Capacity => 3,
            F::Internal => 4,
        }
    }
}
fn anchor(v: &Value) -> Result<Option<TimeAnchor>, F> {
    if v.is_null() {
        return Ok(None);
    }
    let d = v.get("day").and_then(Value::as_str).ok_or(F::Protocol)?;
    let i = v
        .get("inclusive")
        .and_then(Value::as_bool)
        .ok_or(F::Protocol)?;
    if v.as_object().is_none_or(|m| m.len() != 2) || d.len() != 10 {
        return Err(F::Protocol);
    }
    let day = NaiveDate::parse_from_str(d, "%Y-%m-%d").map_err(|_| F::Protocol)?;
    if day.year() < 1 || day.to_string() != d {
        return Err(F::Protocol);
    }
    Ok(Some(TimeAnchor { day, inclusive: i }))
}
fn run(b: &[u8]) -> Result<Vec<u8>, F> {
    if b.len() > MAX {
        return Err(F::Capacity);
    }
    if b.len() < 16 || b.get(..4) != Some(R) {
        return Err(F::Protocol);
    }
    let n = usize::try_from(u64::from_be_bytes(
        b.get(4..12)
            .ok_or(F::Protocol)?
            .try_into()
            .map_err(|_| F::Protocol)?,
    ))
    .map_err(|_| F::Capacity)?;
    let e = 12usize.checked_add(n).ok_or(F::Capacity)?;
    if e + 4 != b.len() || b.get(e..) != Some(E) {
        return Err(F::Protocol);
    }
    let mut de = serde_json::Deserializer::from_slice(b.get(12..e).ok_or(F::Protocol)?);
    let v = StrictValue::deserialize(&mut de)
        .map_err(|_| F::Protocol)?
        .0;
    de.end().map_err(|_| F::Protocol)?;
    let m = v.as_object().ok_or(F::Protocol)?;
    let op = m.get("op").and_then(Value::as_str).ok_or(F::Protocol)?;
    let out = match op {
        "anchor" => {
            if m.len() != 3
                || m.keys()
                    .any(|k| !["version", "op", "query"].contains(&k.as_str()))
                || m.get("version").and_then(Value::as_u64) != Some(1)
            {
                return Err(F::Protocol);
            }
            let q = m.get("query").and_then(Value::as_str).ok_or(F::Protocol)?;
            if q.len() > Q {
                return Err(F::Capacity);
            }
            let a = time_anchor(q);
            json!({"version":1,"op":"anchor","anchor":a.map(|x|json!({"day":x.day.to_string(),"inclusive":x.inclusive}))})
        }
        "cutoff" => {
            if m.len() != 4
                || m.keys()
                    .any(|k| !["version", "op", "anchor", "candidates"].contains(&k.as_str()))
                || m.get("version").and_then(Value::as_u64) != Some(1)
            {
                return Err(F::Protocol);
            }
            let a = anchor(m.get("anchor").ok_or(F::Protocol)?)?;
            let rows = m
                .get("candidates")
                .and_then(Value::as_array)
                .ok_or(F::Protocol)?;
            if rows.len() > N {
                return Err(F::Capacity);
            }
            let mut c = Vec::new();
            for r in rows {
                let d = match r.get("day") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(s)) if s.len() <= 64 => Some(s.clone()),
                    Some(Value::String(_)) => return Err(F::Capacity),
                    _ => return Err(F::Protocol),
                };
                if r.as_object()
                    .is_none_or(|x| x.len() > 1 || x.keys().any(|k| k != "day"))
                {
                    return Err(F::Protocol);
                }
                c.push(Candidate {
                    id: c.len() + 1,
                    day: d,
                })
            }
            let x = apply_cutoff(a, c);
            json!({"version":1,"op":"cutoff","anchor":a.map(|x|json!({"day":x.day.to_string(),"inclusive":x.inclusive})),"kept_ranks":x.kept.iter().map(|c|c.id).collect::<Vec<_>>(),"removed":x.removed.iter().map(|(c,r)|json!({"rank":c.id,"reason":r})).collect::<Vec<_>>()})
        }
        _ => return Err(F::Protocol),
    };
    let body = serde_json::to_vec(&out).map_err(|_| F::Internal)?;
    if body.len() > 1024 * 1024 {
        return Err(F::Capacity);
    }
    let mut o = Vec::new();
    o.extend(S);
    o.extend((body.len() as u64).to_be_bytes());
    o.extend(body);
    o.extend(E);
    Ok(o)
}
fn main() {
    let mut b = Vec::new();
    let x = io::stdin().take((MAX + 1) as u64).read_to_end(&mut b);
    let r = if x.is_err() {
        Err(F::Internal)
    } else {
        run(&b)
    };
    match r {
        Ok(o) => {
            if io::stdout().write_all(&o).is_err() {
                std::process::exit(4)
            }
        }
        Err(e) => std::process::exit(e.code()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn frame(v: Value) -> Vec<u8> {
        let body = serde_json::to_vec(&v).unwrap_or_default();
        let mut bytes = R.to_vec();
        bytes.extend((body.len() as u64).to_be_bytes());
        bytes.extend(body);
        bytes.extend(E);
        bytes
    }
    #[test]
    fn operations_and_cutoff_match_contract() {
        let a = run(&frame(
            json!({"version":1,"op":"anchor","query":"As of January 20, 2030, what happened?"}),
        ));
        assert!(a.is_ok());
        let no = run(&frame(
            json!({"version":1,"op":"anchor","query":"No anchor"}),
        ));
        assert!(no.is_ok());
        let c = run(&frame(
            json!({"version":1,"op":"cutoff","anchor":{"day":"2030-01-20","inclusive":false},"candidates":[{"day":"2030-01-20"},{"day":null},{"day":"bad"},{"day":"2030-01-21"}]}),
        ));
        assert!(c.is_ok());
        let Ok(bytes) = c else { return };
        let n = usize::try_from(u64::from_be_bytes(
            bytes
                .get(4..12)
                .and_then(|x| x.try_into().ok())
                .unwrap_or([0; 8]),
        ))
        .unwrap_or(0);
        let value: Value = serde_json::from_slice(bytes.get(12..12 + n).unwrap_or_default())
            .unwrap_or(Value::Null);
        assert_eq!(
            value.get("anchor"),
            Some(&json!({"day":"2030-01-20","inclusive":false}))
        );
        assert_eq!(value.get("kept_ranks"), Some(&json!([2, 3])));
        assert_eq!(
            value.get("removed"),
            Some(
                &json!([{"rank":1,"reason":"after_time_anchor"},{"rank":4,"reason":"after_time_anchor"}])
            )
        );
    }
    #[test]
    fn capacities_and_trailing_bytes_are_rejected() {
        assert_eq!(
            run(&frame(
                json!({"version":1,"op":"anchor","query":"x".repeat(Q+1)})
            ),),
            Err(F::Capacity)
        );
        assert_eq!(
            run(&frame(
                json!({"version":1,"op":"cutoff","anchor":null,"candidates":vec![json!({"day":null});N+1]})
            )),
            Err(F::Capacity)
        );
        let mut f = frame(json!({"version":1,"op":"anchor","query":"?"}));
        f.push(0);
        assert_eq!(run(&f), Err(F::Protocol));
    }

    #[test]
    fn rejects_duplicate_keys_at_every_depth() {
        for request in [
            br#"{"version":1,"op":"anchor","query":"?","op":"cutoff"}"#.as_slice(),
            br#"{"version":1,"op":"cutoff","anchor":{"day":"2030-01-20","day":"2030-01-21","inclusive":true},"candidates":[]}"#.as_slice(),
            br#"{"version":1,"op":"cutoff","anchor":null,"candidates":[{"day":null,"day":"2030-01-20"}]}"#.as_slice(),
        ] {
            let mut frame = R.to_vec();
            frame.extend((request.len() as u64).to_be_bytes());
            frame.extend(request);
            frame.extend(E);
            assert_eq!(run(&frame), Err(F::Protocol));
        }
    }

    #[test]
    fn anchor_none() {
        assert!(run(b"").is_err());
        assert!(time_anchor("no date").is_none())
    }
}
