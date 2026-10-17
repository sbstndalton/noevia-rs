//! The Python-exact pieces of `downloader.py` and its helpers that the native job table needs:
//! `utils.human_bytes`, the ETA text, `round(x, 1)`, `email.utils.parsedate_to_datetime` (for the
//! resume validator), `_validator`, and `safe_dest` with `Path.resolve()`'s `realpath`.

/// `round(x, 1)` on a finite float: correctly rounded, ties to even on the exact binary value
/// (Rust's fixed-precision formatting does the same).
pub fn round1(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.1}").parse().unwrap_or(x)
}

/// A float as `json.dumps` writes it (`repr`); never called with a non-finite value.
pub fn float(x: f64) -> String {
    gguf::py_float_repr(x)
}

/// A JSON string literal as `json.dumps(ensure_ascii=False)` writes it.
pub fn string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

/// `utils.human_bytes(n)`.
pub fn human_bytes(n: f64) -> String {
    let mut n = n;
    for unit in ["B", "KB", "MB", "GB", "TB", "PB"] {
        if n.abs() < 1024.0 {
            return if unit == "B" {
                #[allow(clippy::cast_possible_truncation)]
                let whole = n.trunc() as i64;
                format!("{whole} B")
            } else {
                format!("{n:.1} {unit}")
            };
        }
        n /= 1024.0;
    }
    format!("{n:.1} EB")
}

/// `DownloadJob.eta_h`.
pub fn eta_h(eta: Option<i64>) -> String {
    match eta {
        None => "—".into(),
        Some(e) if e < 60 => format!("{e}s"),
        Some(e) if e < 3600 => format!("{}m {}s", e.div_euclid(60), e.rem_euclid(60)),
        Some(e) => format!(
            "{}h {}m",
            e.div_euclid(3600),
            e.rem_euclid(3600).div_euclid(60)
        ),
    }
}

/// `str.split()` whitespace for the characters an HTTP header can decode to.
fn py_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{0b}'
            | '\u{0c}'
            | '\r'
            | '\u{1c}'..='\u{1f}'
            | ' '
            | '\u{85}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

/// `str.strip()`.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(py_space)
}

/// `int(s)` for a header token: sign, ASCII digits with single underscores between them,
/// surrounding whitespace. `None` where Python raises ValueError (or the value is past i64,
/// where `datetime` would raise anyway).
fn py_int(s: &str) -> Option<i64> {
    let t = py_strip(s);
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, t.get(1..)?),
        Some(b'+') => (false, t.get(1..)?),
        _ => (false, t),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'_')
    {
        return None;
    }
    let clean: String = digits.chars().filter(|c| *c != '_').collect();
    let value: i64 = clean.parse().ok()?;
    Some(if neg { -value } else { value })
}

/// `str.isdigit()` on one character in the range a header decodes to.
fn py_isdigit(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '¹' | '²' | '³')
}

const MONTHS: [&str; 24] = [
    "jan",
    "feb",
    "mar",
    "apr",
    "may",
    "jun",
    "jul",
    "aug",
    "sep",
    "oct",
    "nov",
    "dec",
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];
const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

fn zone(name: &str) -> Option<i64> {
    Some(match name {
        "UT" | "UTC" | "GMT" | "Z" => 0,
        "AST" => -400,
        "ADT" => -300,
        "EDT" => -400,
        "EST" => -500,
        "CST" => -600,
        "CDT" => -500,
        "MST" => -700,
        "MDT" => -600,
        "PST" => -800,
        "PDT" => -700,
        _ => return None,
    })
}

fn last_char(s: &str) -> Option<char> {
    s.chars().last()
}

fn drop_last(s: &str) -> String {
    let mut c = s.chars();
    c.next_back();
    c.as_str().to_owned()
}

