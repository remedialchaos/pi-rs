#![forbid(unsafe_code)]

// One linked binary for all contracts; the library embeds the codemode runtime.
#[path = "contracts/codemode.rs"]
mod codemode;
#[path = "contracts/codemode_session.rs"]
mod codemode_session;
#[path = "contracts/core.rs"]
mod core;
#[path = "contracts/env_conformance.rs"]
mod env_conformance;
#[path = "contracts/images.rs"]
mod images;
#[path = "contracts/session.rs"]
mod session;
#[path = "contracts/storage_backends.rs"]
mod storage_backends;
#[path = "contracts/storage_benchmark.rs"]
mod storage_benchmark;
#[path = "contracts/storage_conformance.rs"]
mod storage_conformance;
#[path = "contracts/tools.rs"]
mod tools;
