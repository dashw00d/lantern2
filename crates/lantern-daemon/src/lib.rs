//! Everything that runs inside `lantern daemon` (docs/01-architecture.md §4).
//! Module owners are listed on the blackboard (`/home/ryan/dev/lantern-agents/BLACKBOARD.md`).

pub mod api;
pub mod discovery;
pub mod events;
pub mod git;
pub mod instances;
pub mod operations;
pub mod registry;
pub mod runtime;
