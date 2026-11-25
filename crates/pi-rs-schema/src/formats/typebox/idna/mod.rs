// The IDNA and hostname validators, ported from `packages/schema/src/formats/typebox/idna`.
//
// The upstream package validates hostnames in two flavours: `IsHostname` accepts ASCII hostnames only, and
// `IsIdnHostname` accepts internationalized hostnames, normalising the hostname first.

/// The encodings and character rules the validators are built from.
pub mod format;
/// The ASCII hostname rules.
pub mod hostname;
/// The IDN hostname rules.
pub mod idn_hostname;
/// The label rules the hostname rules are built from.
pub mod label;
/// The regular expressions the validators are built from.
pub mod pattern;

pub use hostname::is_hostname;
pub use idn_hostname::{is_idn_hostname, normalize_hostname};
