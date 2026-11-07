//! The in-process event bus (docs/01-architecture.md §9, docs/02-decisions.md D7).
//!
//! Controller-owned. One `broadcast` channel of [`Event`] snapshots. Log lines are NOT on this bus:
//! they go through `runtime::journal::LogHub::subscribe_logs`.

use lantern_core::api::Event;
use tokio::sync::broadcast;

/// Default channel capacity. A subscriber that falls this far behind gets `Lagged`, which the SSE
/// handler turns into `resync` + close (02 D7).
pub const DEFAULT_CAPACITY: usize = 1024;

/// Cheap to clone; all clones publish to the same channel.
#[derive(Debug, Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    /// A bus with [`DEFAULT_CAPACITY`].
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// A bus with an explicit capacity (tests use small values to exercise lag).
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publish an event. Having no subscribers is not an error.
    pub fn publish(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    /// Subscribe to events published from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
