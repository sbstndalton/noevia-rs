//! Lossless legacy capsule values. Raw JSON tokens avoid rounding Python integers;
//! ordered objects retain dict insertion order (including duplicate-key replacement).
//! Serialization follows Python's JSONResponse or sorted capsule writer as selected.
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use std::fmt;

#[derive(Clone, Debug)]
pub(crate) enum Json {
    Scalar(String),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

struct Entries(Vec<(String, Box<RawValue>)>);
impl<'de> Deserialize<'de> for Entries {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Ordered;
        impl<'de> Visitor<'de> for Ordered {
            type Value = Entries;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an ordered JSON object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Entries, M::Error> {
                let mut entries = Vec::new();
                while let Some(pair) = map.next_entry()? {
                    entries.push(pair);
                }
                Ok(Entries(entries))
            }
        }
        d.deserialize_map(Ordered)
    }
}

impl Json {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, ()> {
        let raw: Box<RawValue> = serde_json::from_slice(bytes).map_err(|_| ())?;
        Self::parse_raw(&raw, 0, &mut 0)
    }
    fn parse_raw(raw: &RawValue, depth: usize, nodes: &mut usize) -> Result<Self, ()> {
        *nodes += 1;
        if depth > 128 || *nodes > 100_000 {
            return Err(());
        }
        let text = raw.get();
        match text.as_bytes().first() {
            Some(b'{') => {
                let Entries(entries) = serde_json::from_str(text).map_err(|_| ())?;
                let mut object: Vec<(String, Json)> = Vec::new();
                let mut positions = std::collections::HashMap::new();
                for (key, value) in entries {
                    let value = Self::parse_raw(&value, depth + 1, nodes)?;
                    if let Some(&index) = positions.get(&key) {
                        let (_, current) = object.get_mut(index).ok_or(())?;
                        *current = value;
                    } else {
                        positions.insert(key.clone(), object.len());
                        object.push((key, value));
                    }
                }
                Ok(Self::Object(object))
            }
            Some(b'[') => {
                let entries: Vec<Box<RawValue>> = serde_json::from_str(text).map_err(|_| ())?;
                Ok(Self::Array(
                    entries
                        .iter()
                        .map(|v| Self::parse_raw(v, depth + 1, nodes))
                        .collect::<Result<_, _>>()?,
                ))
            }
            Some(b'"') => Ok(Self::String(serde_json::from_str(text).map_err(|_| ())?)),
            _ if matches!(text, "null" | "true" | "false") => Ok(Self::Scalar(text.into())),
            _ if text.contains(['.', 'e', 'E']) => {
                let number: f64 = text.parse().map_err(|_| ())?;
                if !number.is_finite() {
                    return Err(());
                }
                Ok(Self::Scalar(diary_backup_format::python_float(number)))
            }
            _ => {
                // RawValue has validated the integer's JSON syntax. Python's default
                // integer digit limit is 4300; unsupported records safely decline.
                if text.trim_start_matches('-').len() > 4300 {
                    return Err(());
                }
                Ok(Self::Scalar(if text == "-0" { "0" } else { text }.into()))
            }
        }
    }
    pub(crate) fn from_value(value: &serde_json::Value) -> Result<Self, ()> {
        Self::parse(&serde_json::to_vec(value).map_err(|_| ())?)
    }
    pub(crate) fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(o) => o.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }
    pub(crate) fn set(&mut self, key: &str, value: Self) -> Result<(), ()> {
        let Self::Object(o) = self else {
            return Err(());
        };
        if let Some((_, current)) = o.iter_mut().find(|(k, _)| k == key) {
            *current = value;
        } else {
            o.push((key.into(), value));
        }
        Ok(())
    }
    pub(crate) fn object(entries: Vec<(&str, Self)>) -> Self {
        Self::Object(entries.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }
    pub(crate) fn bool(value: bool) -> Self {
        Self::Scalar(value.to_string())
    }
    pub(crate) fn encode(&self, sort_keys: bool, spaces: bool) -> Result<Vec<u8>, ()> {
        fn render(v: &Json, sort: bool, spaces: bool) -> Result<String, ()> {
            let comma = if spaces { ", " } else { "," };
            Ok(match v {
                Json::Scalar(s) => s.clone(),
                Json::String(s) => serde_json::to_string(s).map_err(|_| ())?,
                Json::Array(a) => format!(
                    "[{}]",
                    a.iter()
                        .map(|v| render(v, sort, spaces))
                        .collect::<Result<Vec<_>, _>>()?
                        .join(comma)
                ),
                Json::Object(o) => {
                    let mut entries = o.iter().collect::<Vec<_>>();
                    if sort {
                        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
                    }
                    let colon = if spaces { ": " } else { ":" };
                    format!(
                        "{{{}}}",
                        entries
                            .into_iter()
                            .map(|(k, v)| Ok(format!(
                                "{}{colon}{}",
                                serde_json::to_string(k).map_err(|_| ())?,
                                render(v, sort, spaces)?
                            )))
                            .collect::<Result<Vec<_>, ()>>()?
                            .join(comma)
                    )
                }
            })
        }
        Ok(render(self, sort_keys, spaces)?.into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn python_values_keep_order_numbers_and_duplicate_key_position() -> Result<(), ()> {
        let v = Json::parse(br#"{"z":{"b":1,"a":2},"n":18446744073709551617,"f":[1e-7,1e20,-0.0],"z":{"y":1,"x":2},"s":"\u00e9\n"}"#)?;
        assert_eq!(String::from_utf8(v.encode(false, false)?).map_err(|_| ())?, "{\"z\":{\"y\":1,\"x\":2},\"n\":18446744073709551617,\"f\":[1e-07,1e+20,-0.0],\"s\":\"é\\n\"}");
        assert_eq!(String::from_utf8(v.encode(true, true)?).map_err(|_| ())?, "{\"f\": [1e-07, 1e+20, -0.0], \"n\": 18446744073709551617, \"s\": \"é\\n\", \"z\": {\"x\": 2, \"y\": 1}}");
        assert!(Json::parse(br#"{"unused":"\ud800"}"#).is_err());
        assert!(Json::parse(br#"{"n":1e400}"#).is_err());
        Ok(())
    }
}