/// `email._parseaddr._parsedate_tz(data)`: `(y, m, d, H, M, S, tzoffset seconds or None)`.
#[allow(clippy::too_many_lines)]
fn parsedate_tz(data: &str) -> Option<([i64; 6], Option<i64>)> {
    let mut data: Vec<String> = data
        .split(py_space)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    let first = data.first()?.clone();
    if first.ends_with(',') || DAYS.contains(&first.to_lowercase().as_str()) {
        data.remove(0);
    } else if let Some(i) = first.rfind(',') {
        if let Some(head) = data.first_mut() {
            *head = first.get(i + 1..)?.to_owned();
        }
    }
    if data.len() == 3 {
        let stuff: Vec<String> = data.first()?.split('-').map(str::to_owned).collect();
        if stuff.len() == 3 {
            let rest: Vec<String> = data.get(1..)?.to_vec();
            data = stuff.into_iter().chain(rest).collect();
        }
    }
    if data.len() == 4 {
        let s = data.get(3)?.clone();
        let i = s.find('+').or_else(|| s.find('-'));
        match i {
            Some(i) if i > 0 => {
                data.truncate(3);
                data.push(s.get(..i)?.to_owned());
                data.push(s.get(i..)?.to_owned());
            }
            _ => data.push(String::new()),
        }
    }
    if data.len() < 5 {
        return None;
    }
    data.truncate(5);
    let [mut dd, mut mm, mut yy, mut tm, mut tz]: [String; 5] = data.try_into().ok()?;
    if dd.is_empty() || mm.is_empty() || yy.is_empty() {
        return None;
    }
    mm = mm.to_lowercase();
    if !MONTHS.contains(&mm.as_str()) {
        let swapped = dd.to_lowercase();
        dd = mm;
        mm = swapped;
        if !MONTHS.contains(&mm.as_str()) {
            return None;
        }
    }
    let mut month = i64::try_from(MONTHS.iter().position(|m| *m == mm)? + 1).ok()?;
    if month > 12 {
        month -= 12;
    }
    if last_char(&dd) == Some(',') {
        dd = drop_last(&dd);
    }
    if yy.find(':').is_some_and(|i| i > 0) {
        std::mem::swap(&mut yy, &mut tm);
    }
    if last_char(&yy) == Some(',') {
        yy = drop_last(&yy);
        if yy.is_empty() {
            return None;
        }
    }
    if !yy.chars().next().is_some_and(py_isdigit) {
        std::mem::swap(&mut yy, &mut tz);
    }
    if last_char(&tm) == Some(',') {
        tm = drop_last(&tm);
    }
    let parts: Vec<&str> = tm.split(':').collect();
    let (thh, tmm, tss) = match parts.as_slice() {
        [h, m] => ((*h).to_owned(), (*m).to_owned(), "0".to_owned()),
        [h, m, s] => ((*h).to_owned(), (*m).to_owned(), (*s).to_owned()),
        [one] if one.contains('.') => {
            let dotted: Vec<&str> = one.split('.').collect();
            match dotted.as_slice() {
                [h, m] => ((*h).to_owned(), (*m).to_owned(), "0".to_owned()),
                [h, m, s] => ((*h).to_owned(), (*m).to_owned(), (*s).to_owned()),
                _ => return None,
            }
        }
        _ => return None,
    };
    let mut year = py_int(&yy)?;
    let day = py_int(&dd)?;
    let (hour, minute, second) = (py_int(&thh)?, py_int(&tmm)?, py_int(&tss)?);
    if year < 100 {
        year += if year > 68 { 1900 } else { 2000 };
    }
    let tz = tz.to_uppercase();
    let mut offset = zone(&tz).or_else(|| py_int(&tz));
    if offset == Some(0) && tz.starts_with('-') {
        offset = None;
    }
    let offset = match offset {
        Some(o) if o != 0 => {
            let (sign, o) = if o < 0 { (-1, -o) } else { (1, o) };
            Some(sign * (o.div_euclid(100) * 3600 + o.rem_euclid(100) * 60))
        }
        other => other,
    };
    Some(([year, month, day, hour, minute, second], offset))
}

