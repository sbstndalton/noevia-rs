//! Node usage-summary.cjs derivation: floating counters, stable ties, local civil days.
use crate::{Fault, KEY_CAP, SAFE_INTEGER};
use chrono::{DateTime, Duration, LocalResult, NaiveDate, TimeZone};
use chrono_tz::Tz;
use icu_collator::{options::CollatorOptions, Collator};
use icu_locale::Locale;
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, Default)]
pub struct Totals {
    pub input: f64,
    pub output: f64,
    pub replies: f64,
}
impl Totals {
    pub fn from(v: &Value) -> Self {
        Self {
            input: count(&prop(v, "input")),
            output: count(&prop(v, "output")),
            replies: count(&prop(v, "replies")),
        }
    }
    pub fn add(&mut self, b: Self) {
        self.input = (self.input + b.input).min(SAFE_INTEGER);
        self.output = (self.output + b.output).min(SAFE_INTEGER);
        self.replies = (self.replies + b.replies).min(SAFE_INTEGER);
    }
    fn value(self) -> Value {
        json!({"input":self.input,"output":self.output,"replies":self.replies})
    }
}
pub fn count(v: &Value) -> f64 {
    v.as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .unwrap_or(0.0)
        .min(SAFE_INTEGER)
}
fn index(k: &str) -> Option<u32> {
    let n = k.parse::<u32>().ok()?;
    (n < u32::MAX && n.to_string() == k).then_some(n)
}
/// Object.entries enumerates integer keys before insertion-ordered string keys.
fn ordered<T>(map: &IndexMap<String, T>) -> Vec<(&String, &T)> {
    let mut out: Vec<_> = map.iter().collect();
    out.sort_by(|(a, _), (b, _)| match (index(a), index(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        _ => Ordering::Equal,
    });
    out
}
fn entries(v: &crate::input::Input) -> Vec<(String, crate::input::Input)> {
    let mut out: Vec<_> = match v {
        crate::input::Input::Object(m) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        crate::input::Input::Array(a) => a
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v.clone()))
            .collect(),
        crate::input::Input::Scalar(Value::String(s)) => s
            .encode_utf16()
            .enumerate()
            .map(|(i, _)| (i.to_string(), crate::input::Input::Scalar(Value::Null)))
            .collect(),
        _ => Vec::new(),
    };
    out.sort_by(|(a, _), (b, _)| match (index(a), index(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        _ => Ordering::Equal,
    });
    out
}
fn input_prop(v: &crate::input::Input, k: &str) -> crate::input::Input {
    match v {
        crate::input::Input::Object(m) => m.get(k).cloned(),
        crate::input::Input::Array(a) => index(k).and_then(|i| a.get(i as usize)).cloned(),
        _ => None,
    }
    .unwrap_or(crate::input::Input::Scalar(Value::Null))
}
fn set(v: &mut Value, k: &str, x: Value) {
    if let Some(m) = v.as_object_mut() {
        m.insert(k.into(), x);
    }
}
fn prop(v: &Value, k: &str) -> Value {
    match v {
        Value::Object(m) => m.get(k).cloned().unwrap_or(Value::Null),
        Value::Array(a) => index(k)
            .and_then(|i| a.get(i as usize))
            .cloned()
            .unwrap_or(Value::Null),
        _ => Value::Null,
    }
}
#[derive(Clone, Debug, Default)]
pub struct Bucket {
    totals: Totals,
    models: IndexMap<String, Totals>,
    tools: IndexMap<String, f64>,
    hours: IndexMap<String, f64>,
}
#[derive(Clone, Debug, Default)]
pub struct Usage {
    days: IndexMap<String, Bucket>,
    keys: usize,
}
fn day_shape(k: &str) -> bool {
    k.len() == 10
        && k.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
}
fn slot<'a, T: Default>(
    map: &'a mut IndexMap<String, T>,
    k: String,
    keys: &mut usize,
) -> Result<&'a mut T, Fault> {
    if !map.contains_key(&k) {
        *keys += 1;
        if *keys > KEY_CAP {
            return Err(Fault::Limit);
        };
    }
    Ok(map.entry(k).or_default())
}
impl Usage {
    /// Aggregate's mergeUsage discards non-date keys/non-object buckets. Own reads preserve
    /// direct bucket lookup semantics; all dates emitted below are valid civil dates.
    pub fn merge(&mut self, store: &Value, aggregate: bool) -> Result<(), Fault> {
        self.merge_input(&crate::input::Input::from_value(store), aggregate)
    }
    pub fn merge_input(
        &mut self,
        store: &crate::input::Input,
        aggregate: bool,
    ) -> Result<(), Fault> {
        for (day, v) in entries(&input_prop(store, "days")) {
            if aggregate
                && (!day_shape(&day)
                    || !matches!(
                        v,
                        crate::input::Input::Object(_) | crate::input::Input::Array(_)
                    ))
            {
                continue;
            }
            let target = slot(&mut self.days, day, &mut self.keys)?;
            target.totals.add(Totals::from(&v.value()));
            for (k, v) in entries(&input_prop(&v, "models")) {
                slot(&mut target.models, k, &mut self.keys)?.add(Totals::from(&v.value()));
            }
            for (k, v) in entries(&input_prop(&v, "tools")) {
                let n = slot(&mut target.tools, k, &mut self.keys)?;
                *n = (*n + count(&v.value())).min(SAFE_INTEGER);
            }
            for (k, v) in entries(&input_prop(&v, "hours")) {
                let n = slot(&mut target.hours, k, &mut self.keys)?;
                *n = (*n + count(&v.value())).min(SAFE_INTEGER);
            }
        }
        Ok(())
    }
}

