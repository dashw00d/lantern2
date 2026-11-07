//! Synchronous, daemon-independent Lantern installation and diagnostics.

pub mod command;
pub mod paths;
pub mod step;

/// Creation and inspection of Lantern's constrained root CA.
pub mod ca;
/// Read-only, daemon-independent health checks.
pub mod doctor;
mod file;
/// Composition of user and privileged installation steps.
pub mod init;
/// Individual installation steps.
pub mod steps;