/// Days from 1970-01-01 to the given proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9).rem_euclid(12);
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn month_days(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `downloader._http_date(value)`: the POSIX time of an HTTP date with a zone, else `None`.
/// Python lets an `OverflowError` from an absurd field escape and fail the download; here such
/// a date is simply not a date.
pub fn http_date(value: Option<&str>) -> Option<f64> {
    let value = py_strip(value.unwrap_or(""));
    if value.is_empty() {
        return None;
    }
    let ([y, m, d, hh, mm, ss], offset) = parsedate_tz(value)?;
    let offset = offset?;
    // `datetime(...)` and `timezone(timedelta(seconds=tz))` refuse what is out of range.
    if !(1..=9999).contains(&y)
        || !(1..=12).contains(&m)
        || d < 1
        || d > month_days(y, m)
        || !(0..24).contains(&hh)
        || !(0..60).contains(&mm)
        || !(0..60).contains(&ss)
        || offset.abs() >= 86_400
    {
        return None;
    }
    let local = days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss;
    #[allow(clippy::cast_precision_loss)]
    Some((local - offset) as f64)
}

/// RFC 9110: a Last-Modified at least this long before the response's Date is a strong
/// validator (`LAST_MODIFIED_MIN_AGE_S`).
pub const LAST_MODIFIED_MIN_AGE_S: f64 = 60.0;

/// `downloader._validator(headers)`. `get` is httpx's `headers.get` (multiple values joined
/// with `", "`); `now` is `time.time()`.
pub fn validator(get: &dyn Fn(&str) -> Option<String>, now: f64) -> String {
    let etag = get("etag").unwrap_or_default();
    let etag = py_strip(&etag);
    if !etag.is_empty() && !etag.starts_with("W/") {
        return etag.to_owned();
    }
    let last_modified = get("last-modified").unwrap_or_default();
    let last_modified = py_strip(&last_modified);
    let Some(modified) = http_date(Some(last_modified)) else {
        return String::new();
    };
    let date = get("date");
    let now = http_date(date.as_deref()).unwrap_or(now);
    if now - modified >= LAST_MODIFIED_MIN_AGE_S {
        last_modified.to_owned()
    } else {
        String::new()
    }
}

/// `PurePosixPath(base) / name` as text: `name` absolute replaces `base`; empty and `.` parts
/// dropped, `..` kept.
pub fn join(base: &str, name: &str) -> String {
    let joined = if name.starts_with('/') {
        name.to_owned()
    } else {
        format!("{base}/{name}")
    };
    let absolute = joined.starts_with('/');
    let parts: Vec<&str> = joined
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    let body = parts.join("/");
    if absolute {
        format!("/{body}")
    } else if body.is_empty() {
        ".".into()
    } else {
        body
    }
}

/// `posixpath.split`.
fn split(path: &str) -> (String, String) {
    let i = path.rfind('/').map_or(0, |i| i + 1);
    let (head, tail) = path.split_at(i);
    let trimmed = head.trim_end_matches('/');
    let head = if !head.is_empty() && trimmed.is_empty() {
        head.to_owned()
    } else {
        trimmed.to_owned()
    };
    (head, tail.to_owned())
}

/// `posixpath.join(a, b)`.
fn pjoin(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_owned()
    } else if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Resolve {
    Path(String),
    /// `Path.resolve()` raised (a symlink loop is its RuntimeError; a NUL its ValueError).
    Raised,
}

/// `posixpath._joinrealpath(path, rest, strict=False, seen)`: `(path, ok)`.
fn join_realpath(
    mut path: String,
    rest: &str,
    seen: &mut std::collections::HashMap<String, Option<String>>,
    depth: usize,
) -> Result<(String, bool), ()> {
    if depth > 256 {
        return Err(());
    }
    let mut rest = rest.to_owned();
    if rest.starts_with('/') {
        rest.remove(0);
        path = "/".into();
    }
    while !rest.is_empty() {
        let (name, tail) = match rest.split_once('/') {
            Some((n, t)) => (n.to_owned(), t.to_owned()),
            None => (rest.clone(), String::new()),
        };
        rest = tail;
        if name.is_empty() || name == "." {
            continue;
        }
        if name == ".." {
            if path.is_empty() {
                path = "..".into();
            } else {
                let (head, last) = split(&path);
                path = if last == ".." {
                    pjoin(&pjoin(&head, ".."), "..")
                } else {
                    head
                };
            }
            continue;
        }
        let newpath = pjoin(&path, &name);
        let is_link = std::fs::symlink_metadata(&newpath)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if !is_link {
            path = newpath;
            continue;
        }
        if let Some(cached) = seen.get(&newpath) {
            match cached {
                Some(resolved) => {
                    path = resolved.clone();
                    continue;
                }
                None => return Ok((pjoin(&newpath, &rest), false)),
            }
        }
        seen.insert(newpath.clone(), None);
        let target = std::fs::read_link(&newpath).map_err(|_| ())?;
        let target = target.to_str().ok_or(())?.to_owned();
        let (resolved, ok) = join_realpath(path, &target, seen, depth + 1)?;
        if !ok {
            return Ok((pjoin(&resolved, &rest), false));
        }
        seen.insert(newpath, Some(resolved.clone()));
        path = resolved;
    }
    Ok((path, true))
}

/// `Path(p).resolve()` (non-strict) for an absolute `p`, including its symlink-loop check.
pub fn resolve(path: &str) -> Resolve {
    if path.contains('\0') || !path.starts_with('/') {
        return Resolve::Raised;
    }
    let mut seen = std::collections::HashMap::new();
    let Ok((resolved, _)) = join_realpath(String::new(), path, &mut seen, 0) else {
        return Resolve::Raised;
    };
    let resolved = join("/", &resolved);
    // Non-strict resolve stats the result to turn a symlink loop into RuntimeError.
    if let Err(e) = std::fs::metadata(&resolved) {
        if e.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) {
            return Resolve::Raised;
        }
    }
    Resolve::Path(resolved)
}

