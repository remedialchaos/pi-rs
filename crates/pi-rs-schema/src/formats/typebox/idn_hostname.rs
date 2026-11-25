// The format checks of JSON Schema 2020-12, adapted from TypeBox 1.3.27 (MIT).

use crate::formats::typebox::idna;

/// Whether the value is a valid internationalized hostname (RFC 3490, RFC 5891, RFC 5892, RFC 5893).
pub fn is_idn_hostname(value: &str) -> bool {
    idna::is_idn_hostname(value)
}
