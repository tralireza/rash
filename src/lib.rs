//! rash — Rust Auto SSH.
//!
//! Starts an `ssh` session or tunnel, monitors it, and restarts it if it dies or
//! stops passing traffic. A behaviour-compatible reimplementation of autossh(1)
//! by Carson Harding.
//!
//! The library exists so the pieces that are pure — argument splitting, config
//! resolution, the exit-status policy, the backoff curve — can be tested without
//! spawning anything.

pub mod backoff;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod log;
pub mod monitor;
pub mod pidfile;
pub mod supervise;
