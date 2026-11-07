//! Listening-server discovery (docs/12-discovery.md).

pub mod source;

mod build;
mod classify;
pub mod hide;
pub mod matcher;
mod owners;
pub mod procinfo;
mod redact;
mod scanner;
pub use scanner::{Scanner, Snapshot};
mod port;
pub use port::{PortBinding, PortConflict, PortHolder};
mod docker;
pub mod sched;
