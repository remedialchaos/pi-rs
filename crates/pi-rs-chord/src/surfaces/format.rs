//! Locale defaults captured from CLDR 48 via Intl.NumberFormat and Intl.DateTimeFormat. The compact data stores distinct decimal and short-time
//! patterns and the locales whose pattern differs from their parent.
use crate::{Error, Result};
use serde::Deserialize;
use std::{collections::HashMap, sync::LazyLock};

#[derive(Deserialize)]
struct Formats {
    locales: HashMap<String, usize>,
    profiles: Vec<Profile>,
}
#[derive(Deserialize)]
pub(super) struct Profile {
    decimal: String,
    group: String,
    primary: usize,
    secondary: usize,
    group_thousands: bool,
    digits: String,
    negative: String,
    time: String,
    cycle: String,
    pad_hour: bool,
    am: String,
    pm: String,
}
static FORMATS: LazyLock<std::result::Result<Formats, serde_json::Error>> =
    LazyLock::new(|| serde_json::from_str(include_str!("format-data.json")));

pub(super) fn profile(locale: &str) -> Result<&'static Profile> {
    let data = FORMATS
        .as_ref()
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let mut tag = locale;
    loop {
        if let Some(profile) = data.locales.get(tag).and_then(|i| data.profiles.get(*i)) {
            return Ok(profile);
        }
        if let Some((parent, _)) = tag.rsplit_once('-') {
            tag = parent;
        } else {
            break;
        }
    }
    data.locales
        .get("en")
        .and_then(|i| data.profiles.get(*i))
        .ok_or_else(|| Error::Invalid("missing default locale data".into()))
}

fn current_profile(category: &str) -> Result<&'static Profile> {
    if let Some(language) = super::current_language() {
        return Ok(language.format);
    }
    let locale = ["LC_ALL", category, "LANG"]
        .into_iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|s| !s.is_empty())
        .unwrap_or_default();
    profile(&locale.split('.').next().unwrap_or("en").replace('_', "-"))
}

