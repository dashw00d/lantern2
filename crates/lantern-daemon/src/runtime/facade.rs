//! Runtime facade and per-instance lifecycle coordination.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use lantern_core::Timestamp;
use lantern_core::api::{
    ApiError, Operation as OperationSnapshot, OperationId, OperationKind, OperationStatus,
};
use lantern_core::ids::{InstanceId, branch_slug};
use lantern_core::model::{
    ExitInfo, ExitKind, ExternalState, HealthSample, HealthState, Instance, InstanceStatus, IssueCode,
    Listener, LogLine, RuntimeIssue, RuntimeState, ServeMode,
};
use lantern_core::settings::Settings;
use lantern_core::template::VarContext;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;

use super::journal::{self, LogHub};
use super::launcher::{self, LaunchSpec};
use super::ports::{PortAvailability, PortLeases, PortRange};
use super::seams::{
    InstancePreparer, NoopPreparer, PortProbe, RouteTarget, RuntimeRouter, RuntimeStore, ServiceControl,
    UnavailableRouter, UnavailableServices,
};
use super::shell_env;
use super::state_machine::{
    self as sm, Event, Operation, RequestPolicy, StartKind, StartValidation, SupervisorState,
};
use super::systemd::{
    self, Active, ExitCodeKind, Load, OneShotSpec, UnitNames, UnitSnapshot, UnitStart, UserSystemd,
};
use crate::discovery::PortBinding;
use crate::operations::{OpHandle, OpOutcome, OperationTracker};

/// Directories, backend names, and scheduling intervals supplied by the daemon.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// User-supplied settings.
    pub settings: Settings,
    /// Private directory for launcher scripts.
    pub run_dir: PathBuf,
    /// Private shell environment cache file.
    pub shell_env_file: PathBuf,
    /// Home directory used for toolchain and shell discovery.
    pub home: PathBuf,
    /// Unit prefixes and slice (test instances must override these).
    pub names: UnitNames,
    /// Batched systemd polling period.
    pub poll_interval: Duration,
    /// Startup listener probing period.
    pub readiness_interval: Duration,
    /// Time an incorrect port must be observed before diagnosing a mismatch.
    pub mismatch_after: Duration,
}

/// Optional cross-subsystem adapters required for complete site and service handling.
pub struct RuntimeAdapters {
    /// Checkout preparation implementation.
    pub preparer: Arc<dyn InstancePreparer>,
    /// Route and php-fpm implementation.
    pub router: Arc<dyn RuntimeRouter>,
    /// Shared-service implementation.
    pub services: Arc<dyn ServiceControl>,
}
impl std::fmt::Debug for RuntimeAdapters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeAdapters").finish_non_exhaustive()
    }
}

/// Error returned before an operation is accepted or when runtime I/O fails.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// No registered instance exists.
    #[error("instance not found: {0}")]
    NotFound(InstanceId),
    /// A different operation is already running.
    #[error("operation in progress: {0}")]
    Busy(OperationId),
    /// The requested action cannot control an externally started process.
    #[error("instance is externally running")]
    External,
    /// The instance has been disabled.
    #[error("instance is disabled")]
    Disabled,
    /// The registry or another runtime subsystem failed.
    #[error("{0}")]
    Other(String),
}

/// A lightweight reference to an operation whose worker owns the `OpHandle`.
#[derive(Debug, Clone)]
pub struct RuntimeOperation {
    /// Identifier used by the shared tracker and API.
    pub id: OperationId,
    tracker: OperationTracker,
}
impl RuntimeOperation {
    /// Wait for completion, or return the running snapshot at the deadline.
    pub async fn wait(&self, timeout: Duration) -> Option<OperationSnapshot> {
        self.tracker.wait(&self.id, timeout).await
    }
    /// Request cancellation through the shared operation tracker.
    pub fn cancel(&self) -> bool {
        self.tracker.cancel(&self.id)
    }
}

/// Start request options.
#[derive(Debug, Clone, Copy, Default)]
pub struct StartOpts;
/// Stop request options.
#[derive(Debug, Clone, Copy, Default)]
pub struct StopOpts {
    /// Stop dependent instances before this one.
    pub cascade: bool,
}

/// Result of an on-demand deploy status command.
#[derive(Debug, Clone)]
pub struct DeployStatus {
    /// Whether the command exited with code zero.
    pub alive: bool,
    /// Exit code reported by systemd, if available.
    pub exit_code: Option<i32>,
    /// Bounded output read from the operation's journal.
    pub output: String,
}

/// Launcher inputs for a setup or deploy one-shot.
#[derive(Debug, Clone)]
pub struct OneShotLaunch {
    /// `setup` or `deploy`, which determines journal stream attribution.
    pub verb: String,
    /// Checkout-confined working directory.
    pub cwd: PathBuf,
    /// Command already rewritten with `TemplateString::render_shell()`.
    pub shell_command: String,
    /// Literal environment values supplied by the caller.
    pub env: BTreeMap<String, String>,
    /// Maximum one-shot duration.
    pub timeout: Duration,
}

/// Current shell environment capture metadata, without secret values.
#[derive(Debug, Clone, Default)]
pub struct EnvReport {
    /// When the most recent successful or fallback capture was saved.
    pub captured_at: Option<Timestamp>,
    /// Effective PATH after merge.
    pub path: String,
    /// Warning from the most recent capture, if fallback was needed.
    pub warning: Option<String>,
}

/// Result of boot reconciliation.
#[derive(Debug, Default)]
pub struct ReconcileReport {
    /// Adopted known units.
    pub adopted: Vec<InstanceId>,
    /// Units without registry entries, left untouched.
    pub orphans: Vec<String>,
    /// Reconciliation errors that require operator or integration attention.
    pub errors: Vec<String>,
}

/// Journal history selection.
#[derive(Debug, Clone, Copy)]
pub struct HistoryQuery {
    /// Maximum number of lines.
    pub limit: usize,
}

#[derive(Debug, Clone)]
struct Fault {
    code: &'static str,
    message: String,
    issue: IssueCode,
}
impl Fault {
    fn new(code: &'static str, issue: IssueCode, message: impl Into<String>) -> Self {
        Self {
            code,
            issue,
            message: message.into(),
        }
    }
    fn api(&self) -> ApiError {
        ApiError {
            code: self.code.into(),
            message: self.message.clone(),
            details: serde_json::Value::Null,
        }
    }
}

#[derive(Debug)]
struct ActiveOperation {
    operation: Operation,
    id: OperationId,
    token: CancellationToken,
}
#[derive(Debug)]
struct ActorState {
    supervisor: SupervisorState,
    active: Option<ActiveOperation>,
    generation: u64,
    retry_streak: u32,
    ready_at: Option<Instant>,
    deploy_started: bool,
    worktree_env: BTreeMap<String, String>,
}
#[derive(Debug)]
struct Actor {
    state: Mutex<ActorState>,
    io_lock: tokio::sync::Mutex<()>,
    idle: tokio::sync::Notify,
    snapshot: watch::Sender<Option<UnitSnapshot>>,
}
impl Actor {
    fn new(state: &RuntimeState) -> Self {
        let (snapshot, _) = watch::channel(None);
        Self {
            state: Mutex::new(ActorState {
                supervisor: SupervisorState {
                    status: state.status,
                    restarts: state.restarts,
                    ..SupervisorState::default()
                },
                active: None,
                generation: 0,
                retry_streak: 0,
                ready_at: None,
                deploy_started: false,
                worktree_env: BTreeMap::new(),
            }),
            io_lock: tokio::sync::Mutex::new(()),
            idle: tokio::sync::Notify::new(),
            snapshot,
        }
    }
}
struct Inner {
    config: RuntimeConfig,
    store: Arc<dyn RuntimeStore>,
    probe: Arc<dyn PortProbe>,
    preparer: Arc<dyn InstancePreparer>,
    router: Arc<dyn RuntimeRouter>,
    services: Arc<dyn ServiceControl>,
    backend: Arc<dyn UserSystemd>,
    tracker: OperationTracker,
    logs: Arc<LogHub>,
    actors: Mutex<HashMap<InstanceId, Arc<Actor>>>,
    lease_lock: tokio::sync::Mutex<()>,
    env: Mutex<EnvReport>,
    env_lock: tokio::sync::Mutex<()>,
    env_sources: Mutex<Option<shell_env::SourceMtimes>>,
    shutdown: CancellationToken,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Facade for lifecycle operations, polling, logs and reconciliation.
#[derive(Clone)]
pub struct Runtime {
    inner: Arc<Inner>,
}
impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime").finish_non_exhaustive()
    }
}

