//! Finite Python repr(float) spelling, qualified against CPython by the source differential.
pub(crate) fn python_float(value: f64) -> Option<String> {
    if !value.is_finite() {
        return None;
    }
    if value == 0.0 {
        return Some(
            if value.is_sign_negative() {
                "-0.0"
            } else {
                "0.0"
            }
            .into(),
        );
    }
    let negative = value.is_sign_negative();
    let mut buffer = ryu::Buffer::new();
    let raw = buffer.format_finite(value.abs());
    let (mantissa, explicit) = match raw.split_once('e') {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i32>().ok()?),
        None => (raw, 0),
    };
    let raw = mantissa;
    let decimal = raw.find('.').unwrap_or(raw.len());
    let digits: String = raw.chars().filter(|c| *c != '.').collect();
    let first = digits.find(|c| c != '0')?;
    let exponent = i32::try_from(decimal).ok()? - i32::try_from(first).ok()? - 1 + explicit;
    let significant = digits.get(first..)?.trim_end_matches('0');
    let sign = if negative { "-" } else { "" };
    if !(-4..16).contains(&exponent) {
        let (head, tail) = significant.split_at_checked(1)?;
        let fraction = if tail.is_empty() {
            String::new()
        } else {
            format!(".{tail}")
        };
        Some(format!(
            "{sign}{head}{fraction}e{}{magnitude:02}",
            if exponent < 0 { "-" } else { "+" },
            magnitude = exponent.unsigned_abs()
        ))
    } else {
        let point = exponent + 1;
        let number = if point <= 0 {
            format!(
                "0.{}{}",
                "0".repeat(usize::try_from(-point).ok()?),
                significant
            )
        } else {
            let point = usize::try_from(point).ok()?;
            if point >= significant.len() {
                format!("{}{}.0", significant, "0".repeat(point - significant.len()))
            } else {
                let (head, tail) = significant.split_at_checked(point)?;
                format!("{head}.{tail}")
            }
        };
        Some(format!("{sign}{number}"))
    }
}
