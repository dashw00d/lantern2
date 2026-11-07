//! Synchronous, daemon-independent Lantern installation and diagnostics.

pub mod command;
pub mod paths;
pub mod step;

mod file;
/// Individual installation steps.
pub mod steps;
