//! Operation tracking (docs/15-api-mcp.md §3.4, docs/02-decisions.md D8/D18).
//!
//! Controller-owned. Every long action (start, stop, prepare, create instance, ...) gets an
//! [`OpHandle`]. Workers report steps and the final result through it; the API waits on it
//! (`?wait=true`) and returns `Operation` snapshots. Per-instance *policy* (join / preempt / 409)
//! is decided by the caller (runtime `state_machine::request_policy`); this module only offers
//! [`OperationTracker::active_for`] and cancellation tokens to implement it.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lantern_core::Timestamp;
use lantern_core::api::{
    ApiError, Event, Operation, OperationId, OperationKind, OperationStatus, OperationStep,
};
use lantern_core::ids::InstanceId;
use serde_json::Value;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::events::EventBus;

/// Finished operations kept for `GET /api/operations/{id}`.
pub const RETAIN_FINISHED: usize = 200;
/// How long finished operations stay queryable (15 §3.4: 1 hour).
pub const RETAIN_FOR: Duration = Duration::from_secs(3600);

#[derive(Debug)]
struct Entry {
    op: Operation,
    cancel: CancellationToken,
    tx: watch::Sender<Operation>,
}

#[derive(Debug, Default)]
struct Inner {
    seq: u64,
    ops: HashMap<OperationId, Entry>,
    finished: VecDeque<OperationId>,
}

/// Shared registry of operations. Cheap to clone.
#[derive(Debug, Clone)]
pub struct OperationTracker {
    inner: Arc<Mutex<Inner>>,
    bus: EventBus,
    retain_for: Duration,
}

/// Owned by the worker performing the operation. Dropping it without [`OpHandle::finish`] marks the
/// operation `Failed` with code `internal` (so a panicking worker never leaves it `Running`).
#[derive(Debug)]
pub struct OpHandle {
    id: OperationId,
    tracker: OperationTracker,
    cancel: CancellationToken,
    finished: bool,
}

/// Outcome passed to [`OpHandle::finish`].
#[derive(Debug, Clone)]
pub enum OpOutcome {
    /// Succeeded, with an optional JSON result.
    Succeeded(Option<Value>),
    /// Nothing to do (already in the requested state): `noop = true`, succeeded.
    Noop,
    /// Failed with a stable error.
    Failed(ApiError),
    /// Cancelled (e.g. preempted by Stop, 02 D18).
    Cancelled,
}

impl OperationTracker {
    /// A tracker publishing `OperationProgress` events on `bus`.
    pub fn new(bus: EventBus) -> Self {
        Self::with_retention(bus, RETAIN_FOR)
    }