impl Runtime {
    /// Construct the runtime with a no-op preparer and start its poller and journal follower.
    pub fn new(
        config: RuntimeConfig,
        store: Arc<dyn RuntimeStore>,
        probe: Arc<dyn PortProbe>,
        backend: Arc<dyn UserSystemd>,
        tracker: OperationTracker,
    ) -> Self {
        Self::with_preparer(config, store, probe, backend, tracker, Arc::new(NoopPreparer))
    }
    /// Construct the runtime with a checkout preparation hook.
    pub fn with_preparer(
        config: RuntimeConfig,
        store: Arc<dyn RuntimeStore>,
        probe: Arc<dyn PortProbe>,
        backend: Arc<dyn UserSystemd>,
        tracker: OperationTracker,
        preparer: Arc<dyn InstancePreparer>,
    ) -> Self {
        Self::with_adapters(
            config,
            store,
            probe,
            backend,
            tracker,
            preparer,
            Arc::new(UnavailableRouter),
        )
    }
    /// Construct the runtime with checkout preparation and routing adapters.
    pub fn with_adapters(
        config: RuntimeConfig,
        store: Arc<dyn RuntimeStore>,
        probe: Arc<dyn PortProbe>,
        backend: Arc<dyn UserSystemd>,
        tracker: OperationTracker,
        preparer: Arc<dyn InstancePreparer>,
        router: Arc<dyn RuntimeRouter>,
    ) -> Self {
        Self::with_all_adapters(
            config,
            store,
            probe,
            backend,
            tracker,
            RuntimeAdapters {
                preparer,
                router,
                services: Arc::new(UnavailableServices),
            },
        )
    }
    /// Construct with checkout, networking and shared-service adapters.
    pub fn with_all_adapters(
        config: RuntimeConfig,
        store: Arc<dyn RuntimeStore>,
        probe: Arc<dyn PortProbe>,
        backend: Arc<dyn UserSystemd>,
        tracker: OperationTracker,
        adapters: RuntimeAdapters,
    ) -> Self {
        let RuntimeAdapters {
            preparer,
            router,
            services,
        } = adapters;
        let logs = Arc::new(LogHub::new(
            config.settings.logs.ring_lines,
            config.settings.logs.ring_bytes,
            1024,
        ));
        let initial_sources = config
            .shell_env_file
            .is_file()
            .then(|| shell_env::source_mtimes(&config.home));
        let inner = Arc::new(Inner {
            config,
            store,
            probe,
            preparer,
            router,
            services,
            backend,
            tracker,
            logs,
            actors: Mutex::new(HashMap::new()),
            lease_lock: tokio::sync::Mutex::new(()),
            env: Mutex::new(EnvReport::default()),
            env_lock: tokio::sync::Mutex::new(()),
            env_sources: Mutex::new(initial_sources),
            shutdown: CancellationToken::new(),
        });
        tokio::spawn(poll_loop(Arc::downgrade(&inner)));
        tokio::spawn(journal_loop(
            inner.config.names.clone(),
            inner.logs.clone(),
            inner.shutdown.clone(),
        ));
        Self { inner }
    }
    fn actor(&self, instance: &Instance) -> Arc<Actor> {
        self.inner
            .actors
            .lock()
            .unwrap()
            .entry(instance.id.clone())
            .or_insert_with(|| Arc::new(Actor::new(&instance.runtime)))
            .clone()
    }
    /// Return the current registry-owned runtime state.
    pub fn state(&self, id: &InstanceId) -> Option<RuntimeState> {
        self.inner.store.instance(id).ok().flatten().map(|i| i.runtime)
    }
    /// Atomically subscribe to backlog and future lines.
    pub fn subscribe_logs(
        &self,
        id: &InstanceId,
        backlog: usize,
    ) -> (Vec<LogLine>, broadcast::Receiver<LogLine>) {
        self.inner.logs.subscribe_logs(id, backlog)
    }
    /// Reserve a sticky port immediately after creating an instance record, before setup.
    pub async fn reserve_port(&self, id: &InstanceId) -> Result<u16, RuntimeError> {
        if self
            .inner
            .store
            .instance(id)
            .map_err(|e| RuntimeError::Other(e.to_string()))?
            .is_none()
        {
            return Err(RuntimeError::NotFound(id.clone()));
        }
        let port = self
            .allocate(id)
            .await
            .map_err(|e| RuntimeError::Other(e.message))?;
        self.inner
            .store
            .update_runtime(id, Box::new(move |r| r.port = Some(port)))
            .map_err(|e| RuntimeError::Other(e.to_string()))?;
        Ok(port)
    }
    /// Start an instance and its dependencies.
    pub async fn start(&self, id: &InstanceId, _: StartOpts) -> Result<RuntimeOperation, RuntimeError> {
        self.request(id, Operation::Start, true)
    }
    /// Stop an instance, optionally stopping its dependents first.
    pub async fn stop(&self, id: &InstanceId, opts: StopOpts) -> Result<RuntimeOperation, RuntimeError> {
        if opts.cascade {
            return self.stop_cascade(id);
        }
        self.request(id, Operation::Stop, true)
    }
    /// Restart an instance without stopping its dependents.
    pub async fn restart(&self, id: &InstanceId) -> Result<RuntimeOperation, RuntimeError> {
        self.request(id, Operation::Restart, true)
    }
    fn request(
        &self,
        id: &InstanceId,
        operation: Operation,
        manual: bool,
    ) -> Result<RuntimeOperation, RuntimeError> {
        let instance = self
            .inner
            .store
            .instance(id)
            .map_err(|e| RuntimeError::Other(e.to_string()))?
            .ok_or_else(|| RuntimeError::NotFound(id.clone()))?;
        if operation != Operation::Stop && !instance.manifest.enabled {
            return Err(RuntimeError::Disabled);
        }
        if instance.runtime.status == InstanceStatus::External {
            return Err(RuntimeError::External);
        }
        let actor = self.actor(&instance);
        let mut guard = actor.state.lock().unwrap();
        match sm::request_policy(guard.active.as_ref().map(|a| a.operation), operation) {
            RequestPolicy::Join => {
                return Ok(RuntimeOperation {
                    id: guard.active.as_ref().unwrap().id.clone(),
                    tracker: self.inner.tracker.clone(),
                });
            }
            RequestPolicy::Busy => return Err(RuntimeError::Busy(guard.active.as_ref().unwrap().id.clone())),
            RequestPolicy::Preempt => {
                if let Some(a) = guard.active.take() {
                    a.token.cancel();
                }
            }
            RequestPolicy::Run => {}
        }
        let kind = match operation {
            Operation::Start => OperationKind::Start,
            Operation::Stop => OperationKind::Stop,
            Operation::Restart => OperationKind::Restart,
            Operation::Prepare => OperationKind::Prepare,
        };
        let handle = self.inner.tracker.begin(kind, Some(id.clone()));
        let result = RuntimeOperation {
            id: handle.id().clone(),
            tracker: self.inner.tracker.clone(),
        };
        guard.generation = guard.generation.wrapping_add(1);
        let generation = guard.generation;
        if manual && operation != Operation::Stop {
            guard.retry_streak = 0;
        }
        guard.active = Some(ActiveOperation {
            operation,
            id: handle.id().clone(),
            token: handle.cancel_token(),
        });
        let event = if !manual && guard.supervisor.retry_pending {
            Event::RetryTimer
        } else {
            Event::Request {
                operation,
                validation: StartValidation::Ready,
            }
        };
        let (next, _) = sm::transition(&guard.supervisor, event);
        guard.supervisor = next;
        self.publish_supervisor(id, &guard.supervisor);
        drop(guard);
        let runtime = self.clone();
        let id = id.clone();
        tokio::spawn(Box::pin(async move {
            runtime
                .operation_worker(id, actor, generation, operation, handle)
                .await;
        }));
        Ok(result)
    }
    fn publish_supervisor(&self, id: &InstanceId, state: &SupervisorState) {
        let state = state.clone();
        if let Err(e) = self.inner.store.update_runtime(
            id,
            Box::new(move |r| {
                r.status = state.status;
                r.restarts = state.restarts;
                r.message = state.message.clone();
                r.issue = state.issue.map(|code| RuntimeIssue {
                    code,
                    detail: state.message.clone().unwrap_or_else(|| format!("{code:?}")),
                    hint: None,
                });
            }),
        ) {
            tracing::error!(%e, %id, "failed to persist supervisor state");
        }
    }
    fn event(&self, id: &InstanceId, actor: &Actor, generation: u64, event: Event) -> bool {
        let mut guard = actor.state.lock().unwrap();
        if guard.generation != generation {
            return false;
        }
        let (next, _) = sm::transition(&guard.supervisor, event);
        guard.supervisor = next;
        self.publish_supervisor(id, &guard.supervisor);
        true
    }
    fn update(
        &self,
        id: &InstanceId,
        actor: &Actor,
        generation: u64,
        f: impl FnOnce(&mut RuntimeState) + Send + 'static,
    ) -> bool {
        let guard = actor.state.lock().unwrap();
        if guard.generation != generation {
            return false;
        }
        if let Err(e) = self.inner.store.update_runtime(id, Box::new(f)) {
            tracing::error!(%e, %id, "runtime update failed");
        }
        true
    }
    async fn operation_worker(
        &self,
        id: InstanceId,
        actor: Arc<Actor>,
        generation: u64,
        operation: Operation,
        handle: OpHandle,
    ) {
        let token = handle.cancel_token();
        let result = match operation {
            Operation::Stop => self.perform_stop(&id, &actor, generation, &token).await,
            Operation::Start | Operation::Restart => {
                if operation == Operation::Restart
                    && matches!(self.state(&id).map(|r| r.status), Some(InstanceStatus::Stopping))
                {
                    let deploy_restart = self.inner.store.instance(&id).ok().flatten().is_some_and(|i| {
                        i.manifest.deploy.start.is_some() && i.manifest.deploy.restart.is_some()
                    });
                    if deploy_restart {
                        self.event(&id, &actor, generation, Event::UnitInactive);
                        self.perform_start(&id, &actor, generation, &token, &handle, true)
                            .await
                    } else {
                        match self.perform_stop(&id, &actor, generation, &token).await {
                            Ok(_) => {
                                self.perform_start(&id, &actor, generation, &token, &handle, false)
                                    .await
                            }
                            Err(e) => Err(e),
                        }
                    }
                } else {
                    self.perform_start(&id, &actor, generation, &token, &handle, false)
                        .await
                }
            }
            Operation::Prepare => Err(Fault::new(
                "unsupported",
                IssueCode::Other,
                "prepare is owned by the instances subsystem",
            )),
        };
        if token.is_cancelled() && operation != Operation::Stop {
            // An explicit cancellation without a superseding Stop must also clean up.
            if self.event(
                &id,
                &actor,
                generation,
                Event::Request {
                    operation: Operation::Stop,
                    validation: StartValidation::Ready,
                },
            ) && let Err(error) = self
                .perform_stop(&id, &actor, generation, &CancellationToken::new())
                .await
            {
                tracing::error!(%id, message=%error.message, "cancelled operation cleanup failed");
            }
        }
        let current = {
            let mut guard = actor.state.lock().unwrap();
            if guard.generation == generation {
                guard.active = None;
                true
            } else {
                false
            }
        };
        if current {
            actor.idle.notify_one();
        }
        if token.is_cancelled() || !current {
            handle.finish(OpOutcome::Cancelled);
        } else {
            match result {
                Ok(noop) => {
                    handle.finish(if noop {
                        OpOutcome::Noop
                    } else {
                        OpOutcome::Succeeded(None)
                    });
                }
                Err(err) => {
                    handle.finish(OpOutcome::Failed(err.api()));
                }
            }
        }
    }
    async fn perform_stop(
        &self,
        id: &InstanceId,
        actor: &Actor,
        generation: u64,
        token: &CancellationToken,
    ) -> Result<bool, Fault> {
        let state = self
            .state(id)
            .ok_or_else(|| Fault::new("not_found", IssueCode::Other, "instance disappeared"))?;
        let noop = state.status == InstanceStatus::Stopped
            && actor.state.lock().unwrap().supervisor.inflight.is_none();
        let _io_guard = actor.io_lock.lock().await;
        if token.is_cancelled() {
            return Err(Fault::new("cancelled", IssueCode::Other, "cancelled"));
        }
        if !noop && let Err(error) = self.inner.router.unpublish(id).await {
            tracing::warn!(%id, %error, "could not unpublish route before stop");
        }
        let instance = self
            .inner
            .store
            .instance(id)
            .map_err(store_fault)?
            .ok_or_else(|| Fault::new("not_found", IssueCode::Other, "instance disappeared"))?;
        let deploy = instance.manifest.deploy.start.is_some();
        let deploy_started = actor.state.lock().unwrap().deploy_started;
        if deploy && deploy_started {
            if let Some(command) = &instance.manifest.deploy.stop {
                let (_, unit) = self.launch(
                    &instance,
                    state.port.unwrap_or(0),
                    command.render_shell(),
                    true,
                    &actor.state.lock().unwrap().worktree_env,
                    false,
                )?;
                let spec = OneShotSpec {
                    unit,
                    timeout: Duration::from_secs(self.inner.config.settings.runtime.deploy_timeout_secs),
                    poll_interval: self.inner.config.poll_interval,
                };
                let result = systemd::run_oneshot(self.inner.backend.as_ref(), &spec, token.cancelled())
                    .await
                    .map_err(|e| Fault::new("deploy_stop_failed", IssueCode::Other, e.to_string()))?;
                if result.exit_kind != ExitCodeKind::Exited || result.exit_code != Some(0) {
                    return Err(Fault::new(
                        "deploy_stop_failed",
                        IssueCode::Other,
                        format!("deploy.stop exited {:?}", result.exit_code),
                    ));
                }
                if let Some(port) = state.port {
                    let deadline = Instant::now()
                        + Duration::from_secs(self.inner.config.settings.runtime.stop_timeout_secs);
                    while !matches!(self.probe(port, None).await?, PortBinding::Free) {
                        if Instant::now() >= deadline {
                            self.event(id, actor, generation, Event::StopIncomplete);
                            return Err(Fault::new(
                                "stop_incomplete",
                                IssueCode::Other,
                                format!("deploy listener still holds port {port}"),
                            ));
                        }
                        tokio::select! { ()=token.cancelled()=>return Err(Fault::new("cancelled",IssueCode::Other,"cancelled")), ()=tokio::time::sleep(self.inner.config.readiness_interval)=>{} }
                    }
                }
            } else {
                self.update(id, actor, generation, |r| {
                    r.message = Some("no deploy.stop configured; the process may still be running".into())
                });
            }
            actor.state.lock().unwrap().deploy_started = false;
        } else if !deploy && instance.manifest.run.cmd.is_some() && (!noop || state.unit.is_some()) {
            let unit = self.inner.config.names.instance(id);
            if let Err(e) = self
                .inner
                .backend
                .stop(
                    &unit,
                    Duration::from_secs(self.inner.config.settings.runtime.stop_timeout_secs),
                )
                .await
            {
                self.event(id, actor, generation, Event::StopTimeout);
                return Err(Fault::new("stop_timeout", IssueCode::Other, e.to_string()));
            }
            self.inner
                .backend
                .reset_failed(&unit)
                .await
                .map_err(|e| Fault::new("systemd", IssueCode::SystemdUnavailable, e.to_string()))?;
        }
        if token.is_cancelled() {
            return Err(Fault::new("cancelled", IssueCode::Other, "cancelled"));
        }
        self.event(id, actor, generation, Event::UnitInactive);
        self.update(id, actor, generation, |r| {
            r.main_pid = None;
            r.unit = None;
            r.upstream = None;
            r.next_retry_at = None;
        });
        let _ = self.inner.store.set_activated(id, false);
        Ok(noop)
    }
    async fn perform_start(
        &self,
        id: &InstanceId,
        actor: &Arc<Actor>,
        generation: u64,
        token: &CancellationToken,
        handle: &OpHandle,
        deploy_restart: bool,
    ) -> Result<bool, Fault> {
        if matches!(
            self.state(id).map(|s| s.status),
            Some(InstanceStatus::Running | InstanceStatus::Unhealthy)
        ) {
            return Ok(true);
        }
        let result = self
            .start_inner(id, actor, generation, token, handle, deploy_restart)
            .await;
        if let Err(fault) = &result
            && !token.is_cancelled()
        {
            let status = self.state(id).map(|r| r.status);
            if status == Some(InstanceStatus::Starting) {
                self.event(id, actor, generation, Event::DeployFailed(fault.issue));
            }
            if status == Some(InstanceStatus::Preparing) {
                let ev = if fault.issue == IssueCode::ManifestInvalid {
                    Event::PlanInvalid(fault.issue)
                } else {
                    Event::PreparationFailed(fault.issue)
                };
                self.event(id, actor, generation, ev);
            }
            if !matches!(status, Some(InstanceStatus::Crashed)) {
                self.update(id, actor, generation, {
                    let fault = fault.clone();
                    move |r| {
                        r.issue = Some(RuntimeIssue {
                            code: fault.issue,
                            detail: fault.message.clone(),
                            hint: None,
                        });
                        r.message = Some(fault.message);
                    }
                });
            }
        }
        result
    }
    async fn start_inner(
        &self,
        id: &InstanceId,
        actor: &Arc<Actor>,
        generation: u64,
        token: &CancellationToken,
        handle: &OpHandle,
        deploy_restart: bool,
    ) -> Result<bool, Fault> {
        let instance = self
            .inner
            .store
            .instance(id)
            .map_err(store_fault)?
            .ok_or_else(|| Fault::new("not_found", IssueCode::Other, "instance disappeared"))?;
        if !instance.manifest.usable {
            return Err(Fault::new(
                "needs_config",
                IssueCode::ManifestInvalid,
                "manifest contains errors",
            ));
        }
        handle.step("dependencies");
        let order = self.dependencies(id)?;
        for dep in order {
            if &dep == id {
                continue;
            }
            let op = self.request(&dep, Operation::Start, true).map_err(|e| {
                Fault::new(
                    "dependency_failed",
                    IssueCode::DependencyFailed,
                    format!("{dep}: {e}"),
                )
            })?;
            let snapshot = tokio::select! {
                () = token.cancelled() => return Err(Fault::new("cancelled", IssueCode::Other, "cancelled")),
                value = op.wait(Duration::from_secs(self.inner.config.settings.runtime.startup_timeout_secs.saturating_add(self.inner.config.settings.runtime.deploy_timeout_secs).saturating_add(60))) => value,
            };
            if !snapshot.is_some_and(|op| op.status == OperationStatus::Succeeded) {
                return Err(Fault::new(
                    "dependency_failed",
                    IssueCode::DependencyFailed,
                    format!("dependency {dep} failed or timed out"),
                ));
            }
        }
        for name in &instance.manifest.depends_on {
            let project = if name.starts_with("service:") {
                None
            } else {
                self.inner.store.resolve_dependency(name).map_err(store_fault)?
            };
            if project.is_none() {
                let service = name.strip_prefix("service:").unwrap_or(name);
                let timeout = Duration::from_secs(self.inner.config.settings.runtime.startup_timeout_secs);
                tokio::select! {
                    () = token.cancelled() => return Err(Fault::new("cancelled", IssueCode::Other, "cancelled")),
                    result = self.inner.services.ensure_running(service, timeout, token.clone()) => result.map_err(|e| Fault::new("dependency_failed",IssueCode::DependencyFailed,format!("service {service}: {e}")))?,
                }
            }
        }
        let m = &instance.manifest;
        let upstream = m
            .upstream_url
            .as_deref()
            .map(reqwest::Url::parse)
            .transpose()
            .map_err(|e| {
                Fault::new(
                    "needs_config",
                    IssueCode::ManifestInvalid,
                    format!("invalid upstream URL: {e}"),
                )
            })?;
        let command = if deploy_restart {
            m.deploy.restart.as_ref()
        } else {
            m.deploy.start.as_ref().or(m.run.cmd.as_ref())
        };
        if command.is_none() && upstream.is_none() && !matches!(m.serve, ServeMode::Php | ServeMode::Static) {
            return Err(Fault::new(
                "needs_config",
                IssueCode::ManifestInvalid,
                "no run command or upstream URL",
            ));
        }
        if let Some(deploy) = &m.deploy.start
            && upstream.is_none()
            && !deploy.mentions("PORT")
        {
            return Err(Fault::new(
                "needs_config",
                IssueCode::ManifestInvalid,
                "deploy.start needs ${PORT} or upstream_url",
            ));
        }
        let requires_process_port = command.is_some()
            && upstream.is_none()
            && (m.deploy.start.is_some()
                || matches!(m.serve, ServeMode::Proxy | ServeMode::Unknown)
                || command.is_some_and(|c| c.mentions("PORT")));
        let setup_needs_port = m
            .setup
            .as_ref()
            .is_some_and(|s| s.iter().any(|c| c.mentions("PORT")));
        let fixed_port = upstream.as_ref().and_then(|u| u.port_or_known_default());
        let lease = if deploy_restart {
            instance.runtime.port
        } else if requires_process_port || setup_needs_port {
            handle.step("port");
            Some(self.allocate(id).await?)
        } else {
            None
        };
        let port = if upstream.is_some() { fixed_port } else { lease };
        self.update(id, actor, generation, move |r| r.port = port);
        let mut prepared = instance.clone();
        prepared.runtime.port = port;
        handle.step("env");
        self.ensure_shell_env()
            .await
            .map_err(|e| Fault::new("env", IssueCode::Other, e.to_string()))?;
        if token.is_cancelled() {
            return Err(Fault::new("cancelled", IssueCode::Other, "cancelled"));
        }
        handle.step("prepare");
        let prepared_result = {
            // A preparer must observe cancellation and clean up child one-shots before Stop proceeds.
            let _guard = actor.io_lock.lock().await;
            let prepare = self
                .inner
                .preparer
                .ensure_ready(&prepared, port, token.clone(), handle);
            tokio::pin!(prepare);
            tokio::select! {
                () = token.cancelled() => {
                    let limit = Duration::from_secs(self.inner.config.settings.runtime.stop_timeout_secs.saturating_add(15));
                    if tokio::time::timeout(limit, &mut prepare).await.is_err() {
                        tracing::error!(%id, "preparer failed to finish cancellation cleanup");
                    }
                    return Err(Fault::new("cancelled", IssueCode::Other, "cancelled"));
                }
                result = &mut prepare => result.map_err(|e| Fault::new("setup_failed", IssueCode::SetupFailed, e))?,
            }
        };
        if token.is_cancelled() {
            return Err(Fault::new("cancelled", IssueCode::Other, "cancelled"));
        }
        let updated = &prepared_result.manifest;
        if !updated.usable
            || !updated.enabled
            || updated.depends_on != m.depends_on
            || updated.serve != m.serve
            || updated.upstream_url != m.upstream_url
            || updated.run.cmd != m.run.cmd
            || updated.deploy.start != m.deploy.start
            || updated.deploy.restart != m.deploy.restart
            || updated.setup != m.setup
        {
            return Err(Fault::new(
                "manifest_changed",
                IssueCode::ManifestInvalid,
                "launch plan changed during preparation; start again",
            ));
        }
        let mut instance = instance;
        instance.manifest = prepared_result.manifest;
        let worktree_env = prepared_result.worktree_env;
        let m = &instance.manifest;
        let command = if deploy_restart {
            m.deploy.restart.as_ref()
        } else {
            m.deploy.start.as_ref().or(m.run.cmd.as_ref())
        };
        {
            let mut state = actor.state.lock().unwrap();
            if state.generation != generation {
                return Err(Fault::new("cancelled", IssueCode::Other, "cancelled"));
            }
            state.worktree_env = worktree_env.clone();
        }
        // Recheck the lease immediately before spawn in case setup took a long time.
        let prepare_port = port;
        let port = if lease.is_some() && !deploy_restart {
            Some(self.allocate(id).await?)
        } else {
            port
        };
        self.update(id, actor, generation, move |r| r.port = port);
        if command.is_none() {
            self.event(id, actor, generation, Event::Prepared(StartKind::Site));
            if let Some(url) = &upstream {
                self.wait_upstream(url, token).await?;
            }
            let _guard = actor.io_lock.lock().await;
            if token.is_cancelled() {
                return Err(Fault::new("cancelled", IssueCode::Other, "cancelled"));
            }
            if let Err(error) = self.publish_route(&instance, None).await {
                self.event(id, actor, generation, Event::PlanInvalid(IssueCode::RouteFailed));
                return Err(error);
            }
            if token.is_cancelled() {
                return Err(Fault::new("cancelled", IssueCode::Other, "cancelled"));
            }
            self.event(id, actor, generation, Event::Ready);
            self.inner.store.set_activated(id, true).map_err(store_fault)?;
            self.inner.store.record_started(id).map_err(store_fault)?;
            self.start_health(id.clone(), actor.clone(), generation);
            return Ok(false);
        }
        let command = command.expect("checked above");
        let is_deploy = m.deploy.start.is_some();
        let (launch, unit) = self.launch(
            &instance,
            port.unwrap_or(0),
            command.render_shell(),
            is_deploy,
            &worktree_env,
            port != prepare_port,
        )?;
        // A retry must not consume an exit observation from the previous invocation.
        self.update(id, actor, generation, |r| {
            r.unit = None;
            r.next_retry_at = None;
        });
        actor.snapshot.send_replace(None);
        self.event(
            id,
            actor,
            generation,
            Event::Prepared(if is_deploy {
                StartKind::Deploy
            } else {
                StartKind::Unit
            }),
        );
        if token.is_cancelled() {
            return Err(Fault::new("cancelled", IssueCode::Other, "cancelled"));
        }
        if is_deploy {
            handle.step("deploy");
            let _guard = actor.io_lock.lock().await;
            if token.is_cancelled() {
                return Err(Fault::new("cancelled", IssueCode::Other, "cancelled"));
            }
            {
                let mut state = actor.state.lock().unwrap();
                if state.generation != generation {
                    return Err(Fault::new("cancelled", IssueCode::Other, "cancelled"));
                }
                state.deploy_started = true;
            }
            let spec = OneShotSpec {
                unit: unit.clone(),
                timeout: Duration::from_secs(self.inner.config.settings.runtime.deploy_timeout_secs),
                poll_interval: self.inner.config.poll_interval,
            };
            let result = systemd::run_oneshot(self.inner.backend.as_ref(), &spec, token.cancelled())
                .await
                .map_err(|e| Fault::new("deploy_failed", IssueCode::SpawnFailed, e.to_string()))?;
            if result.exit_kind != ExitCodeKind::Exited || result.exit_code != Some(0) {
                self.event(id, actor, generation, Event::DeployFailed(IssueCode::SpawnFailed));
                return Err(Fault::new(
                    "deploy_failed",
                    IssueCode::SpawnFailed,
                    format!("deploy exited {:?}", result.exit_code),
                ));
            }
        } else {
            handle.step("spawn");
            let _guard = actor.io_lock.lock().await;
            if token.is_cancelled() {
                return Err(Fault::new("cancelled", IssueCode::Other, "cancelled"));
            }
            // Once systemd-run starts, let it finish before releasing the lock: a preempting
            // stop then runs after it, so a late start cannot resurrect a stopped service.
            self.inner
                .backend
                .start(&unit)
                .await
                .map_err(|e| Fault::new("spawn_failed", IssueCode::SpawnFailed, e.to_string()))?;
            if token.is_cancelled() {
                return Err(Fault::new("cancelled", IssueCode::Other, "cancelled"));
            }
            let name = unit.unit.clone();
            self.update(id, actor, generation, move |r| {
                r.unit = Some(name);
                r.started_at = Some(Timestamp::now());
            });
        }
        drop(launch);
        handle.step("wait-ready");
        if let Some(url) = &upstream {
            let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"));
            if !local || is_deploy {
                self.wait_upstream(url, token).await?;
            } else if let Some(port) = port {
                self.wait_ready(id, actor, generation, token, port, &unit.unit)
                    .await?;
            }
        } else if requires_process_port {
            if is_deploy {
                self.wait_any_port(port.unwrap_or(0), token).await?;
            } else {
                self.wait_ready(id, actor, generation, token, port.unwrap_or(0), &unit.unit)
                    .await?;
            }
        } else {
            tokio::select! { () = token.cancelled() => return Err(Fault::new("cancelled", IssueCode::Other, "cancelled")), () = tokio::time::sleep(Duration::from_secs(self.inner.config.settings.runtime.settle_secs)) => {} }
        }
        {
            let _guard = actor.io_lock.lock().await;
            if token.is_cancelled() {
                return Err(Fault::new("cancelled", IssueCode::Other, "cancelled"));
            }
            let upstream_addr = self
                .state(id)
                .and_then(|s| s.upstream)
                .or_else(|| port.map(|p| SocketAddr::from((Ipv4Addr::LOCALHOST, p))));
            let route = self.publish_route(&instance, upstream_addr).await;
            let site_companion = matches!(instance.manifest.serve, ServeMode::Php | ServeMode::Static)
                && instance.manifest.upstream_url.is_none()
                && !is_deploy;
            if site_companion && let Err(error) = &route {
                self.event(id, actor, generation, Event::PlanInvalid(IssueCode::RouteFailed));
                drop(_guard);
                if !is_deploy {
                    self.stop_failed_unit(actor, &unit.unit).await;
                }
                return Err(error.clone());
            }
            if !self.event(id, actor, generation, Event::Ready) {
                return Err(Fault::new("cancelled", IssueCode::Other, "cancelled"));
            }
            if let Err(error) = route {
                self.event(id, actor, generation, Event::RouteFailed);
                self.update(id, actor, generation, move |r| {
                    r.message = Some(error.message.clone());
                    r.issue = Some(RuntimeIssue {
                        code: IssueCode::RouteFailed,
                        detail: error.message,
                        hint: None,
                    });
                });
            }
        }
        {
            let mut s = actor.state.lock().unwrap();
            if s.generation == generation {
                s.ready_at = Some(Instant::now());
            }
        }
        let _ = self.inner.store.record_started(id);
        self.start_health(id.clone(), actor.clone(), generation);
        Ok(false)
    }
    async fn publish_route(&self, instance: &Instance, upstream: Option<SocketAddr>) -> Result<(), Fault> {
        let manifest = &instance.manifest;
        let root = manifest.root.as_deref().unwrap_or(".");
        let root = instance.checkout.path.join(root);
        let target = if manifest.upstream_url.is_some() || manifest.deploy.start.is_some() {
            if let Some(url) = &manifest.upstream_url {
                RouteTarget::Url(url.clone())
            } else if let Some(addr) = upstream {
                RouteTarget::Socket(addr)
            } else {
                return Err(Fault::new(
                    "route_failed",
                    IssueCode::RouteFailed,
                    "no upstream address",
                ));
            }
        } else {
            match manifest.serve {
                ServeMode::Php => {
                    let root = confined_path(&instance.checkout.path, &root)?;
                    if !root.is_dir() {
                        return Err(Fault::new(
                            "needs_config",
                            IssueCode::RouteFailed,
                            format!("site root missing: {}", root.display()),
                        ));
                    }
                    self.inner
                        .router
                        .php_ready()
                        .await
                        .map_err(|e| Fault::new("php_unavailable", IssueCode::RouteFailed, e))?;
                    RouteTarget::Php(root)
                }
                ServeMode::Static => {
                    let root = confined_path(&instance.checkout.path, &root)?;
                    if !root.is_dir() {
                        return Err(Fault::new(
                            "needs_config",
                            IssueCode::RouteFailed,
                            format!("site root missing: {}", root.display()),
                        ));
                    }
                    RouteTarget::Static(root)
                }
                _ => {
                    if let Some(url) = &manifest.upstream_url {
                        RouteTarget::Url(url.clone())
                    } else if let Some(addr) = upstream {
                        RouteTarget::Socket(addr)
                    } else {
                        return Err(Fault::new(
                            "route_failed",
                            IssueCode::RouteFailed,
                            "no upstream address",
                        ));
                    }
                }
            }
        };
        self.inner
            .router
            .publish(instance, target)
            .await
            .map_err(|e| Fault::new("route_failed", IssueCode::RouteFailed, e))
    }
    fn dependencies(&self, root: &InstanceId) -> Result<Vec<InstanceId>, Fault> {
        fn visit(
            rt: &Runtime,
            id: &InstanceId,
            stack: &mut Vec<InstanceId>,
            done: &mut HashSet<InstanceId>,
            out: &mut Vec<InstanceId>,
        ) -> Result<(), Fault> {
            if let Some(pos) = stack.iter().position(|x| x == id) {
                return Err(Fault::new(
                    "dependency_cycle",
                    IssueCode::ManifestInvalid,
                    format!(
                        "dependency cycle: {} -> {id}",
                        stack[pos..]
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(" -> ")
                    ),
                ));
            }
            if done.contains(id) {
                return Ok(());
            }
            let instance = rt.inner.store.instance(id).map_err(store_fault)?.ok_or_else(|| {
                Fault::new(
                    "dependency_missing",
                    IssueCode::DependencyFailed,
                    format!("missing dependency {id}"),
                )
            })?;
            if !instance.manifest.enabled {
                return Err(Fault::new(
                    "dependency_disabled",
                    IssueCode::DependencyFailed,
                    format!("disabled dependency {id}"),
                ));
            }
            stack.push(id.clone());
            for name in &instance.manifest.depends_on {
                let project = if name.starts_with("service:") {
                    None
                } else {
                    rt.inner.store.resolve_dependency(name).map_err(store_fault)?
                };
                if let Some(dep) = project {
                    visit(rt, &dep, stack, done, out)?;
                } else {
                    let service = name.strip_prefix("service:").unwrap_or(name);
                    if !rt.inner.services.has(service) {
                        return Err(Fault::new(
                            "dependency_missing",
                            IssueCode::DependencyFailed,
                            format!("unresolved dependency {name}"),
                        ));
                    }
                }
            }
            stack.pop();
            done.insert(id.clone());
            out.push(id.clone());
            Ok(())
        }
        let mut out = Vec::new();
        visit(self, root, &mut Vec::new(), &mut HashSet::new(), &mut out)?;
        Ok(out)
    }
    async fn allocate(&self, id: &InstanceId) -> Result<u16, Fault> {
        let _guard = self.inner.lease_lock.lock().await;
        for _ in 0..32 {
            let expected = self.inner.store.port_leases().map_err(store_fault)?;
            let mut leases = PortLeases::from_table(expected.clone());
            let probe = self.inner.probe.clone();
            let id = id.clone();
            let range = PortRange {
                start: self.inner.config.settings.ports.range_start,
                end: self.inner.config.settings.ports.range_end,
            };
            let reserved = vec![
                self.inner.config.settings.daemon.port,
                self.inner.config.settings.proxy.http_port,
                self.inner.config.settings.proxy.https_port,
                5354,
                2019,
            ];
            let (port, table) = tokio::task::spawn_blocking(move || {
                let outcome = leases
                    .try_allocate(
                        &id,
                        range,
                        &reserved,
                        |port| -> io::Result<PortAvailability<lantern_core::model::PortHolder>> {
                            match probe.port_binding(port, None)? {
                                PortBinding::HeldByOther(holder) => Ok(PortAvailability::Busy(Some(holder))),
                                PortBinding::BoundByUnit { .. } => Ok(PortAvailability::Busy(None)),
                                PortBinding::Free => match bindable(port) {
                                    Ok(true) => Ok(PortAvailability::Free),
                                    Ok(false) => Ok(PortAvailability::Busy(None)),
                                    Err(e) => Err(e),
                                },
                            }
                        },
                    )
                    .map_err(|e| format!("port allocation failed: {e:?}"))?;
                Ok::<_, String>((outcome.port, leases.table().clone()))
            })
            .await
            .map_err(|e| Fault::new("ports", IssueCode::PortsExhausted, e.to_string()))?
            .map_err(|e| Fault::new("ports", IssueCode::PortsExhausted, e))?;
            if self
                .inner
                .store
                .compare_port_leases(&expected, table)
                .map_err(store_fault)?
            {
                return Ok(port);
            }
        }
        Err(Fault::new(
            "ports",
            IssueCode::Other,
            "port lease table changed repeatedly",
        ))
    }
    fn launch(
        &self,
        instance: &Instance,
        port: u16,
        command: String,
        oneshot: bool,
        worktree_env: &BTreeMap<String, String>,
        port_changed: bool,
    ) -> Result<(LaunchSpec, UnitStart), Fault> {
        let project = self
            .inner
            .store
            .project(&instance.project)
            .map_err(store_fault)?
            .ok_or_else(|| Fault::new("not_found", IssueCode::Other, "project disappeared"))?;
        let mut context = VarContext {
            port,
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            domain: instance.domain.clone(),
            url: format!("https://{}", instance.domain),
            project: instance.project.clone(),
            instance: instance.id.clone(),
            branch: instance.checkout.branch.clone(),
            branch_slug: instance.checkout.branch.as_deref().map(branch_slug),
            checkout: instance.checkout.path.clone(),
            primary: project.root,
        };
        if context.branch.is_none()
            && let Some(sha) = &instance.checkout.head
        {
            context.set_detached(sha);
        }
        let cwd = PathBuf::from(instance.manifest.run.cwd.render_data(&context));
        let cwd = if cwd.is_absolute() {
            cwd
        } else {
            instance.checkout.path.join(cwd)
        };
        let cwd = confined_path(&instance.checkout.path, &cwd)?;
        let shell_env = self
            .inner
            .config
            .shell_env_file
            .is_file()
            .then(|| self.inner.config.shell_env_file.clone());
        let effective_path = self.inner.env.lock().unwrap().path.clone();
        let path = if effective_path.is_empty() {
            std::env::var("PATH").unwrap_or_default()
        } else {
            effective_path
        };
        let toolchain = launcher::detect_toolchain(
            &cwd,
            &instance.checkout.path,
            &path,
            None,
            &self.inner.config.home,
        );
        let vars = BTreeMap::from([
            ("PORT".into(), port.to_string()),
            ("HOST".into(), context.host.to_string()),
            ("DOMAIN".into(), context.domain.clone()),
            ("URL".into(), context.url.clone()),
            ("PROJECT".into(), context.project.to_string()),
            ("BRANCH".into(), context.branch.clone().unwrap_or_default()),
            (
                "BRANCH_SLUG".into(),
                context.branch_slug.clone().unwrap_or_default(),
            ),
            (
                "BRANCH_SNAKE".into(),
                context
                    .branch_slug
                    .as_deref()
                    .unwrap_or_default()
                    .replace('-', "_"),
            ),
            ("CHECKOUT".into(), context.checkout.to_string_lossy().into_owned()),
            ("PRIMARY".into(), context.primary.to_string_lossy().into_owned()),
        ]);
        let mut run_env: BTreeMap<String, String> = instance
            .manifest
            .run
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.render_data(&context)))
            .collect();
        if !instance.id.is_primary() {
            run_env.extend(
                instance
                    .manifest
                    .worktree
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.render_data(&context))),
            );
            run_env.extend(worktree_env.clone());
            if port_changed {
                // Values returned by preparation used the original lease; rebuild manifest worktree variables.
                run_env.extend(
                    instance
                        .manifest
                        .worktree
                        .env
                        .iter()
                        .map(|(k, v)| (k.clone(), v.render_data(&context))),
                );
            }
        }
        let env_file = match &instance.manifest.deploy.env_file {
            Some(path) => read_dotenv(&instance.checkout.path, path)?,
            None => BTreeMap::new(),
        };
        let spec = LaunchSpec {
            instance: instance.id.clone(),
            cwd: cwd.clone(),
            shell_env,
            safety_path: shell_env::safety_net_dirs(&self.inner.config.home),
            toolchain,
            runtime_env: self.inner.config.settings.runtime.env.clone(),
            variables: vars,
            env_file,
            run_env,
            pre_start: None,
            post_stop: None,
            command,
        };
        let text = launcher::try_render(&spec)
            .map_err(|e| Fault::new("needs_config", IssueCode::ManifestInvalid, e.to_string()))?;
        let run_dir = &self.inner.config.run_dir;
        std::fs::create_dir_all(run_dir).map_err(io_fault)?;
        std::fs::set_permissions(run_dir, std::fs::Permissions::from_mode(0o700)).map_err(io_fault)?;
        let name = if oneshot {
            self.inner.config.names.operation(
                &instance.id,
                "deploy",
                Timestamp::now().as_microsecond().unsigned_abs(),
            )
        } else {
            self.inner.config.names.instance(&instance.id)
        };
        let script = run_dir.join(format!("{name}.sh"));
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&script)
            .map_err(io_fault)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(io_fault)?;
        file.write_all(text.as_bytes()).map_err(io_fault)?;
        Ok((
            spec,
            UnitStart {
                unit: name,
                instance: instance.id.clone(),
                slice: self.inner.config.names.slice.clone(),
                cwd,
                script,
                post_stop: None,
                stop_timeout: Duration::from_secs(self.inner.config.settings.runtime.stop_timeout_secs),
            },
        ))
    }
    async fn probe(&self, port: u16, unit: Option<String>) -> Result<PortBinding, Fault> {
        let probe = self.inner.probe.clone();
        tokio::task::spawn_blocking(move || probe.port_binding(port, unit.as_deref()))
            .await
            .map_err(|e| Fault::new("probe_failed", IssueCode::Other, e.to_string()))?
            .map_err(io_fault)
    }
    async fn wait_ready(
        &self,
        id: &InstanceId,
        actor: &Arc<Actor>,
        generation: u64,
        token: &CancellationToken,
        port: u16,
        unit: &str,
    ) -> Result<(), Fault> {
        let deadline =
            Instant::now() + Duration::from_secs(self.inner.config.settings.runtime.startup_timeout_secs);
        let mut mismatch_since = None;
        let mut rx = actor.snapshot.subscribe();
        loop {
            if token.is_cancelled() {
                return Err(Fault::new("cancelled", IssueCode::Other, "cancelled"));
            }
            let row = rx.borrow_and_update().clone();
            if let Some(row) = row
                && exited(&row)
            {
                self.handle_exit(id, actor, generation, &row, false);
                return Err(Fault::new(
                    "process_exited",
                    exit_issue(&row).unwrap_or(IssueCode::Other),
                    format!("process exited before readiness: {:?}", row.exit_code),
                ));
            }
            match self.probe(port, Some(unit.to_owned())).await? {
                PortBinding::BoundByUnit { pid, addrs } => {
                    let addr = preferred_address(&addrs, port);
                    let exposed = addrs.iter().any(|a| a.ip().is_unspecified());
                    self.update(id, actor, generation, move |r| {
                        r.main_pid = Some(pid);
                        r.upstream = Some(addr);
                        if exposed {
                            r.message = Some("server is exposed on all interfaces".into());
                        }
                    });
                    return Ok(());
                }
                PortBinding::Free | PortBinding::HeldByOther(_) => {}
            }
            let probe = self.inner.probe.clone();
            let unit_owned = unit.to_owned();
            let others = tokio::task::spawn_blocking(move || probe.unit_listeners(&unit_owned))
                .await
                .map_err(|e| Fault::new("probe_failed", IssueCode::Other, e.to_string()))?
                .map_err(io_fault)?;
            let mismatch = others
                .into_iter()
                .find(|a| a.port() != port && ![9222, 9229, 9230].contains(&a.port()));
            if let Some(addr) = mismatch {
                let since = mismatch_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= self.inner.config.mismatch_after || Instant::now() >= deadline {
                    self.event(id, actor, generation, Event::PortMismatch);
                    self.stop_failed_unit(actor, unit).await;
                    return Err(Fault::new(
                        "port_mismatch",
                        IssueCode::PortMismatch,
                        format!(
                            "server listens on {addr}, not leased port {port}; use ${{PORT}} or upstream_url"
                        ),
                    ));
                }
            } else {
                mismatch_since = None;
            }
            if Instant::now() >= deadline {
                self.event(id, actor, generation, Event::ReadinessTimeout);
                self.stop_failed_unit(actor, unit).await;
                return Err(Fault::new(
                    "readiness_timeout",
                    IssueCode::ReadinessTimeout,
                    format!("server did not listen on port {port}"),
                ));
            }
            tokio::select! { () = token.cancelled() => return Err(Fault::new("cancelled", IssueCode::Other, "cancelled")), _ = rx.changed() => {}, () = tokio::time::sleep(self.inner.config.readiness_interval) => {} }
        }
    }
    async fn stop_failed_unit(&self, actor: &Actor, unit: &str) {
        let _guard = actor.io_lock.lock().await;
        let _ = self
            .inner
            .backend
            .stop(
                unit,
                Duration::from_secs(self.inner.config.settings.runtime.stop_timeout_secs),
            )
            .await;
        let _ = self.inner.backend.reset_failed(unit).await;
    }
    async fn wait_any_port(&self, port: u16, token: &CancellationToken) -> Result<(), Fault> {
        let deadline =
            Instant::now() + Duration::from_secs(self.inner.config.settings.runtime.startup_timeout_secs);
        loop {
            if !matches!(self.probe(port, None).await?, PortBinding::Free) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Fault::new(
                    "readiness_timeout",
                    IssueCode::ReadinessTimeout,
                    "deploy port did not become ready",
                ));
            }
            tokio::select! { ()=token.cancelled()=>return Err(Fault::new("cancelled",IssueCode::Other,"cancelled")), ()=tokio::time::sleep(self.inner.config.readiness_interval)=>{} }
        }
    }
    async fn wait_upstream(&self, url: &reqwest::Url, token: &CancellationToken) -> Result<(), Fault> {
        let host = url
            .host_str()
            .ok_or_else(|| {
                Fault::new(
                    "needs_config",
                    IssueCode::ManifestInvalid,
                    "upstream URL lacks host",
                )
            })?
            .to_string();
        let port = url.port_or_known_default().ok_or_else(|| {
            Fault::new(
                "needs_config",
                IssueCode::ManifestInvalid,
                "upstream URL lacks port",
            )
        })?;
        let deadline =
            Instant::now() + Duration::from_secs(self.inner.config.settings.runtime.startup_timeout_secs);
        loop {
            let connected = tokio::select! { ()=token.cancelled()=>return Err(Fault::new("cancelled",IssueCode::Other,"cancelled")), r=tokio::time::timeout(Duration::from_secs(3),tokio::net::TcpStream::connect((host.as_str(),port)))=>r.is_ok_and(|v|v.is_ok()) };
            if connected {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Fault::new(
                    "readiness_timeout",
                    IssueCode::ReadinessTimeout,
                    "upstream was unreachable",
                ));
            }
            tokio::select! { ()=token.cancelled()=>return Err(Fault::new("cancelled",IssueCode::Other,"cancelled")), ()=tokio::time::sleep(self.inner.config.readiness_interval)=>{} }
        }
    }
    fn handle_exit(
        &self,
        id: &InstanceId,
        actor: &Arc<Actor>,
        generation: u64,
        row: &UnitSnapshot,
        observed_late: bool,
    ) {
        let mut state = actor.state.lock().unwrap();
        if state.generation != generation
            || !matches!(
                state.supervisor.status,
                InstanceStatus::Starting | InstanceStatus::Running | InstanceStatus::Unhealthy
            )
        {
            return;
        }
        let ready = matches!(
            state.supervisor.status,
            InstanceStatus::Running | InstanceStatus::Unhealthy
        );
        let clean = is_clean_exit(row) && ready;
        if state.ready_at.is_some_and(|t| {
            t.elapsed() >= Duration::from_secs(self.inner.config.settings.runtime.stable_after_secs)
        }) {
            state.retry_streak = 0;
        }
        state.ready_at = None;
        if !clean {
            state.retry_streak = state.retry_streak.saturating_add(1);
        }
        let deterministic =
            matches!(row.exit_code, Some(78 | 126 | 127)) && row.exit_kind == ExitCodeKind::Exited;
        let retry = !clean
            && !deterministic
            && !observed_late
            && state.retry_streak <= self.inner.config.settings.runtime.max_retries;
        let issue = if !clean && !retry && !deterministic && !observed_late {
            Some(IssueCode::CrashLoop)
        } else {
            exit_issue(row)
        };
        let (next, _) = sm::transition(&state.supervisor, Event::Exited { clean, retry, issue });
        state.supervisor = next;
        state.supervisor.message = Some(format!("process exited: {} {:?}", row.result, row.exit_code));
        self.publish_supervisor(id, &state.supervisor);
        let delay = self
            .inner
            .config
            .settings
            .runtime
            .backoff_base_secs
            .saturating_mul(
                1u64.checked_shl(state.retry_streak.saturating_sub(1).min(63))
                    .unwrap_or(u64::MAX),
            )
            .min(self.inner.config.settings.runtime.backoff_cap_secs);
        let exit = exit_info(row, observed_late);
        let next_retry = retry
            .then(|| Timestamp::from_second(Timestamp::now().as_second().saturating_add(delay as i64)).ok())
            .flatten();
        let _ = self.inner.store.update_runtime(
            id,
            Box::new(move |r| {
                r.last_exit = Some(exit);
                r.main_pid = None;
                r.upstream = None;
                r.next_retry_at = next_retry;
            }),
        );
        drop(state);
        if retry {
            let weak = Arc::downgrade(&self.inner);
            let shutdown = self.inner.shutdown.clone();
            let id = id.clone();
            let actor = actor.clone();
            tokio::spawn(async move {
                tokio::select! { ()=shutdown.cancelled()=>return, ()=tokio::time::sleep(Duration::from_secs(delay))=>{} }
                loop {
                    let wake = actor.idle.notified();
                    let (valid, busy) = {
                        let s = actor.state.lock().unwrap();
                        (
                            s.generation == generation
                                && s.supervisor.retry_pending
                                && s.supervisor.desired_up,
                            s.active.is_some(),
                        )
                    };
                    if !valid {
                        return;
                    }
                    if !busy {
                        break;
                    }
                    tokio::select! { ()=shutdown.cancelled()=>return, ()=wake=>{} }
                }
                if let Some(inner) = weak.upgrade() {
                    let rt = Runtime { inner };
                    if let Err(e) = rt.request(&id, Operation::Start, false) {
                        tracing::warn!(%id,%e,"automatic retry could not start");
                    }
                }
            });
        }
    }
    fn start_health(&self, id: InstanceId, actor: Arc<Actor>, generation: u64) {
        let weak = Arc::downgrade(&self.inner);
        let shutdown = self.inner.shutdown.clone();
        let health = self.inner.config.settings.health.clone();
        tokio::spawn(async move {
            let client = match reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(health.timeout_secs))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(%e,"health client unavailable");
                    return;
                }
            };
            let mut failures = 0u32;
            let ready = Instant::now();
            loop {
                let delay = health
                    .interval_secs
                    .saturating_mul(1u64.checked_shl(failures.min(6)).unwrap_or(u64::MAX))
                    .min(60);
                tokio::select! { ()=shutdown.cancelled()=>return, ()=tokio::time::sleep(Duration::from_secs(delay.max(1)))=>{} }
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let rt = Runtime { inner };
                {
                    let s = actor.state.lock().unwrap();
                    if s.generation != generation
                        || !matches!(
                            s.supervisor.status,
                            InstanceStatus::Running | InstanceStatus::Unhealthy
                        )
                    {
                        break;
                    }
                }
                let Some(inst) = rt.inner.store.instance(&id).ok().flatten() else {
                    break;
                };
                let started = Instant::now();
                let result: Result<Option<u16>, String> =
                    if let Some(endpoint) = &inst.manifest.health_endpoint {
                        let base = inst
                            .manifest
                            .upstream_url
                            .clone()
                            .or_else(|| inst.runtime.upstream.map(|addr| format!("http://{addr}")));
                        match base.and_then(|b| health_url(&b, endpoint)) {
                            Some(url) => match client.get(url).send().await {
                                Ok(resp) if resp.status().as_u16() < 400 => Ok(Some(resp.status().as_u16())),
                                Ok(resp) => Err(format!("HTTP {}", resp.status())),
                                Err(e) => Err(e.to_string()),
                            },
                            None => Err("health URL unavailable".into()),
                        }
                    } else if let (Some(port), Some(unit)) = (inst.runtime.port, inst.runtime.unit.clone()) {
                        match rt.probe(port, Some(unit)).await {
                            Ok(PortBinding::BoundByUnit { .. }) => Ok(None),
                            Ok(_) => Err("unit is not listening".into()),
                            Err(e) => Err(e.message),
                        }
                    } else if let Some(url) = &inst.manifest.upstream_url {
                        match reqwest::Url::parse(url) {
                            Ok(url) => match (url.host_str(), url.port_or_known_default()) {
                                (Some(host), Some(port)) => match tokio::time::timeout(
                                    Duration::from_secs(health.timeout_secs),
                                    tokio::net::TcpStream::connect((host, port)),
                                )
                                .await
                                {
                                    Ok(Ok(_)) => Ok(None),
                                    Ok(Err(e)) => Err(e.to_string()),
                                    Err(_) => Err("upstream timed out".into()),
                                },
                                _ => Err("upstream has no host or port".into()),
                            },
                            Err(e) => Err(e.to_string()),
                        }
                    } else if inst.manifest.deploy.start.is_some() {
                        match inst.runtime.port {
                            Some(port) => match rt.probe(port, None).await {
                                Ok(PortBinding::Free) => Err("deploy listener is absent".into()),
                                Ok(_) => Ok(None),
                                Err(e) => Err(e.message),
                            },
                            None => Err("deploy listener has no port".into()),
                        }
                    } else if inst.manifest.serve == ServeMode::Php {
                        rt.inner.router.php_ready().await.map(|()| None)
                    } else if inst.manifest.serve == ServeMode::Static {
                        if inst
                            .checkout
                            .path
                            .join(inst.manifest.root.as_deref().unwrap_or("."))
                            .is_dir()
                        {
                            Ok(None)
                        } else {
                            Err("static site root is missing".into())
                        }
                    } else {
                        Ok(None)
                    };
                let ok = result.is_ok();
                failures = if ok { 0 } else { failures.saturating_add(1) };
                let latency = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
                let status = result.as_ref().ok().copied().flatten();
                let error = result.err();
                let state = if ok {
                    HealthState::Healthy
                } else if failures >= rt.inner.config.settings.health.fail_threshold
                    && ready.elapsed() >= Duration::from_secs(rt.inner.config.settings.health.grace_secs)
                {
                    HealthState::Failing
                } else {
                    HealthState::Unknown
                };
                rt.update(&id, &actor, generation, move |r| {
                    let h = &mut r.health;
                    h.state = state;
                    h.checked_at = Some(Timestamp::now());
                    h.latency_ms = Some(latency);
                    h.consecutive_failures = failures;
                    h.last_error = error.clone();
                    h.recent.insert(
                        0,
                        HealthSample {
                            at: Timestamp::now(),
                            ok,
                            status,
                            latency_ms: Some(latency),
                            error,
                        },
                    );
                    h.recent.truncate(10);
                });
                if state == HealthState::Failing {
                    rt.event(&id, &actor, generation, Event::HealthFailed);
                } else if ok {
                    rt.event(&id, &actor, generation, Event::HealthRecovered);
                }
            }
        });
    }
    /// Build a launcher-backed setup or deploy one-shot from shell-rendered code and data.
    pub fn prepare_oneshot(
        &self,
        id: &InstanceId,
        request: OneShotLaunch,
    ) -> Result<OneShotSpec, RuntimeError> {
        if !matches!(request.verb.as_str(), "setup" | "deploy") {
            return Err(RuntimeError::Other(
                "one-shot verb must be setup or deploy".into(),
            ));
        }
        let instance = self
            .inner
            .store
            .instance(id)
            .map_err(|e| RuntimeError::Other(e.to_string()))?
            .ok_or_else(|| RuntimeError::NotFound(id.clone()))?;
        let port = instance
            .runtime
            .port
            .or_else(|| {
                self.inner
                    .store
                    .port_leases()
                    .ok()
                    .and_then(|m| m.get(id).copied())
            })
            .ok_or_else(|| {
                RuntimeError::Other("reserve the instance port before preparing a one-shot".into())
            })?;
        let actor = self.actor(&instance);
        let worktree_env = actor.state.lock().unwrap().worktree_env.clone();
        let (mut launch, mut unit) = self
            .launch(&instance, port, request.shell_command, true, &worktree_env, false)
            .map_err(|e| RuntimeError::Other(e.message))?;
        let cwd = confined_path(&instance.checkout.path, &request.cwd)
            .map_err(|e| RuntimeError::Other(e.message))?;
        launch.cwd = cwd.clone();
        launch.run_env.extend(request.env);
        let text = launcher::try_render(&launch).map_err(|e| RuntimeError::Other(e.to_string()))?;
        let name = self.inner.config.names.operation(
            id,
            &request.verb,
            Timestamp::now().as_microsecond().unsigned_abs(),
        );
        let target = self.inner.config.run_dir.join(format!("{name}.sh"));
        std::fs::rename(&unit.script, &target).map_err(|e| RuntimeError::Other(e.to_string()))?;
        std::fs::write(&target, text).map_err(|e| RuntimeError::Other(e.to_string()))?;
        unit.unit = name;
        unit.script = target;
        unit.cwd = cwd;
        Ok(OneShotSpec {
            unit,
            timeout: request.timeout,
            poll_interval: self.inner.config.poll_interval,
        })
    }
    async fn deploy_command(
        &self,
        id: &InstanceId,
        verb: &str,
    ) -> Result<(systemd::OneShotResult, String), RuntimeError> {
        let instance = self
            .inner
            .store
            .instance(id)
            .map_err(|e| RuntimeError::Other(e.to_string()))?
            .ok_or_else(|| RuntimeError::NotFound(id.clone()))?;
        let command = match verb {
            "logs" => instance.manifest.deploy.logs.as_ref(),
            "status" => instance.manifest.deploy.status.as_ref(),
            _ => None,
        }
        .ok_or_else(|| RuntimeError::Other(format!("deploy.{verb} is not configured")))?;
        let actor = self.actor(&instance);
        let _guard = actor.io_lock.lock().await;
        let spec = self.prepare_oneshot(
            id,
            OneShotLaunch {
                verb: "deploy".into(),
                cwd: instance.checkout.path.clone(),
                shell_command: command.render_shell(),
                env: BTreeMap::new(),
                timeout: Duration::from_secs(15),
            },
        )?;
        let unit = spec.unit.unit.clone();
        let cancel = CancellationToken::new();
        let result = systemd::run_oneshot(self.inner.backend.as_ref(), &spec, cancel.cancelled())
            .await
            .map_err(|e| RuntimeError::Other(e.to_string()))?;
        let output = tokio::process::Command::new("journalctl")
            .args([
                "--user",
                "--no-pager",
                "--output=cat",
                "--lines=1000",
                "--user-unit",
                &unit,
            ])
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| RuntimeError::Other(e.to_string()))?;
        if !output.status.success() {
            return Err(RuntimeError::Other(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        let bytes = &output.stdout[..output.stdout.len().min(1024 * 1024)];
        Ok((result, String::from_utf8_lossy(bytes).into_owned()))
    }
    /// Run configured deploy logs and return its bounded journal output.
    pub async fn deploy_logs(&self, id: &InstanceId) -> Result<String, RuntimeError> {
        let (result, output) = self.deploy_command(id, "logs").await?;
        if result.exit_kind != ExitCodeKind::Exited || result.exit_code != Some(0) {
            return Err(RuntimeError::Other(format!(
                "deploy.logs exited {:?}",
                result.exit_code
            )));
        }
        Ok(output)
    }
    /// Run configured deploy status and report its exit and bounded journal output.
    pub async fn deploy_status(&self, id: &InstanceId) -> Result<DeployStatus, RuntimeError> {
        let (result, output) = self.deploy_command(id, "status").await?;
        Ok(DeployStatus {
            alive: result.exit_kind == ExitCodeKind::Exited && result.exit_code == Some(0),
            exit_code: result.exit_code,
            output,
        })
    }
    /// Republish routes after the networking subsystem recovers.
    pub async fn republish_all(&self) {
        if let Ok(instances) = self.inner.store.instances() {
            for instance in instances.into_iter().filter(|i| {
                matches!(
                    i.runtime.status,
                    InstanceStatus::Running | InstanceStatus::Unhealthy
                )
            }) {
                let actor = self.actor(&instance);
                let generation = actor.state.lock().unwrap().generation;
                let _guard = actor.io_lock.lock().await;
                match self.publish_route(&instance, instance.runtime.upstream).await {
                    Ok(()) => {
                        let mut state = actor.state.lock().unwrap();
                        if state.generation == generation
                            && state.supervisor.issue == Some(IssueCode::RouteFailed)
                        {
                            state.supervisor.issue = None;
                            state.supervisor.message = None;
                            self.publish_supervisor(&instance.id, &state.supervisor);
                        }
                    }
                    Err(error) => {
                        self.event(&instance.id, &actor, generation, Event::RouteFailed);
                        tracing::warn!(id=%instance.id,message=%error.message,"route republish failed");
                    }
                }
            }
        }
    }
    /// Run a tracked, cancellable one-shot using the same journal-backed systemd primitive.
    pub fn run_oneshot(&self, spec: OneShotSpec) -> RuntimeOperation {
        let handle = self
            .inner
            .tracker
            .begin(OperationKind::ApiCall, Some(spec.unit.instance.clone()));
        let operation = RuntimeOperation {
            id: handle.id().clone(),
            tracker: self.inner.tracker.clone(),
        };
        let backend = self.inner.backend.clone();
        tokio::spawn(async move {
            let token = handle.cancel_token();
            let result = systemd::run_oneshot(backend.as_ref(), &spec, token.cancelled()).await;
            let outcome = match result {
                Ok(r) if r.exit_kind == ExitCodeKind::Exited && r.exit_code == Some(0) => {
                    OpOutcome::Succeeded(None)
                }
                Ok(r) => OpOutcome::Failed(ApiError {
                    code: "oneshot_failed".into(),
                    message: format!("one-shot exited {:?}", r.exit_code),
                    details: serde_json::Value::Null,
                }),
                Err(systemd::OneShotError::Cancelled) => OpOutcome::Cancelled,
                Err(e) => OpOutcome::Failed(ApiError {
                    code: "oneshot_failed".into(),
                    message: e.to_string(),
                    details: serde_json::Value::Null,
                }),
            };
            handle.finish(outcome);
        });
        operation
    }
    fn stop_sequence(&self, ids: Vec<InstanceId>) -> RuntimeOperation {
        let handle = self.inner.tracker.begin(OperationKind::StopAll, None);
        let operation = RuntimeOperation {
            id: handle.id().clone(),
            tracker: self.inner.tracker.clone(),
        };
        let rt = self.clone();
        tokio::spawn(async move {
            let token = handle.cancel_token();
            for id in ids {
                if token.is_cancelled() {
                    handle.finish(OpOutcome::Cancelled);
                    return;
                }
                match rt.request(&id, Operation::Stop, true) {
                    Ok(op) => {
                        let result = tokio::select! { ()=token.cancelled()=>{ handle.finish(OpOutcome::Cancelled); return; }, r=op.wait(Duration::from_secs(rt.inner.config.settings.runtime.stop_timeout_secs+30))=>r };
                        if !result.is_some_and(|r| r.status == OperationStatus::Succeeded) {
                            handle.finish(OpOutcome::Failed(ApiError {
                                code: "stop_failed".into(),
                                message: format!("could not stop {id}"),
                                details: serde_json::Value::Null,
                            }));
                            return;
                        }
                    }
                    Err(e) => {
                        handle.finish(OpOutcome::Failed(ApiError {
                            code: "stop_failed".into(),
                            message: e.to_string(),
                            details: serde_json::Value::Null,
                        }));
                        return;
                    }
                }
            }
            handle.finish(OpOutcome::Succeeded(None));
        });
        operation
    }
    fn stop_cascade(&self, target: &InstanceId) -> Result<RuntimeOperation, RuntimeError> {
        let all = self
            .inner
            .store
            .instances()
            .map_err(|e| RuntimeError::Other(e.to_string()))?;
        if !all.iter().any(|i| &i.id == target) {
            return Err(RuntimeError::NotFound(target.clone()));
        }
        let mut ids = Vec::new();
        for i in all {
            if i.id != *target && self.dependencies(&i.id).is_ok_and(|d| d.contains(target)) {
                ids.push(i.id);
            }
        }
        ids.sort_by_key(|id| std::cmp::Reverse(self.dependencies(id).map_or(0, |d| d.len())));
        ids.push(target.clone());
        Ok(self.stop_sequence(ids))
    }
    /// Stop all registered managed instances in dependents-first order.
    pub async fn stop_all(&self) -> Result<RuntimeOperation, RuntimeError> {
        let mut ids: Vec<_> = self
            .inner
            .store
            .instances()
            .map_err(|e| RuntimeError::Other(e.to_string()))?
            .into_iter()
            .filter(|i| i.runtime.status != InstanceStatus::External)
            .map(|i| i.id)
            .collect();
        ids.sort_by_key(|id| std::cmp::Reverse(self.dependencies(id).map_or(0, |d| d.len())));
        Ok(self.stop_sequence(ids))
    }
    /// Read recent journal history for an instance from the configured slice.
    pub async fn log_history(&self, id: &InstanceId, q: HistoryQuery) -> Result<Vec<LogLine>, RuntimeError> {
        let count = q.limit.min(10000).to_string();
        let output = tokio::process::Command::new("journalctl")
            .args([
                "--user",
                "--output=json",
                "--all",
                "--no-pager",
                "--lines",
                &count,
                &format!("_SYSTEMD_USER_SLICE={}", self.inner.config.names.slice),
            ])
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| RuntimeError::Other(e.to_string()))?;
        if !output.status.success() {
            return Err(RuntimeError::Other(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|s| journal::parse_line(s, &self.inner.config.names).ok().flatten())
            .map(|j| j.line)
            .filter(|l| &l.instance == id)
            .collect())
    }
    /// Capture and persist the shell environment, falling back to manager environment on failure.
    pub async fn refresh_shell_env(&self) -> Result<EnvReport, RuntimeError> {
        let _guard = self.inner.env_lock.lock().await;
        self.refresh_shell_env_locked().await
    }
    async fn ensure_shell_env(&self) -> Result<(), RuntimeError> {
        let _guard = self.inner.env_lock.lock().await;
        let needs_refresh = {
            let previous = self.inner.env_sources.lock().unwrap();
            !self.inner.config.shell_env_file.is_file()
                || previous
                    .as_ref()
                    .is_none_or(|v| shell_env::sources_changed(&self.inner.config.home, v))
        };
        if needs_refresh {
            self.refresh_shell_env_locked().await?;
        }
        Ok(())
    }
    async fn refresh_shell_env_locked(&self) -> Result<EnvReport, RuntimeError> {
        let cfg = &self.inner.config;
        let shell = if cfg.settings.runtime.shell.is_empty() {
            std::env::var_os("SHELL")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/bin/bash"))
        } else {
            PathBuf::from(&cfg.settings.runtime.shell)
        };
        let captured = shell_env::capture(
            &shell,
            Duration::from_secs(cfg.settings.runtime.env_capture_timeout_secs),
        )
        .await;
        let manager: BTreeMap<String, String> = std::env::vars().collect();
        let (vars, warning) = match captured {
            Ok(c) => (shell_env::merge_environment(&c.vars, &manager, &cfg.home), None),
            Err(e) => (
                shell_env::merge_environment(&manager, &manager, &cfg.home),
                Some(e.to_string()),
            ),
        };
        let parent = cfg
            .shell_env_file
            .parent()
            .ok_or_else(|| RuntimeError::Other("shell cache has no parent".into()))?;
        std::fs::create_dir_all(parent).map_err(|e| RuntimeError::Other(e.to_string()))?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| RuntimeError::Other(e.to_string()))?;
        shell_env::write_shell_env(&cfg.shell_env_file, &vars)
            .map_err(|e| RuntimeError::Other(e.to_string()))?;
        let report = EnvReport {
            captured_at: Some(Timestamp::now()),
            path: vars.get("PATH").cloned().unwrap_or_default(),
            warning,
        };
        *self.inner.env.lock().unwrap() = report.clone();
        *self.inner.env_sources.lock().unwrap() = Some(shell_env::source_mtimes(&cfg.home));
        Ok(report)
    }
    /// Last capture report; environment values other than PATH are not exposed.
    pub fn env_report(&self) -> EnvReport {
        self.inner.env.lock().unwrap().clone()
    }
    /// Register an added instance so it participates in polling.
    pub fn on_instance_added(&self, instance: &Instance) {
        self.actor(instance);
    }
    /// Remove actor, logs and sticky lease after the registry removes an instance.
    pub async fn on_instance_removed(&self, id: &InstanceId) -> Result<(), RuntimeError> {
        if let Some(actor) = self.inner.actors.lock().unwrap().remove(id) {
            let mut s = actor.state.lock().unwrap();
            s.generation = s.generation.wrapping_add(1);
            if let Some(op) = s.active.take() {
                op.token.cancel();
            }
        }
        self.inner.logs.remove(id);
        let _guard = self.inner.lease_lock.lock().await;
        for _ in 0..32 {
            let expected = self
                .inner
                .store
                .port_leases()
                .map_err(|e| RuntimeError::Other(e.to_string()))?;
            let mut replacement = expected.clone();
            replacement.remove(id);
            if self
                .inner
                .store
                .compare_port_leases(&expected, replacement)
                .map_err(|e| RuntimeError::Other(e.to_string()))?
            {
                return Ok(());
            }
        }
        Err(RuntimeError::Other("port lease table changed repeatedly".into()))
    }
    /// Mark running instances as needing a restart after a manifest change.
    pub fn on_manifest_changed(&self, id: &InstanceId) {
        if let Ok(Some(i)) = self.inner.store.instance(id) {
            let actor = self.actor(&i);
            let generation = actor.state.lock().unwrap().generation;
            self.event(
                id,
                &actor,
                generation,
                Event::ManifestChanged {
                    valid: i.manifest.usable,
                },
            );
            if !i.manifest.enabled
                && !matches!(
                    i.runtime.status,
                    InstanceStatus::Stopped | InstanceStatus::External
                )
                && let Err(error) = self.request(id, Operation::Stop, true)
            {
                tracing::warn!(%id, %error, "disabled instance could not be stopped");
            }
        }
    }
    /// Update the external-listener status using discovery's current observation.
    pub fn note_external(&self, id: &InstanceId, listener: Option<&Listener>) {
        if let Ok(Some(i)) = self.inner.store.instance(id) {
            let actor = self.actor(&i);
            let generation = actor.state.lock().unwrap().generation;
            let event = if listener.is_some() {
                Event::ExternalAppeared
            } else {
                Event::ExternalDisappeared
            };
            self.event(id, &actor, generation, event);
            let external = listener.map(|l| ExternalState {
                port: l.port,
                upstream: l.addr,
                scheme: lantern_core::model::Scheme::Http,
                pid: l.pid,
                label: l.label.clone(),
                extra_ports: Vec::new(),
                since: Timestamp::now(),
            });
            self.update(id, &actor, generation, move |r| {
                if r.status == InstanceStatus::External {
                    r.external = external;
                } else {
                    r.external = None;
                }
            });
        }
    }
    /// Adopt existing managed units and mark missing units stopped without starting processes.
    pub async fn reconcile(&self) -> ReconcileReport {
        let mut report = ReconcileReport::default();
        let instances = match self.inner.store.instances() {
            Ok(v) => v,
            Err(e) => {
                report.errors.push(e.to_string());
                return report;
            }
        };
        let names = match self
            .inner
            .backend
            .list_prefix(&self.inner.config.names.instance_prefix)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                report.errors.push(e.to_string());
                return report;
            }
        };
        let known: HashMap<String, Instance> = instances
            .iter()
            .cloned()
            .map(|i| (self.inner.config.names.instance(&i.id), i))
            .collect();
        for name in &names {
            if !known.contains_key(name) {
                report.orphans.push(name.clone());
            }
        }
        let known_names: Vec<_> = names.into_iter().filter(|n| known.contains_key(n)).collect();
        let snapshots = match self.inner.backend.poll(&known_names).await {
            Ok(v) => v,
            Err(e) => {
                report.errors.push(e.to_string());
                return report;
            }
        };
        let snapshots: HashMap<_, _> = snapshots.into_iter().map(|s| (s.unit.clone(), s)).collect();
        for instance in instances {
            let id = &instance.id;
            let actor = self.actor(&instance);
            let name = self.inner.config.names.instance(id);
            let row = snapshots.get(&name);
            if let Some(row) = row
                && row.load != Load::NotFound
                && row.active == Active::Active
                && row.sub != "exited"
            {
                let binding = match instance.runtime.port {
                    Some(port) => match self.probe(port, Some(name.clone())).await {
                        Ok(PortBinding::BoundByUnit { pid, addrs }) => {
                            Some((pid, preferred_address(&addrs, port)))
                        }
                        Ok(_) => None,
                        Err(error) => {
                            report.errors.push(format!("{id}: {}", error.message));
                            None
                        }
                    },
                    None => None,
                };
                let ready = instance.runtime.port.is_none() || binding.is_some();
                let generation = {
                    let mut s = actor.state.lock().unwrap();
                    s.supervisor = SupervisorState::default();
                    s.generation = s.generation.wrapping_add(1);
                    s.generation
                };
                self.event(id, &actor, generation, Event::ReconcileUnit { ready });
                let row = row.clone();
                let name2 = name.clone();
                self.update(id, &actor, generation, move |r| {
                    r.unit = Some(name2);
                    r.main_pid = binding.map(|(pid, _)| pid).or(row.main_pid);
                    r.upstream = binding.map(|(_, addr)| addr);
                    r.started_at = row.started_at;
                });
                actor.snapshot.send_replace(Some(row.clone()));
                report.adopted.push(id.clone());
                if ready {
                    let upstream = binding.map(|(_, addr)| addr);
                    if let Err(error) = self.publish_route(&instance, upstream).await {
                        self.event(id, &actor, generation, Event::RouteFailed);
                        report.errors.push(format!("{id}: {}", error.message));
                    }
                    self.start_health(id.clone(), actor.clone(), generation);
                } else if let Some(port) = instance.runtime.port {
                    let rt = self.clone();
                    let id = id.clone();
                    let actor = actor.clone();
                    tokio::spawn(async move {
                        let token = CancellationToken::new();
                        if rt
                            .wait_ready(&id, &actor, generation, &token, port, &name)
                            .await
                            .is_ok()
                        {
                            rt.event(&id, &actor, generation, Event::Ready);
                            if let Ok(Some(instance)) = rt.inner.store.instance(&id)
                                && rt
                                    .publish_route(&instance, instance.runtime.upstream)
                                    .await
                                    .is_err()
                            {
                                rt.event(&id, &actor, generation, Event::RouteFailed);
                            }
                            rt.start_health(id, actor, generation);
                        }
                    });
                }
            } else if let Some(row) = row
                && row.active == Active::Failed
            {
                let generation = {
                    let mut s = actor.state.lock().unwrap();
                    s.supervisor.status = InstanceStatus::Starting;
                    s.supervisor.desired_up = true;
                    s.generation
                };
                self.handle_exit(id, &actor, generation, row, true);
                let _ = self.inner.backend.reset_failed(&name).await;
            } else if self.inner.store.activated(id).unwrap_or(false)
                && instance.manifest.run.cmd.is_none()
                && instance.manifest.deploy.start.is_none()
            {
                let generation = {
                    let mut s = actor.state.lock().unwrap();
                    s.supervisor = SupervisorState::default();
                    s.generation = s.generation.wrapping_add(1);
                    s.generation
                };
                self.event(id, &actor, generation, Event::ReconcileSite);
                match self.publish_route(&instance, None).await {
                    Ok(()) => {
                        self.event(id, &actor, generation, Event::Ready);
                        self.start_health(id.clone(), actor.clone(), generation);
                    }
                    Err(error) => {
                        self.event(id, &actor, generation, Event::PlanInvalid(IssueCode::RouteFailed));
                        let message = error.message;
                        report.errors.push(format!("{id}: {message}"));
                        self.update(id, &actor, generation, move |r| {
                            r.message = Some(message);
                        });
                    }
                }
            } else {
                {
                    let mut s = actor.state.lock().unwrap();
                    s.supervisor = SupervisorState::default();
                    s.generation = s.generation.wrapping_add(1);
                }
                let _ = self.inner.store.update_runtime(
                    id,
                    Box::new(|r| {
                        r.status = InstanceStatus::Stopped;
                        r.main_pid = None;
                        r.unit = None;
                        r.upstream = None;
                        r.next_retry_at = None;
                    }),
                );
            }
        }
        let valid: HashSet<_> = known.values().map(|i| i.id.clone()).collect();
        let _guard = self.inner.lease_lock.lock().await;
        for attempt in 0..32 {
            match self.inner.store.port_leases() {
                Ok(expected) => {
                    let mut replacement = expected.clone();
                    replacement.retain(|id, _| valid.contains(id));
                    match self.inner.store.compare_port_leases(&expected, replacement) {
                        Ok(true) => break,
                        Ok(false) if attempt < 31 => {}
                        Ok(false) => report.errors.push("port lease table changed repeatedly".into()),
                        Err(e) => {
                            report.errors.push(e.to_string());
                            break;
                        }
                    }
                }
                Err(e) => {
                    report.errors.push(e.to_string());
                    break;
                }
            }
        }
        report
    }
}

