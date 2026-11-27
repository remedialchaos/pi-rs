//! `p3` server, worker, supervisor, v1 API, and built-in plugins (Milestone 3.1).
//!
//! A Pi is a data directory (`~/.pi/p3`, `~/.pi/p3/pis/<name>`, or any `--dir`). The `p3` command
//! starts one in the background under a supervisor, runs it in the foreground, or asks about it.
//! A session's worker runs as a child process of the server; control messages travel as
//! newline-delimited JSON envelopes (`{"p3": ...}`) on the worker's stdio.
//!
//! The base is wired onto `pi-rs-chord`: `node::http` serves the HTTP component (`/_p3/...` routes
//! and the `/chord` link endpoint), `node::app` provides private files, `node::workers::ProcessLock`
//! provides the data directory's locks, and `host` loads the built-in facets (ADR-002: plugins are
//! compiled in as native Rust facets).
//!
//! `pi-rs-chord` does not yet route objects by host address (the `2.1b-3` orchestration gap), so
//! `p3` keeps one local seam for it: [`server::SessionRouter`] decides whether a session exists,
//! admits a principal, and asks a [`runner::WorkerRunner`] to start its worker. See `MILESTONE.md`.
#![deny(unsafe_code)]

#[cfg(unix)]
mod unix_signal;
#[cfg(windows)]
mod windows_signal;

pub mod api;
pub mod app;
pub mod cli;
pub mod client;
pub mod files;
pub mod http;
pub mod install;
pub mod pi;
pub mod plugins;
pub mod principals;
pub mod records;
pub mod registry;
pub mod runner;
pub mod server;
pub mod server_core;
pub mod supervisor;
pub mod worker;
pub mod worker_core;
