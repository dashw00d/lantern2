//! Level-triggered route synchronization with a bounded debounce.

use lantern_core::{
    ids::InstanceId,
    model::{Instance, InstanceStatus},
};
use std::{
    collections::BTreeMap,
    future::Future,
    net::SocketAddr,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;

use super::{
    caddy::Caddy,
    routes::{PlanInput, RouteOwner, RoutePlan, Target, add_mailpit, build_plan},
};
use crate::runtime::seams::{RouteFuture, RouteTarget, RuntimeRouter};

/// Asynchronous full-plan application result.
pub type ApplyFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
/// Consumer boundary for applying a complete desired plan.
pub trait RouteApplier: Send + Sync {
    /// Apply a complete plan and report an error without discarding desired state.
    fn apply<'a>(&'a self, plan: &'a RoutePlan) -> ApplyFuture<'a>;
}
impl RouteApplier for Caddy {
    fn apply<'a>(&'a self, plan: &'a RoutePlan) -> ApplyFuture<'a> {
        Box::pin(async move {
            Caddy::apply(self, plan)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }
}
#[derive(Debug, Default)]
struct Desired {
    instances: BTreeMap<InstanceId, Instance>,
    targets: BTreeMap<InstanceId, RouteTarget>,
    mailpit: Option<Option<SocketAddr>>,
    generation: u64,
}
struct Inner {
    desired: Mutex<Desired>,
    tld: String,
    auto_adopt: bool,
    php_socket: PathBuf,
    notify: Notify,
    result: watch::Sender<(u64, Result<(), String>)>,
    cancel: CancellationToken,
}
impl Inner {
    fn plan(&self, desired: &Desired) -> RoutePlan {
        let instances: Vec<_> = desired.instances.values().cloned().collect();
        let mut plan = build_plan(&PlanInput {
            instances: &instances,
            tld: &self.tld,
            auto_adopt: self.auto_adopt,
        });
        if let Some(upstream) = desired.mailpit {
            add_mailpit(&mut plan, &self.tld, upstream);
        }
        for route in &mut plan.routes {
            if let RouteOwner::Instance(id) = &route.owner
                && let Some(instance) = desired.instances.get(id)
                && matches!(
                    instance.runtime.status,
                    InstanceStatus::Running | InstanceStatus::Unhealthy
                )
                && let Some(target) = desired.targets.get(id)
            {
                route.target = Target::Live {
                    target: target.clone(),
                    tls: false,
                };
            }
        }
        plan
    }
    fn dirty(&self, state: &mut Desired) -> u64 {
        state.generation = state.generation.saturating_add(1);
        self.notify.notify_one();
        state.generation
    }
    async fn wait(&self, generation: u64) -> Result<(), String> {
        let mut rx = self.result.subscribe();
        loop {
            let (done, result) = rx.borrow().clone();
            if done >= generation {
                return result;
            }
            tokio::select! {
                _ = self.cancel.cancelled() => return Err("network router stopped".into()),
                changed = rx.changed() => if changed.is_err() { return Err("network router stopped".into()); },
            }
        }
    }
}

/// Runtime routing adapter with a retained desired plan and background retry.
#[derive(Clone)]
pub struct NetRouter {
    inner: Arc<Inner>,
}
impl std::fmt::Debug for NetRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetRouter")
            .field("tld", &self.inner.tld)
            .finish_non_exhaustive()
    }
}
impl NetRouter {
    /// Start the synchronization worker; the caller owns the cancellation token.
    pub fn new(
        applier: Arc<dyn RouteApplier>,
        tld: String,
        auto_adopt: bool,
        php_socket: PathBuf,
        cancel: CancellationToken,
    ) -> Self {
        let (result, _) = watch::channel((0, Ok(())));
        let inner = Arc::new(Inner {
            desired: Mutex::new(Desired::default()),
            tld,
            auto_adopt,
            php_socket,
            notify: Notify::new(),
            result,
            cancel,
        });
        tokio::spawn(run(inner.clone(), applier));
        Self { inner }
    }
    /// Replace the whole registry snapshot; call on boot and every relevant registry event.
    pub fn replace_instances(&self, instances: Vec<Instance>) {
        let mut state = self.inner.desired.lock().unwrap();
        let next: BTreeMap<_, _> = instances.into_iter().map(|i| (i.id.clone(), i)).collect();
        let stale: Vec<_> = state
            .targets
            .keys()
            .filter(|id| match (state.instances.get(*id), next.get(*id)) {
                (Some(old), Some(new)) => {
                    old.manifest != new.manifest || old.checkout.path != new.checkout.path
                }
                _ => true,
            })
            .cloned()
            .collect();
        for id in stale {
            state.targets.remove(&id);
        }
        state.instances = next;
        self.inner.dirty(&mut state);
    }
    /// Set whether Mailpit is published and its live UI upstream, if running.
    pub fn set_mailpit(&self, enabled: bool, upstream: Option<SocketAddr>) {
        let mut state = self.inner.desired.lock().unwrap();
        state.mailpit = enabled.then_some(upstream);
        self.inner.dirty(&mut state);
    }
    /// Snapshot the current desired plan for gateway integration.
    pub fn current_plan(&self) -> RoutePlan {
        self.inner.plan(&self.inner.desired.lock().unwrap())
    }
    /// Schedule another full-plan application, including after proxy recovery.
    pub fn reconcile(&self) {
        let mut state = self.inner.desired.lock().unwrap();
        self.inner.dirty(&mut state);
    }
    /// Wait until the currently desired generation has been attempted by the worker.
    pub async fn flush(&self) -> Result<(), String> {
        let generation = self.inner.desired.lock().unwrap().generation;
        if generation == 0 {
            return Ok(());
        }
        self.inner.wait(generation).await
    }
}
impl RuntimeRouter for NetRouter {
    fn publish<'a>(&'a self, instance: &'a Instance, target: RouteTarget) -> RouteFuture<'a> {
        Box::pin(async move {
            let generation = {
                let mut state = self.inner.desired.lock().unwrap();
                if let Some(current) = state.instances.get(&instance.id)
                    && (current.manifest != instance.manifest
                        || current.checkout.path != instance.checkout.path)
                {
                    return Err("instance manifest changed while publishing its route; retry".into());
                }
                state
                    .instances
                    .entry(instance.id.clone())
                    .or_insert_with(|| instance.clone());
                state.targets.insert(instance.id.clone(), target);
                self.inner.dirty(&mut state)
            };
            self.inner.wait(generation).await
        })
    }
    fn unpublish<'a>(&'a self, id: &'a InstanceId) -> RouteFuture<'a> {
        Box::pin(async move {
            let generation = {
                let mut state = self.inner.desired.lock().unwrap();
                if let Some(instance) = state.instances.get_mut(id) {
                    instance.runtime.status = InstanceStatus::Stopped;
                }
                state.targets.remove(id);
                self.inner.dirty(&mut state)
            };
            self.inner.wait(generation).await
        })
    }
    fn php_ready<'a>(&'a self) -> RouteFuture<'a> {
        Box::pin(async move {
            if !self.inner.php_socket.is_absolute() {
                return Err("php-fpm socket has not been resolved to an absolute path".into());
            }
            tokio::time::timeout(
                Duration::from_secs(2),
                tokio::net::UnixStream::connect(&self.inner.php_socket),
            )
            .await
            .map_err(|_| "php-fpm connection timed out".to_string())?
            .map(|_| ())
            .map_err(|e| format!("php-fpm socket {}: {e}", self.inner.php_socket.display()))
        })
    }
}
async fn run(inner: Arc<Inner>, applier: Arc<dyn RouteApplier>) {
    loop {
        tokio::select! { _=inner.cancel.cancelled()=>return, _=inner.notify.notified()=>{} }
        let first = tokio::time::Instant::now();
        let cap = first + Duration::from_secs(1);
        let mut deadline = first + Duration::from_millis(150);
        loop {
            tokio::select! {
                _=inner.cancel.cancelled()=>return,
                _=tokio::time::sleep_until(deadline)=>break,
                _=inner.notify.notified()=> deadline=(tokio::time::Instant::now()+Duration::from_millis(150)).min(cap),
            }
        }
        let (generation, plan) = {
            let state = inner.desired.lock().unwrap();
            (state.generation, inner.plan(&state))
        };
        let result =
            tokio::select! { _=inner.cancel.cancelled()=>return, result=applier.apply(&plan)=>result };
        let retry = result.is_err();
        let _previous = inner.result.send_replace((generation, result));
        if inner.desired.lock().unwrap().generation != generation {
            inner.notify.notify_one();
        }
        if retry {
            tokio::select! { _=inner.cancel.cancelled()=>return, _=tokio::time::sleep(Duration::from_secs(1))=>inner.notify.notify_one() }
        }
    }
}