fn confined_path(checkout: &std::path::Path, path: &std::path::Path) -> Result<PathBuf, Fault> {
    let root = std::fs::canonicalize(checkout).map_err(io_fault)?;
    let path = std::fs::canonicalize(path).map_err(io_fault)?;
    if !path.starts_with(&root) {
        return Err(Fault::new(
            "needs_config",
            IssueCode::ManifestInvalid,
            "configured path escapes checkout",
        ));
    }
    Ok(path)
}
fn read_dotenv(checkout: &std::path::Path, path: &str) -> Result<BTreeMap<String, String>, Fault> {
    let full = confined_path(checkout, &checkout.join(path))?;
    let content = std::fs::read(&full).map_err(io_fault)?;
    if content.len() > 1024 * 1024 {
        return Err(Fault::new(
            "needs_config",
            IssueCode::ManifestInvalid,
            "dotenv file exceeds 1 MiB",
        ));
    }
    let content = std::str::from_utf8(&content).map_err(|_| {
        Fault::new(
            "needs_config",
            IssueCode::ManifestInvalid,
            "dotenv file is not UTF-8",
        )
    })?;
    let mut values = BTreeMap::new();
    for (index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, raw) = line.split_once('=').ok_or_else(|| {
            Fault::new(
                "needs_config",
                IssueCode::ManifestInvalid,
                format!("invalid dotenv assignment on line {}", index + 1),
            )
        })?;
        let key = key.trim();
        let mut chars = key.chars();
        if !matches!(chars.next(), Some('a'..='z' | 'A'..='Z' | '_'))
            || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(Fault::new(
                "needs_config",
                IssueCode::ManifestInvalid,
                format!("invalid dotenv key on line {}", index + 1),
            ));
        }
        let raw = raw.trim();
        let value = if let Some(rest) = raw.strip_prefix('\'') {
            let (v, tail) = rest.split_once('\'').ok_or_else(|| {
                Fault::new(
                    "needs_config",
                    IssueCode::ManifestInvalid,
                    format!("unterminated dotenv quote on line {}", index + 1),
                )
            })?;
            if !tail.trim().is_empty() && !tail.trim().starts_with('#') {
                return Err(Fault::new(
                    "needs_config",
                    IssueCode::ManifestInvalid,
                    format!("invalid dotenv tail on line {}", index + 1),
                ));
            }
            v.to_owned()
        } else if let Some(rest) = raw.strip_prefix('"') {
            let mut out = String::new();
            let mut escaped = false;
            let mut closed = false;
            for ch in rest.chars() {
                if escaped {
                    out.push(match ch {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        other => other,
                    });
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    closed = true;
                    break;
                } else {
                    out.push(ch);
                }
            }
            if !closed {
                return Err(Fault::new(
                    "needs_config",
                    IssueCode::ManifestInvalid,
                    format!("unterminated dotenv quote on line {}", index + 1),
                ));
            }
            out
        } else {
            raw.split_once(" #").map_or(raw, |(v, _)| v).trim_end().to_owned()
        };
        values.insert(key.to_owned(), value);
    }
    Ok(values)
}

