// The format checks of JSON Schema 2020-12, for compile()'s `formats` option. An optional entry point: schemas
// compiled without it treat `format` as an annotation and do not bundle these checks.

pub mod typebox;

use crate::types::FormatCheck;
use indexmap::IndexMap;
use std::sync::Arc;

/// Every format JSON Schema 2020-12 defines, by name, plus `url` and `json-pointer-uri-fragment`, which TypeBox
/// and ajv-formats also check. The checks are adapted from TypeBox 1.3.27 (MIT).
pub fn formats() -> IndexMap<String, FormatCheck> {
    let mut formats: IndexMap<String, FormatCheck> = IndexMap::new();
    formats.insert("date".to_string(), check(typebox::date::is_date));
    formats.insert(
        "date-time".to_string(),
        check(typebox::date_time::is_date_time),
    );
    formats.insert(
        "duration".to_string(),
        check(typebox::duration::is_duration),
    );
    formats.insert("email".to_string(), check(typebox::email::is_email));
    formats.insert(
        "hostname".to_string(),
        check(typebox::hostname::is_hostname),
    );
    formats.insert(
        "idn-email".to_string(),
        check(typebox::idn_email::is_idn_email),
    );
    formats.insert(
        "idn-hostname".to_string(),
        check(typebox::idn_hostname::is_idn_hostname),
    );
    formats.insert("ipv4".to_string(), check(typebox::ipv4::is_ipv4));
    formats.insert("ipv6".to_string(), check(typebox::ipv6::is_ipv6));
    formats.insert("iri".to_string(), check(typebox::iri::is_iri));
    formats.insert(
        "iri-reference".to_string(),
        check(typebox::iri_reference::is_iri_reference),
    );
    formats.insert(
        "json-pointer".to_string(),
        check(typebox::json_pointer::is_json_pointer),
    );
    formats.insert(
        "json-pointer-uri-fragment".to_string(),
        check(typebox::json_pointer_uri_fragment::is_json_pointer_uri_fragment),
    );
    formats.insert("regex".to_string(), check(typebox::regex::is_regex));
    formats.insert(
        "relative-json-pointer".to_string(),
        check(typebox::relative_json_pointer::is_relative_json_pointer),
    );
    formats.insert("time".to_string(), check(typebox::time::is_time));
    formats.insert("uri".to_string(), check(typebox::uri::is_uri));
    formats.insert(
        "uri-reference".to_string(),
        check(typebox::uri_reference::is_uri_reference),
    );
    formats.insert(
        "uri-template".to_string(),
        check(typebox::uri_template::is_uri_template),
    );
    formats.insert("url".to_string(), check(typebox::url::is_url));
    formats.insert("uuid".to_string(), check(typebox::uuid::is_uuid));
    formats
}

/// A format check as compile() takes it.
fn check(function: fn(&str) -> bool) -> FormatCheck {
    Arc::new(function)
}
