// The regular expressions the IDNA/hostname validators are built from.

/// The regular expressions the validators are built from.
#[expect(
    clippy::module_inception,
    reason = "the upstream layout has a `pattern/pattern.ts` module, and the port mirrors it"
)]
pub mod pattern;

pub use pattern::*;