    /// A tracker with a custom retention period for finished operations (tests).
    pub fn with_retention(bus: EventBus, retain_for: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            bus,
            retain_for,
        }
    }

    /// Drop finished operations older than the retention period. `finished` is in finish order.
    fn prune(&self, inner: &mut Inner) {
        let Ok(span) = jiff::SignedDuration::try_from(self.retain_for) else {
            return;
        };
        let Ok(cutoff) = Timestamp::now().checked_sub(span) else {
            return;
        };
        while let Some(front) = inner.finished.front() {
            let expired = inner
                .ops
                .get(front)
                .and_then(|e| e.op.finished_at)
                .is_none_or(|at| at < cutoff);
            if !expired {
                break;
            }
            if let Some(old) = inner.finished.pop_front() {
                inner.ops.remove(&old);
            }
        }
    }

    /// Register a new running operation.
    pub fn begin(&self, kind: OperationKind, instance: Option<InstanceId>) -> OpHandle {
        let cancel = CancellationToken::new();
        let mut inner = self.inner.lock().expect("operations lock");
        self.prune(&mut inner);
        inner.seq += 1;
        let id = OperationId(format!("op-{}-{}", Timestamp::now().as_second(), inner.seq));
        let op = Operation {
            id: id.clone(),
            kind,
            instance,
            status: OperationStatus::Running,
            noop: false,
            step: None,
            steps: Vec::new(),
            created_at: Timestamp::now(),
            finished_at: None,
            error: None,
            result: None,
        };
        let (tx, _) = watch::channel(op.clone());
        inner.ops.insert(
            id.clone(),
            Entry {
                op,
                cancel: cancel.clone(),
                tx,
            },
        );
        OpHandle {
            id,
            tracker: self.clone(),
            cancel,
            finished: false,
        }
    }

    /// Current snapshot of an operation.
    pub fn get(&self, id: &OperationId) -> Option<Operation> {
        let mut inner = self.inner.lock().expect("operations lock");
        self.prune(&mut inner);
        inner.ops.get(id).map(|e| e.op.clone())
    }

    /// All retained operations, newest first.
    pub fn list(&self) -> Vec<Operation> {
        let mut inner = self.inner.lock().expect("operations lock");
        self.prune(&mut inner);
        let mut v: Vec<Operation> = inner.ops.values().map(|e| e.op.clone()).collect();
        v.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        v
    }

    /// Running (not finished) operations for an instance.
    pub fn active_for(&self, instance: &InstanceId) -> Vec<Operation> {
        let inner = self.inner.lock().expect("operations lock");
        inner
            .ops
            .values()
            .filter(|e| e.op.instance.as_ref() == Some(instance) && is_active(e.op.status))
            .map(|e| e.op.clone())
            .collect()
    }

    /// Request cancellation of a running operation. Returns false if unknown or finished.
    pub fn cancel(&self, id: &OperationId) -> bool {
        let inner = self.inner.lock().expect("operations lock");
        match inner.ops.get(id) {
            Some(e) if is_active(e.op.status) => {
                e.cancel.cancel();
                true
            }
            _ => false,
        }
    }

    /// Wait until the operation finishes or `timeout` elapses; returns the latest snapshot
    /// (still `Running` on timeout). `None` if the id is unknown.
    pub async fn wait(&self, id: &OperationId, timeout: Duration) -> Option<Operation> {
        let mut rx = {
            let inner = self.inner.lock().expect("operations lock");
            inner.ops.get(id)?.tx.subscribe()
        };
        let finished = |op: &Operation| !is_active(op.status);
        let _ = tokio::time::timeout(timeout, rx.wait_for(finished)).await;
        let latest = rx.borrow().clone();
        Some(latest)
    }

    fn update(&self, id: &OperationId, f: impl FnOnce(&mut Operation)) -> Option<Operation> {
        let mut inner = self.inner.lock().expect("operations lock");
        let entry = inner.ops.get_mut(id)?;
        f(&mut entry.op);
        let snap = entry.op.clone();
        entry.tx.send_replace(snap.clone());
        if !is_active(snap.status) {
            inner.finished.push_back(id.clone());
            while inner.finished.len() > RETAIN_FINISHED {
                if let Some(old) = inner.finished.pop_front() {
                    inner.ops.remove(&old);
                }
            }
        }
        Some(snap)
    }

    fn publish_progress(&self, op: &Operation, step: &str, done: bool) {
        if let Some(instance) = &op.instance {
            self.bus.publish(Event::OperationProgress {
                op: op.id.clone(),
                instance: instance.clone(),
                step: step.to_string(),
                done,
                error: op.error.as_ref().map(|e| e.message.clone()),
            });
        }
    }
}

fn is_active(s: OperationStatus) -> bool {
    matches!(s, OperationStatus::Queued | OperationStatus::Running)
}

impl OpHandle {
    /// This operation's id.
    pub fn id(&self) -> &OperationId {
        &self.id
    }

    /// Token cancelled when the operation is cancelled or preempted. Workers must select on it.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Record and publish a new step (finishing the previous one).
    pub fn step(&self, name: &str) {
        let now = Timestamp::now();
        if let Some(op) = self.tracker.update(&self.id, |op| {
            if let Some(last) = op.steps.last_mut()
                && last.status == OperationStatus::Running
            {
                last.status = OperationStatus::Succeeded;
                last.finished_at = Some(now);
            }
            op.step = Some(name.to_string());
            op.steps.push(OperationStep {
                name: name.to_string(),
                status: OperationStatus::Running,
                started_at: Some(now),
                finished_at: None,
            });
        }) {
            self.tracker.publish_progress(&op, name, false);
        }
    }

    /// Finish the operation and publish the final progress event.
    pub fn finish(mut self, outcome: OpOutcome) -> Operation {
        self.finished = true;
        self.finish_inner(outcome)
            .expect("operation exists until finished")
    }

