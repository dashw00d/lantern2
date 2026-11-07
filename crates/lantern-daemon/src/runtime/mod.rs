//! Supervisor (docs/10-runtime.md). Owner: lane `runtime`.

/// Launcher script generation and toolchain detection.
pub mod launcher;

/// Login-shell environment capture and cache.
pub mod shell_env;

/// systemd user-unit boundary and test fake.
pub mod systemd;

/// Journal decoding and per-instance log subscriptions.
pub mod journal;
