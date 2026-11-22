// The format checks of JSON Schema 2020-12, adapted from TypeBox 1.3.27 (MIT).

use crate::formats::typebox::date::is_date;
use crate::formats::typebox::time::is_time;

/// Whether the value is an ISO date-time string (RFC 3339).
pub fn is_date_time(value: &str) -> bool {
    let mut parts = value.split(['T', 't']);
    let (Some(date), Some(time), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    is_date(date) && is_time(time)
}