    fn finish_inner(&self, outcome: OpOutcome) -> Option<Operation> {
        let now = Timestamp::now();
        let op = self.tracker.update(&self.id, |op| {
            let (status, step_status) = match &outcome {
                OpOutcome::Succeeded(_) | OpOutcome::Noop => {
                    (OperationStatus::Succeeded, OperationStatus::Succeeded)
                }
                OpOutcome::Failed(_) => (OperationStatus::Failed, OperationStatus::Failed),
                OpOutcome::Cancelled => (OperationStatus::Cancelled, OperationStatus::Cancelled),
            };
            if let Some(last) = op.steps.last_mut()
                && last.status == OperationStatus::Running
            {
                last.status = step_status;
                last.finished_at = Some(now);
            }
            op.status = status;
            op.finished_at = Some(now);
            match outcome {
                OpOutcome::Succeeded(v) => op.result = v,
                OpOutcome::Noop => op.noop = true,
                OpOutcome::Failed(e) => op.error = Some(e),
                OpOutcome::Cancelled => {
                    op.error = Some(ApiError {
                        code: "cancelled".into(),
                        message: "operation was cancelled".into(),
                        details: Value::Null,
                    })
                }
            }
        })?;
        let step = op.step.clone().unwrap_or_else(|| "done".into());
        self.tracker.publish_progress(&op, &step, true);
        Some(op)
    }
}

impl Drop for OpHandle {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.finish_inner(OpOutcome::Failed(ApiError {
                code: "internal".into(),
                message: "operation worker ended without a result".into(),
                details: Value::Null,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lantern_core::ids::ProjectId;

    fn inst() -> InstanceId {
        InstanceId::primary(&ProjectId::parse("myapp").unwrap())
    }

    #[tokio::test]
    async fn begin_step_finish_and_wait() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let t = OperationTracker::new(bus);
        let h = t.begin(OperationKind::Start, Some(inst()));
        let id = h.id().clone();
        assert_eq!(t.active_for(&inst()).len(), 1);
        h.step("spawn");
        let waiter = {
            let t = t.clone();
            let id = id.clone();
            tokio::spawn(async move { t.wait(&id, Duration::from_secs(5)).await })
        };
        let op = h.finish(OpOutcome::Succeeded(None));
        assert_eq!(op.status, OperationStatus::Succeeded);
        assert_eq!(op.steps[0].status, OperationStatus::Succeeded);
        let waited = waiter.await.unwrap().unwrap();
        assert_eq!(waited.status, OperationStatus::Succeeded);
        assert!(t.active_for(&inst()).is_empty());
        assert!(matches!(
            rx.recv().await.unwrap(),
            Event::OperationProgress { done: false, .. }
        ));
        assert!(matches!(
            rx.recv().await.unwrap(),
            Event::OperationProgress { done: true, .. }
        ));
    }

    #[tokio::test]
    async fn wait_times_out_while_running() {
        let t = OperationTracker::new(EventBus::new());
        let h = t.begin(OperationKind::Prepare, Some(inst()));
        let op = t.wait(h.id(), Duration::from_millis(20)).await.unwrap();
        assert_eq!(op.status, OperationStatus::Running);
        h.finish(OpOutcome::Noop);
    }

    #[tokio::test]
    async fn finished_operations_expire_after_retention() {
        let t = OperationTracker::with_retention(EventBus::new(), Duration::from_millis(30));
        let h = t.begin(OperationKind::Start, Some(inst()));
        let id = h.id().clone();
        h.finish(OpOutcome::Succeeded(None));
        assert!(t.get(&id).is_some(), "kept right after finishing");
        let running = t.begin(OperationKind::Prepare, Some(inst()));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(t.get(&id).is_none(), "expired after retention");
        assert!(t.get(running.id()).is_some(), "running operations never expire");
        running.finish(OpOutcome::Noop);
    }

    #[tokio::test]
    async fn cancel_and_drop_semantics() {
        let t = OperationTracker::new(EventBus::new());
        let h = t.begin(OperationKind::Start, Some(inst()));
        let token = h.cancel_token();
        assert!(t.cancel(h.id()));
        assert!(token.is_cancelled());
        let op = h.finish(OpOutcome::Cancelled);
        assert_eq!(op.status, OperationStatus::Cancelled);
        assert!(!t.cancel(&op.id));
        let dropped = t.begin(OperationKind::Stop, None);
        let id = dropped.id().clone();
        drop(dropped);
        let op = t.get(&id).unwrap();
        assert_eq!(op.status, OperationStatus::Failed);
        assert_eq!(op.error.unwrap().code, "internal");
    }
}
