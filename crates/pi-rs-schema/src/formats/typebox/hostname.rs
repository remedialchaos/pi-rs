// The format checks of JSON Schema 2020-12, adapted from TypeBox 1.3.27 (MIT).

use crate::formats::typebox::idna;

/// Whether the value is a valid hostname (RFC 1123, RFC 5891, RFC 5892).
pub fn is_hostname(value: &str) -> bool {
    idna::is_hostname(value)
}
