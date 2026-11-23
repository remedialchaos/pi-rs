//! p3's built-in plugins, compiled in as native facets (ADR-002): `control` (the owner's stop and
//! apply), the runner (session workers, and the worker's idle rule), `sessions`, and `sign-in`.

pub mod control;
pub mod runner_server;
pub mod runner_worker;
pub mod sessions;
pub mod sign_in;
