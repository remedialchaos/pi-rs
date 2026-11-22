// The format checks of JSON Schema 2020-12, adapted from TypeBox 1.3.27 (MIT).

use regress::Regex;
use std::sync::LazyLock;

/// The number of days of each month.
const DAYS: [u32; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

static DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::with_flags(r##"^(\d\d\d\d)-(\d\d)-(\d\d)$"##, "").expect("the pattern is valid")
});

/// Whether a year is a leap year.
fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

/// Whether the value is an ISO 8601 date component string (RFC 3339).
pub fn is_date(value: &str) -> bool {
    let Some(date) = DATE.find(value) else {
        return false;
    };
    let number = |index: usize| -> u32 {
        date.group(index)
            .map(|range| value[range].parse::<u32>().unwrap_or(0))
            .unwrap_or(0)
    };
    let year = number(1);
    let month = number(2);
    let day = number(3);
    if !(1..=12).contains(&month) {
        return false;
    }
    let days = if month == 2 && is_leap_year(year) {
        29
    } else {
        DAYS[month as usize]
    };
    day >= 1 && day <= days
}
