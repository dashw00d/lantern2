//! Pure domain library shared by every Lantern crate: no tokio, no network, no process spawning.
//! See docs/01-architecture.md §4 and §6, and docs/02-decisions.md.
//!
//! Module owners are listed on the blackboard. `ids` and `model` are controller-owned:
//! lanes request changes via an `ask, controller:` entry under Contracts.

pub mod api;
pub mod detect;
pub mod ids;
pub mod manifest;
pub mod model;
pub mod profiles;
pub mod settings;
pub mod state;
pub mod template;
pub mod yaml;

/// RFC 3339 UTC timestamp used throughout the domain model and on the wire.
pub type Timestamp = jiff::Timestamp;

/// XDG / `LANTERN_*_DIR` directory resolution (re-export of `settings::Dirs`).
pub mod dirs {
    pub use crate::settings::Dirs;
}