/// `PurePath.is_relative_to(root)` on two absolute normalised paths.
fn relative_to(path: &str, root: &str) -> bool {
    let p: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let r: Vec<&str> = root.split('/').filter(|s| !s.is_empty()).collect();
    p.starts_with(&r)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Dest {
    /// `settings.models_dir / filename`, unresolved, as Python writes to it.
    Inside(String),
    /// `safe_dest`'s ValueError.
    Outside,
    /// `Path.resolve()` raised.
    Raised,
}

/// `downloader.safe_dest(filename)` for the models folder `models_dir`.
pub fn safe_dest(models_dir: &str, filename: &str) -> Dest {
    let joined = join(models_dir, filename);
    let (Resolve::Path(root), Resolve::Path(dest)) = (resolve(models_dir), resolve(&joined)) else {
        return Dest::Raised;
    };
    if dest == root || !relative_to(&dest, &root) {
        return Dest::Outside;
    }
    Dest::Inside(joined)
}

/// `dest.with_suffix(dest.suffix + ".download")`: always the name plus `.download`.
pub fn temp_path(dest: &str) -> String {
    format!("{dest}.download")
}

/// `_validator_path(job)`.
pub fn validator_path(temp: &str) -> String {
    format!("{temp}.validator")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn human_bytes_matches_utils() {
        assert_eq!(human_bytes(0.0), "0 B");
        assert_eq!(human_bytes(1023.0), "1023 B");
        assert_eq!(human_bytes(1024.0), "1.0 KB");
        assert_eq!(human_bytes(1_572_864.0), "1.5 MB");
        assert_eq!(eta_h(Some(59)), "59s");
        assert_eq!(eta_h(Some(61)), "1m 1s");
        assert_eq!(eta_h(Some(3661)), "1h 1m");
        assert_eq!(eta_h(None), "—");
    }

    #[test]
    fn zones_follow_the_table() {
        for (name, value) in [
            ("AST", -400),
            ("ADT", -300),
            ("EST", -500),
            ("EDT", -400),
            ("CST", -600),
            ("CDT", -500),
            ("MST", -700),
            ("MDT", -600),
            ("PST", -800),
            ("PDT", -700),
            ("GMT", 0),
        ] {
            assert_eq!(zone(name), Some(value), "{name}");
        }
    }

    #[test]
    fn join_is_purepath() {
        assert_eq!(join("/m", "a/./b//c"), "/m/a/b/c");
        assert_eq!(join("/m", "/x/y"), "/x/y");
        assert_eq!(join("/m", "a/../b"), "/m/a/../b");
        assert_eq!(join("/m", "a/"), "/m/a");
    }
}
