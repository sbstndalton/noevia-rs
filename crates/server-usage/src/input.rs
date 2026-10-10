//! Usage-local ordered JSON. Does not enable workspace-wide serde_json features.
use indexmap::IndexMap;
use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
#[derive(Clone, Debug)]
pub enum Input {
    Object(IndexMap<String, Input>),
    Array(Vec<Input>),
    Scalar(Value),
}
impl Input {
    pub fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(m) => m.get(key),
            _ => None,
        }
    }
    pub fn value(&self) -> Value {
        match self {
            Self::Object(m) => {
                Value::Object(m.iter().map(|(k, v)| (k.clone(), v.value())).collect())
            }
            Self::Array(a) => Value::Array(a.iter().map(Self::value).collect()),
            Self::Scalar(v) => v.clone(),
        }
    }
    pub fn from_value(v: &Value) -> Self {
        match v {
            Value::Object(m) => Self::Object(
                m.iter()
                    .map(|(k, v)| (k.clone(), Self::from_value(v)))
                    .collect(),
            ),
            Value::Array(a) => Self::Array(a.iter().map(Self::from_value).collect()),
            _ => Self::Scalar(v.clone()),
        }
    }
}
struct Ordered;
impl<'de> Visitor<'de> for Ordered {
    type Value = Input;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("bounded JSON usage input")
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Input, M::Error> {
        let mut out = IndexMap::new();
        while let Some((k, v)) = map.next_entry::<String, Input>()? {
            out.insert(k, v);
        }
        Ok(Input::Object(out))
    }
    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Input, S::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element::<Input>()? {
            out.push(v);
        }
        Ok(Input::Array(out))
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Input, E> {
        Ok(Input::Scalar(Value::Bool(v)))
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Input, E> {
        Ok(Input::Scalar(v.into()))
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Input, E> {
        Ok(Input::Scalar(v.into()))
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Input, E> {
        serde_json::Number::from_f64(v)
            .map(|n| Input::Scalar(Value::Number(n)))
            .ok_or_else(|| E::custom("nonfinite number"))
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Input, E> {
        Ok(Input::Scalar(v.into()))
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Input, E> {
        Ok(Input::Scalar(v.into()))
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Input, E> {
        Ok(Input::Scalar(Value::Null))
    }
}
impl<'de> Deserialize<'de> for Input {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(Ordered)
    }
}