fn health_url(base: &str, endpoint: &str) -> Option<reqwest::Url> {
    let base = reqwest::Url::parse(base).ok()?;
    let url = base.join(endpoint).ok()?;
    (url.scheme() == base.scheme()
        && url.host() == base.host()
        && url.port_or_known_default() == base.port_or_known_default())
    .then_some(url)
}

fn store_fault(e: super::seams::StoreError) -> Fault {
    Fault::new("store", IssueCode::Other, e.to_string())
}
fn io_fault(e: io::Error) -> Fault {
    Fault::new("io", IssueCode::Other, e.to_string())
}
fn bindable(port: u16) -> io::Result<bool> {
    let v4 = match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => return Ok(false),
        Err(e) => return Err(e),
    };
    let v6 = match TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
        Ok(v) => Some(v),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => return Ok(false),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
            ) =>
        {
            None
        }
        Err(e) => return Err(e),
    };
    drop((v4, v6));
    Ok(true)
}
fn preferred_address(addrs: &[SocketAddr], port: u16) -> SocketAddr {
    let chosen = addrs
        .iter()
        .copied()
        .find(|a| a.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST))
        .or_else(|| {
            addrs
                .iter()
                .copied()
                .find(|a| a.ip() == IpAddr::V6(Ipv6Addr::LOCALHOST))
        })
        .or_else(|| addrs.first().copied())
        .unwrap_or(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
    match chosen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        IpAddr::V6(ip) if ip.is_unspecified() => SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
        _ => chosen,
    }
}
fn exited(row: &UnitSnapshot) -> bool {
    row.load == Load::NotFound
        || row.active == Active::Failed
        || row.active == Active::Inactive
        || (row.active == Active::Active && row.sub == "exited")
}
fn is_clean_exit(row: &UnitSnapshot) -> bool {
    matches!(
        (row.exit_kind, row.exit_code),
        (ExitCodeKind::Exited, Some(0 | 129 | 130 | 143)) | (ExitCodeKind::Killed, Some(1 | 2 | 15))
    )
}
fn exit_issue(row: &UnitSnapshot) -> Option<IssueCode> {
    match (row.exit_kind, row.exit_code) {
        (ExitCodeKind::Exited, Some(78)) => Some(IssueCode::ToolchainMissing),
        (ExitCodeKind::Exited, Some(126 | 127)) => Some(IssueCode::CommandNotFound),
        _ => None,
    }
}
fn exit_info(row: &UnitSnapshot, observed_late: bool) -> ExitInfo {
    let (kind, code, signal) = match row.exit_kind {
        ExitCodeKind::Exited => (ExitKind::Exited, row.exit_code, None),
        ExitCodeKind::Killed | ExitCodeKind::Dumped => (ExitKind::Signaled, None, row.exit_code),
        ExitCodeKind::Unknown => (ExitKind::Unknown, None, None),
    };
    ExitInfo {
        at: Timestamp::now(),
        kind,
        code,
        signal,
        uptime_secs: row
            .started_at
            .map(|t| Timestamp::now().as_second().saturating_sub(t.as_second()).max(0) as u64),
        unit_result: Some(row.result.clone()),
        observed_late,
    }
}
async fn poll_loop(weak: Weak<Inner>) {
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let delay = inner.config.poll_interval;
        let instances = inner.store.instances().unwrap_or_default();
        let rt = Runtime { inner: inner.clone() };
        let targets: Vec<_> = instances
            .iter()
            .filter(|i| {
                matches!(
                    i.runtime.status,
                    InstanceStatus::Starting | InstanceStatus::Running | InstanceStatus::Unhealthy
                )
            })
            .filter_map(|i| {
                let unit = i.runtime.unit.clone()?;
                let actor = rt.actor(i);
                let generation = actor.state.lock().unwrap().generation;
                Some((unit, i.id.clone(), actor, generation))
            })
            .collect();
        let names: Vec<_> = targets.iter().map(|(unit, _, _, _)| unit.clone()).collect();
        if !names.is_empty() {
            match inner.backend.poll(&names).await {
                Ok(rows) => {
                    for row in rows {
                        if let Some((_, id, actor, generation)) =
                            targets.iter().find(|(unit, _, _, _)| *unit == row.unit)
                        {
                            let status = {
                                let guard = actor.state.lock().unwrap();
                                if guard.generation != *generation {
                                    continue;
                                }
                                actor.snapshot.send_replace(Some(row.clone()));
                                guard.supervisor.status
                            };
                            if exited(&row)
                                && matches!(status, InstanceStatus::Running | InstanceStatus::Unhealthy)
                            {
                                rt.handle_exit(id, actor, *generation, &row, false);
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!(%e,"systemd poll failed"),
            }
        }
        drop(rt);
        let shutdown = inner.shutdown.clone();
        drop(inner);
        tokio::select! { ()=shutdown.cancelled()=>return, ()=tokio::time::sleep(delay)=>{} }
    }
}
async fn journal_loop(names: UnitNames, hub: Arc<LogHub>, shutdown: CancellationToken) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut cursor: Option<String> = None;
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        let mut args = match journal::follow_args(&names.slice, cursor.as_deref()) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(%e,"invalid journal arguments");
                return;
            }
        };
        if cursor.is_none() {
            args.push("--lines=0".into());
        }
        let child = tokio::process::Command::new("journalctl")
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn();
        if let Ok(mut child) = child {
            if let Some(stdout) = child.stdout.take() {
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    let line = tokio::select! {()=shutdown.cancelled()=>return, v=lines.next_line()=>v};
                    match line {
                        Ok(Some(line)) => {
                            if let Ok(Some(entry)) = journal::parse_line(&line, &names) {
                                cursor = entry.cursor;
                                hub.push(entry.line);
                            }
                        }
                        _ => break,
                    }
                }
            }
            let _ = child.kill().await;
        }
        tokio::select! {()=shutdown.cancelled()=>return,()=tokio::time::sleep(Duration::from_secs(1))=>{}}
    }
}

#[cfg(test)]
mod health_url_tests {
    use super::health_url;
    #[test]
    fn health_endpoint_cannot_change_the_named_origin() {
        assert_eq!(
            health_url("http://127.0.0.1:39100", "/health").unwrap().as_str(),
            "http://127.0.0.1:39100/health"
        );
        assert!(health_url("http://127.0.0.1:39100", "http://127.0.0.1:39101/").is_none());
        assert!(health_url("http://127.0.0.1:39100", "//example.invalid/").is_none());
        assert!(health_url("http://127.0.0.1:39100", "https://127.0.0.1:39100/").is_none());
    }
}
