//! What p3 is to its base and its tests: its API (the package and its versions), its facet kinds,
//! and its principals as links stamp them. In this port the API's modules are Rust modules of this
//! crate: plugins are compiled in as native facets (ADR-002), so there are no per-process module
//! tables (`modules-server.ts` / `modules-worker.ts` have no counterpart).

use crate::principals::Principal;

/// p3's API: its package and its versions (newest first).
pub const API_NAME: &str = "@earendil-works/p3";
pub const API_VERSIONS: &[&str] = &["v1"];

/// The facet kinds a plugin may have: a server facet, a worker facet, or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Server,
    Worker,
}

/// The platform and host packages of each facet kind, as the plugin loader reports them.
pub fn host_packages(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Server => &["@earendil-works/p3/v1", "@earendil-works/p3/v1/server"],
        Kind::Worker => &["@earendil-works/p3/v1", "@earendil-works/p3/v1/worker"],
    }
}

/// The server's principal.
pub const SERVER: Principal = Principal::Server;

/// The principal of session `id`'s worker.
pub fn worker_principal(id: impl Into<String>) -> Principal {
    Principal::worker(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_facet_kind_names_the_api_modules_it_may_import() {
        assert_eq!(
            host_packages(Kind::Server),
            ["@earendil-works/p3/v1", "@earendil-works/p3/v1/server"]
        );
        assert_eq!(
            host_packages(Kind::Worker),
            ["@earendil-works/p3/v1", "@earendil-works/p3/v1/worker"]
        );
        assert_eq!(SERVER, Principal::Server);
        assert_eq!(worker_principal("s1"), Principal::worker("s1"));
        assert_eq!(API_NAME, "@earendil-works/p3");
        assert_eq!(API_VERSIONS, ["v1"]);
    }
}
