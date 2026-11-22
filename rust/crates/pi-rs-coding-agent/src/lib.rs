//! Standalone foundation of the pi coding agent: the Rust port of the `pi-ai`-independent
//! subsystems of `packages/coding-agent`. The AI-dependent session runtime, modes, and the `pi`
//! front door are added by Milestone 1.8b.

#![forbid(unsafe_code)]

pub mod config;
pub mod core;
pub mod extensions;
pub mod lockfile;
pub mod migrations;
pub mod utils;
