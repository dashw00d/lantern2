//! In-process state events and their SSE representation.

use serde::{Deserialize, Serialize};

use super::{InstanceView, OperationId, ProjectSummary, ServiceStatus, Suggestion, SystemHealth};
use crate::{
    Timestamp,
    ids::{InstanceId, ProjectId},
    model::{Listener, LogLine},
};

macro_rules! define_events {
    ($($(#[$doc:meta])* $variant:ident { $($field:ident: $ty:ty),* $(,)? } => $name:literal),* $(,)?) => {
        /// A snapshot-bearing state change on the in-process bus; log lines are separate.
        // The public bus contract deliberately carries complete views by value.
        #[allow(clippy::large_enum_variant)]
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub enum Event {
            $($(#[$doc])* $variant { $(#[doc = concat!("Event field `", stringify!($field), "`.")] $field: $ty),* }),*
        }
        /// An SSE message. Its `type` field matches its SSE `event:` name.
        // Keep the SSE payload identical to the public bus contract.
        #[allow(clippy::large_enum_variant)]
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub enum WireEvent {
            /// Protocol/version greeting, first in a snapshot.
            Hello { /// Daemon API version.
                api_version: u32, /// Daemon build version.
                daemon_version: String, /// Identifier for this daemon run.
                boot_id: String, /// Current daemon time.
                now: Timestamp },
            $($(#[$doc])* $variant { $(#[doc = concat!("Event field `", stringify!($field), "`.")] $field: $ty),* },)*
            /// A log line; logs do not travel on the in-process event bus.
            Log { /// Instance, stream, text, and timestamp, flattened on the wire.
                #[serde(flatten)] line: LogLine },
            /// The initial state and log backlog are complete.
            SnapshotComplete,
            /// The client must reconnect to receive a fresh snapshot.
            Resync { /// Reason the snapshot is no longer reliable.
                reason: String, /// Count of skipped messages, when known.
                skipped: Option<u64> },
        }
        impl Event {
            /// Return the standard SSE `event:` name for this state event.
            pub fn event_name(&self) -> &'static str {
                match self { $(Self::$variant { .. } => $name,)* }
            }
        }
        impl WireEvent {
            /// Return the standard SSE `event:` name for this message.
            pub fn event_name(&self) -> &'static str {
                match self {
                    Self::Hello { .. } => "hello",
                    $(Self::$variant { .. } => $name,)*
                    Self::Log { .. } => "log",
                    Self::SnapshotComplete => "snapshot_complete",
                    Self::Resync { .. } => "resync",
                }
            }
        }
        impl From<Event> for WireEvent {
            fn from(event: Event) -> Self {
                match event { $(Event::$variant { $($field),* } => Self::$variant { $($field),* },)* }
            }
        }
    };
}

define_events! {
    /// A project's full summary changed.
    ProjectUpdated { project: ProjectSummary } => "project_updated",
    /// A project was removed.
    ProjectRemoved { id: ProjectId } => "project_removed",
    /// An instance's full view changed.
    InstanceUpdated { instance: InstanceView } => "instance_updated",
    /// An instance was removed.
    InstanceRemoved { id: InstanceId } => "instance_removed",
    /// The non-hidden listener snapshot changed.
    ListenersChanged { seq: u64, listeners: Vec<Listener> } => "listeners_changed",
    /// The current suggestion snapshot changed.
    SuggestionsChanged { suggestions: Vec<Suggestion> } => "suggestions_changed",
    /// A shared service changed.
    ServiceUpdated { service: ServiceStatus } => "service_updated",
    /// The system health snapshot changed.
    SystemHealth { health: SystemHealth } => "system_health",
    /// A long operation made progress.
    OperationProgress { op: OperationId, instance: InstanceId, step: String, done: bool, error: Option<String> } => "operation_progress",
}

/// Optional filters for the SSE endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventParams {
    /// Include state events and the initial snapshot (default true).
    #[serde(default = "super::default_true")]
    pub state: bool,
    /// Instance ids for which log lines should be streamed.
    #[serde(default)]
    pub logs: Vec<InstanceId>,
    /// Number of backlog lines per instance.
    pub tail: Option<u32>,
    /// Include progress for this operation in a log-only stream.
    pub op: Option<OperationId>,
}
impl Default for EventParams {
    fn default() -> Self {
        Self {
            state: true,
            logs: Vec::new(),
            tail: None,
            op: None,
        }
    }
}
