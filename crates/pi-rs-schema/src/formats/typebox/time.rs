// The format checks of JSON Schema 2020-12, adapted from TypeBox 1.3.27 (MIT).

use crate::formats::typebox::date::is_date;
use regress::Regex;
use std::sync::LazyLock;

static TIME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::with_flags(
        r##"^(\d\d):(\d\d):(\d\d)(?:\.\d+)?(?:([Zz])|([+-])(\d\d):(\d\d))?$"##,
        "",
    )
    .expect("the pattern is valid")
});

/// The number a capture group holds.
fn group_number(time: &regress::Match, value: &str, index: usize) -> u32 {
    time.group(index)
        .map(|range| value[range].parse::<u32>().unwrap_or(0))
        .unwrap_or(0)
}

/// Whether the value is an ISO time string (RFC 3339).
pub fn is_time(value: &str) -> bool {
    let Some(time) = TIME.find(value) else {
        return false;
    };
    let hour = group_number(&time, value, 1);
    let minute = group_number(&time, value, 2);
    let second = group_number(&time, value, 3);
    let zulu = time.group(4).is_some();
    let sign = time.group(5).map(|range| &value[range]).unwrap_or("");
    let offset_hour = group_number(&time, value, 6);
    let offset_minute = group_number(&time, value, 7);
    if hour > 23 || minute > 59 || second > 60 {
        return false;
    }
    if !zulu && sign.is_empty() {
        // A time without an offset is not an RFC 3339 time.
        return false;
    }
    if !sign.is_empty() && (offset_hour > 23 || offset_minute > 59) {
        return false;
    }
    if second < 60 {
        return true;
    }
    // Leap second handling: must normalize to 23:59:60 UTC (1439 total UTC minutes).
    let sign = if sign == "-" { -1 } else { 1 };
    let total =
        hour as i64 * 60 + minute as i64 - sign * (offset_hour as i64 * 60 + offset_minute as i64);
    ((total % 1440) + 1440) % 1440 == 1439
}

/// Whether the value is an ISO date-time string (RFC 3339).
pub fn is_date_time(value: &str) -> bool {
    let mut parts = value.split(['T', 't']);
    let (Some(date), Some(time), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    is_date(date) && is_time(time)
}