impl Profile {
    fn localize(&self, ascii: &str) -> String {
        ascii
            .chars()
            .map(|c| {
                c.to_digit(10)
                    .and_then(|d| self.digits.chars().nth(d as usize))
                    .unwrap_or(c)
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct NumberFormatOptions {
    pub minimum_fraction_digits: usize,
    pub maximum_fraction_digits: usize,
    pub use_grouping: bool,
}
impl Default for NumberFormatOptions {
    fn default() -> Self {
        Self {
            minimum_fraction_digits: 0,
            maximum_fraction_digits: 3,
            use_grouping: true,
        }
    }
}
pub fn format_number(value: f64, options: Option<NumberFormatOptions>) -> Result<String> {
    let p = current_profile("LC_NUMERIC")?;
    let o = options.unwrap_or_default();
    if o.maximum_fraction_digits > 100 || o.minimum_fraction_digits > o.maximum_fraction_digits {
        return Err(Error::Invalid("invalid fraction digit range".into()));
    }
    let sign = if value.is_sign_negative() {
        p.negative.as_str()
    } else {
        ""
    };
    if !value.is_finite() {
        return Ok(if value.is_nan() {
            "NaN".into()
        } else {
            format!("{sign}∞")
        });
    }
    // Round decimal digits half away from zero, including values whose binary representation lies just below a tie.
    let decimal = value.abs().to_string();
    let (integer, fraction) = decimal.split_once('.').unwrap_or((&decimal, ""));
    let mut digits = integer.to_owned();
    digits.push_str(&fraction[..fraction.len().min(o.maximum_fraction_digits)]);
    if fraction
        .as_bytes()
        .get(o.maximum_fraction_digits)
        .is_some_and(|c| *c >= b'5')
    {
        let carried = digits.trim_end_matches('9').len();
        let zeroes = digits.len() - carried;
        digits.truncate(carried);
        if let Some(last) = digits.pop() {
            digits.push(char::from(last as u8 + 1));
        } else {
            digits.push('1');
        }
        digits.extend(std::iter::repeat_n('0', zeroes));
    }
    let decimals = fraction.len().min(o.maximum_fraction_digits);
    let (whole, frac) = digits.split_at(digits.len() - decimals);
    let mut formatted = sign.to_owned();
    for (i, ch) in whole.char_indices() {
        let remaining = whole.len() - i;
        if i > 0
            && o.use_grouping
            && (whole.len() > p.primary + 1 || p.group_thousands)
            && remaining >= p.primary
            && (remaining - p.primary).is_multiple_of(p.secondary)
        {
            formatted.push_str(&p.group);
        }
        formatted.push(ch);
    }
    let frac = frac.trim_end_matches('0');
    if !frac.is_empty() || o.minimum_fraction_digits > 0 {
        formatted.push_str(&p.decimal);
        formatted.push_str(frac);
        formatted.extend(std::iter::repeat_n(
            '0',
            o.minimum_fraction_digits.saturating_sub(frac.len()),
        ));
    }
    Ok(p.localize(&formatted))
}

#[derive(Default, Clone, Copy)]
pub struct TimeFormatOptions {
    /// Use this offset in minutes east of UTC. The host time zone applies by default.
    pub offset_minutes: Option<i32>,
}

pub fn format_time(at: impl Into<TimeValue>, options: Option<TimeFormatOptions>) -> Result<String> {
    let at = match at.into() {
        TimeValue::Milliseconds(ms) => ms.div_euclid(1000),
        TimeValue::Iso(text) => {
            iso_seconds(&text).ok_or_else(|| Error::Invalid("invalid ISO date or time".into()))?
        }
    };
    let offset = options
        .and_then(|o| o.offset_minutes)
        .map(|m| i64::from(m) * 60)
        .unwrap_or_else(|| local_offset(at));
    let seconds = (at.rem_euclid(86400) + offset).rem_euclid(86400);
    let hour = seconds / 3600;
    let p = current_profile("LC_TIME")?;
    let h = match p.cycle.as_str() {
        "h11" => hour % 12,
        "h12" => (hour + 11) % 12 + 1,
        "h24" if hour == 0 => 24,
        _ => hour,
    };
    let h = if p.pad_hour {
        format!("{h:02}")
    } else {
        h.to_string()
    };
    Ok(p.localize(
        &p.time
            .replace("{h}", &h)
            .replace("{m}", &format!("{:02}", seconds / 60 % 60))
            .replace("{p}", if hour < 12 { &p.am } else { &p.pm }),
    ))
}
pub enum TimeValue {
    Milliseconds(i64),
    Iso(String),
}
impl From<i64> for TimeValue {
    fn from(value: i64) -> Self {
        Self::Milliseconds(value)
    }
}
impl From<&str> for TimeValue {
    fn from(value: &str) -> Self {
        Self::Iso(value.into())
    }
}
impl From<String> for TimeValue {
    fn from(value: String) -> Self {
        Self::Iso(value)
    }
}

fn iso_seconds(text: &str) -> Option<i64> {
    let (date, clock) = text
        .split_once('T')
        .map_or((text, None), |(d, c)| (d, Some(c)));
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    if date.next().is_some()
        || !(0..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=[
            31,
            28 + i64::from(leap),
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ][month as usize - 1])
            .contains(&day)
    {
        return None;
    }
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let y = year.rem_euclid(400);
    let days = era * 146097 + y * 365 + y / 4 - y / 100
        + (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5
        + day
        - 1
        - 719468;
    let Some(clock) = clock else {
        return Some(days * 86400);
    };
    let (clock, offset) = if let Some(clock) = clock.strip_suffix('Z') {
        (clock, Some(0))
    } else if let Some(i) = clock.find(['+', '-']) {
        let z = &clock[i + 1..];
        let (h, m) = z.split_once(':')?;
        let h: i64 = h.parse().ok()?;
        let m: i64 = m.parse().ok()?;
        if !(0..24).contains(&h) || !(0..60).contains(&m) {
            return None;
        }
        (
            &clock[..i],
            Some((h * 60 + m) * 60 * if clock.as_bytes()[i] == b'+' { 1 } else { -1 }),
        )
    } else {
        (clock, None)
    };
    let mut parts = clock.split(':');
    let hour: i64 = parts.next()?.parse().ok()?;
    let minute: i64 = parts.next()?.parse().ok()?;
    let second = parts.next().unwrap_or("0");
    let second = if let Some((second, fraction)) = second.split_once('.') {
        if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        second
    } else {
        second
    };
    let second: i64 = second.parse().ok()?;
    if parts.next().is_some()
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..60).contains(&second)
    {
        return None;
    }
    let t = days * 86400 + hour * 3600 + minute * 60 + second;
    Some(t - offset.unwrap_or_else(|| local_offset(t)))
}

// TZif stores ordered Unix transition times and an index into the offset table.
fn local_offset(at: i64) -> i64 {
    let path = match std::env::var("TZ") {
        Ok(name) if !name.is_empty() => {
            std::path::Path::new("/usr/share/zoneinfo").join(name.trim_start_matches(':'))
        }
        _ => std::path::PathBuf::from("/etc/localtime"),
    };
    std::fs::read(path)
        .ok()
        .and_then(|b| zone_offset(&b, at))
        .unwrap_or(0)
}
fn zone_offset(bytes: &[u8], at: i64) -> Option<i64> {
    fn word(b: &[u8], n: usize) -> Option<usize> {
        Some(u32::from_be_bytes(b.get(n..n + 4)?.try_into().ok()?) as usize)
    }
    if bytes.get(..4)? != b"TZif" {
        return None;
    }
    let mut header = bytes;
    let mut width = 4;
    if bytes.get(4) != Some(&0) {
        let mut end = 44usize;
        for (field, size) in [(32, 5), (36, 6), (40, 1), (28, 8), (24, 1), (20, 1)] {
            end = end.checked_add(word(bytes, field)?.checked_mul(size)?)?;
        }
        header = bytes.get(end..)?;
        width = 8;
    }
    let count = word(header, 32)?;
    let data = header.get(44..)?;
    let times_end = count.checked_mul(width)?;
    let indices_end = times_end.checked_add(count)?;
    let times = data.get(..times_end)?;
    let indices = data.get(times_end..indices_end)?;
    let mut kind = 0;
    for (time, index) in times.chunks_exact(width).zip(indices) {
        let t = if width == 4 {
            i64::from(i32::from_be_bytes(time.try_into().ok()?))
        } else {
            i64::from_be_bytes(time.try_into().ok()?)
        };
        if t > at {
            break;
        }
        kind = usize::from(*index);
    }
    let pos = indices_end.checked_add(kind * 6)?;
    Some(i64::from(i32::from_be_bytes(
        data.get(pos..pos.checked_add(4)?)?.try_into().ok()?,
    )))
}