fn timezone_name(name: &str) -> Result<String, Fault> {
    static NAMES: std::sync::OnceLock<Option<Value>> = std::sync::OnceLock::new();
    let data = NAMES.get_or_init(|| serde_json::from_str(include_str!("timezone-names.json")).ok());
    data.as_ref()
        .and_then(|d| d.get("mapping"))
        .and_then(|m| m.get(name))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(Fault::Clock)
}

/// Native equivalent of Intl default locale and server local civil dates.
#[derive(Clone, Debug)]
pub struct Clock {
    pub tz: Tz,
    pub timezone: String,
    locale: Locale,
}
impl Clock {
    pub fn new(timezone: &str, locale: &str) -> Result<Self, Fault> {
        Ok(Self {
            tz: timezone.parse().map_err(|_| Fault::Clock)?,
            timezone: timezone_name(timezone)?,
            locale: locale.parse().map_err(|_| Fault::Clock)?,
        })
    }
    pub fn system(get: impl Fn(&str) -> Option<String>) -> Result<Self, Fault> {
        let tz = get("TZ")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into()));
        // ICU treats explicitly empty LC_ALL/LC_MESSAGES as root (und), unlike an
        // absent variable. Preserve that distinction instead of falling through to LANG.
        let raw = get("LC_ALL")
            .or_else(|| get("LC_MESSAGES"))
            .or_else(|| get("LANG"))
            .unwrap_or_else(|| "en-US".into());
        let locale = raw.split('.').next().unwrap_or("").replace('_', "-");
        let locale = if locale.is_empty() {
            "und"
        } else if locale == "C" || locale == "POSIX" {
            "en-US"
        } else {
            &locale
        };
        Self::new(&tz, locale)
    }
    /// JS setDate uses compatible disambiguation: earlier occurrence at overlaps, move forward
    /// by the gap at nonexistent civil times (including Apia's skipped day).
    fn civil_noon(&self, date: NaiveDate) -> Result<DateTime<Tz>, Fault> {
        let noon = date.and_hms_opt(12, 0, 0).ok_or(Fault::Clock)?;
        match self.tz.from_local_datetime(&noon) {
            LocalResult::Single(d) => Ok(d),
            LocalResult::Ambiguous(a, _) => Ok(a),
            LocalResult::None => {
                // Find valid times bounding a gap, then add its offset displacement to the target.
                use chrono::Offset;
                let mut before = None;
                let mut after = None;
                for h in 1..=48 {
                    if before.is_none() {
                        before = self
                            .tz
                            .from_local_datetime(&(noon - Duration::hours(h)))
                            .earliest();
                    }
                    if after.is_none() {
                        after = self
                            .tz
                            .from_local_datetime(&(noon + Duration::hours(h)))
                            .earliest();
                    }
                    if before.is_some() && after.is_some() {
                        break;
                    }
                }
                let (Some(a), Some(b)) = (before, after) else {
                    return Err(Fault::Clock);
                };
                let gap = b.offset().fix().local_minus_utc() - a.offset().fix().local_minus_utc();
                self.tz
                    .from_local_datetime(&(noon + Duration::seconds(i64::from(gap))))
                    .earliest()
                    .ok_or(Fault::Clock)
            }
        }
    }
}
pub fn summarize(
    store: &Usage,
    clock: &Clock,
    now_ms: i64,
    retention: usize,
) -> Result<Value, Fault> {
    if retention == 0 || retention > 365 {
        return Err(Fault::Limit);
    }
    let now = clock
        .tz
        .timestamp_millis_opt(now_ms)
        .single()
        .ok_or(Fault::Clock)?;
    let today = now.format("%Y-%m-%d").to_string();
    let mut days = Vec::with_capacity(retention);
    let mut buckets = Vec::with_capacity(retention);
    let mut models = IndexMap::<String, Totals>::new();
    let mut tools = IndexMap::<String, f64>::new();
    let mut hours = IndexMap::<String, f64>::new();
    for i in (0..retention).rev() {
        let date = now
            .date_naive()
            .checked_sub_signed(Duration::days(i as i64))
            .ok_or(Fault::Clock)?;
        let day = clock.civil_noon(date)?.format("%Y-%m-%d").to_string();
        let b = store.days.get(&day).cloned().unwrap_or_default();
        let mut v = b.totals.value();
        set(&mut v, "day", Value::String(day));
        days.push(v);
        buckets.push(b.totals);
        for (k, v) in ordered(&b.models) {
            models.entry(k.clone()).or_default().add(*v);
        }
        for (k, v) in ordered(&b.tools) {
            let n = tools.entry(k.clone()).or_default();
            *n = (*n + v).min(SAFE_INTEGER);
        }
        for (k, v) in ordered(&b.hours) {
            let n = hours.entry(k.clone()).or_default();
            *n = (*n + v).min(SAFE_INTEGER);
        }
    }
    let total = |n: usize| {
        let mut t = Totals::default();
        for b in buckets.iter().skip(retention.saturating_sub(n)) {
            t.add(*b);
        }
        t.value()
    };
    let mut streak = 0;
    let mut longest = 0;
    let mut run = 0;
    for d in days.iter().rev() {
        if count(&prop(d, "replies")) > 0.0 {
            streak += 1;
        } else if prop(d, "day") != today {
            break;
        }
    }
    for b in &buckets {
        run = if b.replies > 0.0 { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let mut model_rows: Vec<_> = ordered(&models)
        .into_iter()
        .map(|(k, v)| {
            let mut t = v.value();
            set(&mut t, "name", json!(k));
            t
        })
        .collect();
    // #1286: compare combined weights, preserving insertion order for equal weights.
    model_rows.sort_by(model_compare);

    let collator = Collator::try_new(clock.locale.clone().into(), CollatorOptions::default())
        .map_err(|_| Fault::Clock)?;
    let mut tool_rows: Vec<_> = ordered(&tools)
        .into_iter()
        .filter(|(_, v)| **v > 0.0)
        .map(|(k, v)| json!({"name":k,"calls":v}))
        .collect();
    tool_rows.sort_by(|a, b| {
        count(&prop(b, "calls"))
            .partial_cmp(&count(&prop(a, "calls")))
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                collator.compare(
                    prop(a, "name").as_str().unwrap_or(""),
                    prop(b, "name").as_str().unwrap_or(""),
                )
            })
    });
    let hour_rows: Vec<_> = (0..24)
        .map(|h| *hours.get(&h.to_string()).unwrap_or(&0.0))
        .collect();
    let mut peak = Value::Null;
    let mut most = 0.0;
    for (h, n) in hour_rows.iter().enumerate() {
        if *n > most {
            most = *n;
            peak = json!({"hour":h,"replies":n});
        }
    }
    Ok(
        json!({"days":days,"allTime":total(retention),"last7":total(7),"last30":total(30),"activeDays":buckets.iter().filter(|b|b.replies>0.0).count(),"currentStreak":streak,"longestStreak":longest,"models":model_rows,"tools":tool_rows,"hours":hour_rows,"peakHour":peak,"retentionDays":retention,"timeZone":clock.timezone}),
    )
}

fn model_compare(a: &Value, b: &Value) -> Ordering {
    let aw = count(&prop(a, "input")) + count(&prop(a, "output"));
    let bw = count(&prop(b, "input")) + count(&prop(b, "output"));
    bw.partial_cmp(&aw).unwrap_or(Ordering::Equal)
}
#[cfg(test)]
mod sort_properties {
    use super::*;
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn model_comparator_is_reflexive_and_antisymmetric(ai in 0f64..SAFE_INTEGER, ao in 0f64..SAFE_INTEGER, bi in 0f64..SAFE_INTEGER, bo in 0f64..SAFE_INTEGER) {
            let a=json!({"input":ai,"output":ao}); let b=json!({"input":bi,"output":bo});
            prop_assert_eq!(model_compare(&a,&a),Ordering::Equal);
            prop_assert_eq!(model_compare(&a,&b),model_compare(&b,&a).reverse());
        }
    }
    #[test]
    fn fractional_safe_integer_self_comparison_is_equal() {
        let row = json!({"input":SAFE_INTEGER,"output":0.1});
        assert_eq!(model_compare(&row, &row), Ordering::Equal);
    }
}
