//! Serialized discovery scans, snapshot publication, and external-listener settling.

use crate::{
    discovery::{
        Snapshot,
        sched::{MonotonicClock, ScanPolicy},
    },
    registry::{ExternalChange, RegistryHandle},
    runtime::{Runtime, seams::ScannerProbe, systemd::UnitNames},
};
use lantern_core::{
    Timestamp,
    ids::InstanceId,
    model::{ExternalState, InstanceStatus, Listener, ListenerClass, Scheme},
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const APPEAR: Duration = Duration::from_millis(1500);
const DISAPPEAR: Duration = Duration::from_secs(5);

/// Handle for on-demand scans; every request joins the single scanner task.
#[derive(Debug, Clone)]
pub struct Discovery {
    requests: mpsc::Sender<oneshot::Sender<Result<Snapshot, String>>>,
    cache: Arc<RwLock<Option<Snapshot>>>,
}
impl Discovery {
    /// Launch the scanner and return its on-demand handle.
    pub fn spawn(
        scanner: Arc<ScannerProbe>,
        registry: RegistryHandle,
        runtime: Arc<Runtime>,
        names: UnitNames,
        shutdown: CancellationToken,
    ) -> Self {
        let (requests, rx) = mpsc::channel(32);
        let cache = Arc::new(RwLock::new(None));
        tokio::spawn(run(
            scanner,
            registry,
            runtime,
            names,
            shutdown,
            rx,
            cache.clone(),
        ));
        Self { requests, cache }
    }
    /// Return the latest cached snapshot without initiating a scan.
    pub fn cached(&self) -> Option<Snapshot> {
        self.cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    /// Obtain a fresh scan through the single-flight worker.
    pub async fn scan(&self) -> Result<Snapshot, String> {
        let (tx, rx) = oneshot::channel();
        self.requests
            .send(tx)
            .await
            .map_err(|_| "discovery stopped".to_owned())?;
        rx.await.map_err(|_| "discovery stopped".to_owned())?
    }
}

#[derive(Debug)]
struct Candidate {
    listener: Listener,
    first: Instant,
    last: Instant,
    published: bool,
    missing: bool,
    since: Timestamp,
}

async fn run(
    scanner: Arc<ScannerProbe>,
    registry: RegistryHandle,
    runtime: Arc<Runtime>,
    names: UnitNames,
    shutdown: CancellationToken,
    mut requests: mpsc::Receiver<oneshot::Sender<Result<Snapshot, String>>>,
    cache: Arc<RwLock<Option<Snapshot>>>,
) {
    let clock = MonotonicClock::default();
    let mut policy = ScanPolicy::new(clock);
    let epoch = Instant::now();
    let mut index = registry.subscribe_checkout_index();
    let mut candidates: HashMap<InstanceId, Candidate> = HashMap::new();
    let mut pending: Vec<oneshot::Sender<Result<Snapshot, String>>> = Vec::new();
    let mut latest: Option<Snapshot> = None;
    loop {
        let current_index = index.borrow().clone();
        policy.set_watching(
            current_index.any_adopted || candidates.values().any(|c| c.published),
            false,
        );
        let live = registry.instances().iter().any(|i| {
            matches!(
                i.runtime.status,
                InstanceStatus::Starting
                    | InstanceStatus::Running
                    | InstanceStatus::Unhealthy
                    | InstanceStatus::Preparing
            )
        });
        policy.set_interests(usize::from(live));
        let now = epoch.elapsed();
        if policy.next_scan().is_some_and(|deadline| deadline <= now) {
            let scanner = scanner.clone();
            let prefix = names.instance_prefix.clone();
            let mut next_index = current_index;
            for checkout in next_index.by_path.values() {
                for suffix in ["service", "scope"] {
                    next_index.by_unit.insert(
                        format!("{}{}.{}", prefix, checkout.instance.safe_name(), suffix),
                        checkout.instance.clone(),
                    );
                }
            }
            let result = tokio::task::spawn_blocking(move || {
                let mut scanner = scanner.0.lock().map_err(|_| "scanner lock poisoned".to_owned())?;
                scanner.set_index(next_index);
                scanner.scan().map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
            policy.scanned();
            match result {
                Ok(mut snapshot) => {
                    for row in snapshot.listeners.iter_mut().chain(snapshot.hidden.iter_mut()) {
                        if row.unit.as_deref().is_some_and(|unit| {
                            unit.starts_with(&names.instance_prefix)
                                || unit.starts_with(&names.operation_prefix)
                        }) {
                            row.managed = true;
                        }
                    }
                    settle(&snapshot.listeners, &registry, &runtime, &mut candidates);
                    if latest
                        .as_ref()
                        .is_none_or(|old| old.listeners != snapshot.listeners)
                    {
                        policy.changed();
                    }
                    latest = Some(snapshot.clone());
                    *cache.write().unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(snapshot.clone());
                    for tx in pending.drain(..) {
                        let _ = tx.send(Ok(snapshot.clone()));
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "discovery scan failed");
                    for tx in pending.drain(..) {
                        let _ = tx.send(Err(error.clone()));
                    }
                }
            }
        }
        if policy.take_publication()
            && let Some(snapshot) = &latest
        {
            registry.set_listeners(snapshot.seq, snapshot.listeners.clone());
        }
        let followup = candidates
            .values()
            .filter_map(|c| {
                if !c.published {
                    Some(c.first + APPEAR)
                } else if c.missing {
                    Some(c.last + DISAPPEAR)
                } else {
                    None
                }
            })
            .min();
        tokio::select! {
            () = shutdown.cancelled() => return,
            value = requests.recv() => match value { Some(tx) => { pending.push(tx); policy.request_scan(); }, None => return },
            result = index.changed() => if result.is_ok() { policy.request_scan(); },
            () = tokio::time::sleep(Duration::from_millis(250)) => {
                if followup.is_some_and(|deadline| Instant::now() >= deadline) { policy.request_scan(); }
            }
        }
    }
}

fn settle(
    rows: &[Listener],
    registry: &RegistryHandle,
    runtime: &Runtime,
    candidates: &mut HashMap<InstanceId, Candidate>,
) {
    let now = Instant::now();
    let mut grouped: BTreeMap<InstanceId, Vec<&Listener>> = BTreeMap::new();
    for row in rows.iter().filter(|row| !row.managed) {
        if let Some(id) = &row.matched {
            grouped.entry(id.clone()).or_default().push(row);
        }
    }
    for (id, mut matches) in grouped {
        matches.sort_by_key(|l| {
            (
                !l.probe.as_ref().is_some_and(|p| p.http),
                l.class != ListenerClass::DevServer,
                l.port,
            )
        });
        let best = matches[0];
        let candidate = candidates.entry(id.clone()).or_insert_with(|| Candidate {
            listener: best.clone(),
            first: now,
            last: now,
            published: false,
            missing: false,
            since: Timestamp::now(),
        });
        if candidate.listener.port != best.port || candidate.listener.pid != best.pid {
            if !candidate.published {
                candidate.first = now;
            }
            candidate.listener = best.clone();
        }
        candidate.last = now;
        candidate.missing = false;
        if candidate.published || now.duration_since(candidate.first) >= APPEAR {
            let external = ExternalState {
                port: best.port,
                upstream: best.addr,
                scheme: if best.probe.as_ref().is_some_and(|p| p.tls) {
                    Scheme::Https
                } else {
                    Scheme::Http
                },
                pid: best.pid,
                label: best.label.clone(),
                extra_ports: matches.iter().skip(1).map(|l| l.port).collect(),
                since: candidate.since,
            };
            if !candidate.published {
                runtime.note_external(&id, Some(best));
            }
            let change = if candidate.published {
                ExternalChange::Updated {
                    instance: id,
                    external,
                }
            } else {
                ExternalChange::Appeared {
                    instance: id,
                    external,
                }
            };
            let _ = registry.apply_external(change);
            candidate.published = true;
        }
    }
    candidates.retain(|id, candidate| {
        if candidate.last == now {
            return true;
        }
        if !candidate.published {
            return false;
        }
        candidate.missing = true;
        if now.duration_since(candidate.last) < DISAPPEAR {
            return true;
        }
        runtime.note_external(id, None);
        let _ = registry.apply_external(ExternalChange::Gone { instance: id.clone() });
        false
    });
}
