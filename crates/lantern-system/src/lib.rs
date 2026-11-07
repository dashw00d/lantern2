//! Synchronous, daemon-independent Lantern installation and diagnostics.

pub mod command;
pub mod paths;
pub mod step;

/// Read-only, daemon-independent health checks.
pub mod doctor;
mod file;
/// Individual installation steps.
pub mod steps;
