//! Evaluation planner, harness, docker runner, and comparison reporter (Milestone 2.4).
#![forbid(unsafe_code)]

pub mod cli;
pub mod docker;
pub mod error;
pub mod harness;
pub mod plan;
pub mod report;
pub mod runner;
pub mod suites;
