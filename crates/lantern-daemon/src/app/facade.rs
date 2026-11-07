//! API facade over the assembled registry, supervisor and checkout service.

use crate::{
    api::facade::{
        DaemonFacade, FacadeResult, InstanceFilter, ListenerFilter, OperationTicket, ProjectFilter,
        ServiceAction,
    },
    app::discovery::Discovery,
    git::Git,
    instances::InstanceService,
    operations::{OpHandle, OpOutcome, OperationTracker},
    registry::RegistryHandle,
    runtime::{HistoryQuery, Runtime, RuntimeError, StartOpts, StopOpts},
};
use async_trait::async_trait;
use lantern_core::{
    Timestamp,
    api::*,
    dirs::Dirs,
    ids::{InstanceId, ProjectId},
    manifest::{self, raw::RawManifest},
    model::{InstanceStatus, Listener, LogLine, ProjectSource},
    settings::Settings,
    state::{ProjectRecord, ProjectRepoRecord, RepoRecord},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::broadcast;

fn error(code: &str, message: impl Into<String>) -> ApiError {
    ApiError {
        code: code.into(),
        message: message.into(),
        details: json!({}),
    }
}
fn internal(e: impl std::fmt::Display) -> ApiError {
    error("internal", e.to_string())
}
fn runtime_error(e: RuntimeError) -> ApiError {
    let code = match &e {
        RuntimeError::NotFound(_) => "instance_not_found",
        RuntimeError::Busy(_) => "operation_in_progress",
        RuntimeError::External => "not_controllable",
        RuntimeError::Disabled => "needs_config",
        RuntimeError::Other(_) => "operation_failed",
    };
    error(code, e.to_string())
}
fn ticket(id: OperationId, instance: Option<InstanceView>) -> OperationTicket {
    OperationTicket {
        id,
        instance,
        created: None,
        forgotten: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Start,
    Stop,
    Restart,
    Prepare(bool),
    Remove,
}
impl Action {
    fn kind(self) -> OperationKind {
        match self {
            Self::Start => OperationKind::Start,
            Self::Stop => OperationKind::Stop,
            Self::Restart => OperationKind::Restart,
            Self::Prepare(_) => OperationKind::Prepare,
            Self::Remove => OperationKind::RemoveInstance,
        }
    }
}
#[derive(Debug, Clone)]
struct Active {
    action: Action,
    id: OperationId,
    owner: InstanceId,
    remove: Option<RemoveInstanceParams>,
}

/// The daemon's concrete facade. Cloneable handles share operation serialization.
#[derive(Debug, Clone)]
pub struct AppFacade {
    /// Registry providing authoritative snapshots.
    pub registry: RegistryHandle,
    /// Runtime owning process supervisors and log streams.
    pub runtime: Arc<Runtime>,
    /// Checkout preparation and worktree service.
    pub instances_service: Arc<InstanceService>,
    /// Git executable for project registration and branch queries.
    pub git: Git,
    /// Serialized discovery worker.
    pub discovery: Discovery,
    /// Shared operation tracker used by HTTP and MCP.
    pub tracker: OperationTracker,
    /// Effective settings at startup.
    pub settings: Settings,
    /// Explicit config, state and data directories.
    pub dirs: Dirs,
    /// Explicit home directory.
    pub home: PathBuf,
    /// Runtime root used by system diagnostics, when a user session exists.
    pub system_runtime: Option<PathBuf>,
    /// Cache path used by system diagnostics.
    pub system_cache: PathBuf,
    /// Startup environment overrides used when validating settings.
    pub environment: BTreeMap<String, String>,
    /// Daemon boot identifier.
    pub boot_id: String,
    /// Daemon start timestamp.
    pub started_at: Timestamp,
    network_health: Option<Arc<super::network::NetworkHealth>>,
    isolated_health: bool,
    active: Arc<Mutex<HashMap<InstanceId, Active>>>,
}
impl AppFacade {
    /// Construct a facade with one shared operation gate.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: RegistryHandle,
        runtime: Arc<Runtime>,
        instances_service: Arc<InstanceService>,
        git: Git,
        discovery: Discovery,
        tracker: OperationTracker,
        settings: Settings,
        dirs: Dirs,
        home: PathBuf,
        boot_id: String,
    ) -> Self {
        Self {
            registry,
            runtime,
            instances_service,
            git,
            discovery,
            tracker,
            settings,
            system_cache: dirs.state.join("cache"),
            system_runtime: None,
            environment: BTreeMap::new(),
            dirs,
            home,
            boot_id,
            started_at: Timestamp::now(),
            network_health: None,
            isolated_health: false,
            active: Arc::default(),
        }
    }
    /// Include live network errors in health; isolated mode avoids inspecting user services.
    pub fn with_network_health(mut self, health: Arc<super::network::NetworkHealth>, isolated: bool) -> Self {
        self.network_health = Some(health);
        self.isolated_health = isolated;
        self
    }
    /// Supply the actual runtime and cache roots used by doctor.
    pub fn with_system_paths(mut self, runtime: Option<PathBuf>, cache: PathBuf) -> Self {
        self.system_runtime = runtime;
        self.system_cache = cache;
        self
    }
    /// Supply the environment that overrides on-disk settings.
    pub fn with_environment(mut self, environment: BTreeMap<String, String>) -> Self {
        self.environment = environment;
        self
    }
    fn view(&self, id: &InstanceId) -> FacadeResult<InstanceView> {
        self.registry
            .instances()
            .into_iter()
            .find(|v| &v.id == id)
            .ok_or_else(|| error("instance_not_found", format!("instance {id} not found")))
    }
    fn summary(&self, id: &ProjectId) -> FacadeResult<ProjectSummary> {
        self.registry
            .projects()
            .into_iter()
            .find(|v| &v.project.id == id)
            .ok_or_else(|| error("project_not_found", format!("project {id} not found")))
    }
    fn detail(&self, id: &ProjectId) -> FacadeResult<ProjectDetail> {
        let summary = self.summary(id)?;
        let instance = self.view(&summary.primary)?;
        let manifest = self
            .registry
            .manifest(&summary.primary)
            .ok_or_else(|| error("needs_config", "manifest unavailable"))?
            .0;
        let depends_on = manifest
            .depends_on
            .iter()
            .filter_map(|n| self.registry.lookup_project(n))
            .collect();
        Ok(ProjectDetail {
            summary,
            instances: self
                .registry
                .instances()
                .into_iter()
                .filter(|i| &i.project == id)
                .collect(),
            depends_on,
            manifest: instance.manifest,
        })
    }
    fn checkout_siblings(&self, id: &InstanceId) -> Vec<InstanceId> {
        let Some(target) = self.registry.instance(id) else {
            return vec![id.clone()];
        };
        let Some(repo) = self.registry.project(&target.project).and_then(|p| p.repo) else {
            return vec![id.clone()];
        };
        let root = checkout_root(&target.checkout.path, &repo.subpath);
        let mut ids: Vec<_> = self
            .registry
            .instances()
            .into_iter()
            .filter_map(|i| {
                let candidate = self.registry.project(&i.project)?.repo?;
                (candidate.common_dir == repo.common_dir
                    && checkout_root(&i.checkout.path, &candidate.subpath) == root)
                    .then_some(i.id)
            })
            .collect();
        if !ids.contains(id) {
            ids.push(id.clone());
        }
        ids
    }
    fn enqueue(
        &self,
        id: InstanceId,
        action: Action,
        remove: Option<RemoveInstanceParams>,
    ) -> FacadeResult<OperationTicket> {
        let view = self.view(&id)?;
        let siblings = if action == Action::Remove {
            self.checkout_siblings(&id)
        } else {
            vec![id.clone()]
        };
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prior = active.get(&id).cloned();
        if let Some(ref current) = prior {
            if current.action == action && current.owner == id && current.remove == remove {
                return Ok(ticket(current.id.clone(), Some(view)));
            }
            if action != Action::Stop
                || !matches!(
                    current.action,
                    Action::Start | Action::Restart | Action::Prepare(_)
                )
            {
                return Err(error(
                    "operation_in_progress",
                    format!("operation {} is already in progress", current.id),
                ));
            }
            self.tracker.cancel(&current.id);
        }
        for sibling in siblings.iter().filter(|sibling| *sibling != &id) {
            if let Some(current) = active.get(sibling) {
                return Err(error(
                    "operation_in_progress",
                    format!("operation {} is using a sibling checkout", current.id),
                ));
            }
        }
        let op = self.tracker.begin(action.kind(), Some(id.clone()));
        let op_id = op.id().clone();
        for sibling in siblings {
            active.insert(
                sibling,
                Active {
                    action,
                    id: op_id.clone(),
                    owner: id.clone(),
                    remove: remove.clone(),
                },
            );
        }
        drop(active);
        let app = self.clone();
        let target = id.clone();
        let result_id = op_id.clone();
        tokio::spawn(async move {
            if let Some(previous) = prior {
                wait_finished(&app.tracker, &previous.id).await;
            }
            let result = app.execute_operation(&target, action, remove, &op).await;
            let outcome = if op.cancel_token().is_cancelled() {
                OpOutcome::Cancelled
            } else {
                match result {
                    Ok(value) => OpOutcome::Succeeded(value),
                    Err(e) => OpOutcome::Failed(e),
                }
            };
            let mut active = app
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            active.retain(|_, a| a.id != op_id);
            drop(active);
            op.finish(outcome);
        });
        Ok(ticket(result_id, Some(view)))
    }
    async fn execute_operation(
        &self,
        id: &InstanceId,
        action: Action,
        remove: Option<RemoveInstanceParams>,
        op: &OpHandle,
    ) -> FacadeResult<Option<Value>> {
        if op.cancel_token().is_cancelled() {
            return Err(error("cancelled", "operation cancelled"));
        }
        match action {
            Action::Start | Action::Stop | Action::Restart => {
                let operation = match action {
                    Action::Start => self.runtime.start(id, StartOpts).await,
                    Action::Stop => self.runtime.stop(id, StopOpts::default()).await,
                    Action::Restart => self.runtime.restart(id).await,
                    _ => unreachable!(),
                }
                .map_err(runtime_error)?;
                let token = op.cancel_token();
                let result = tokio::select! {
                    result = operation.wait(Duration::from_secs(86400)) => result,
                    () = token.cancelled() => { operation.cancel(); operation.wait(Duration::from_secs(86400)).await }
                }.ok_or_else(|| error("operation_failed", "runtime operation disappeared"))?;
                match result.status {
                    OperationStatus::Succeeded => Ok(result.result),
                    OperationStatus::Cancelled => Err(error("cancelled", "operation cancelled")),
                    _ => Err(result
                        .error
                        .unwrap_or_else(|| error("operation_failed", "runtime operation did not finish"))),
                }
            }
            Action::Prepare(force) => {
                let previous = self.view(id)?.runtime.status;
                if !matches!(previous, InstanceStatus::Stopped | InstanceStatus::NeedsConfig) {
                    return Err(error(
                        "operation_in_progress",
                        "stop the instance before standalone preparation",
                    ));
                }
                self.runtime.reserve_port(id).await.map_err(runtime_error)?;
                self.registry
                    .update_runtime(id, |s| s.status = InstanceStatus::Preparing)
                    .map_err(internal)?;
                let result = self.instances_service.ensure_prepared(id, force, op).await;
                self.registry
                    .update_runtime(id, |s| {
                        if s.status == InstanceStatus::Preparing {
                            s.status = previous
                        }
                    })
                    .map_err(internal)?;
                result.map_err(internal)?;
                Ok(None)
            }
            Action::Remove => {
                let opts = remove.unwrap_or_default();
                let siblings = self.checkout_siblings(id);
                if opts.stop {
                    for sibling in &siblings {
                        let state = self.view(sibling)?.runtime.status;
                        if state == InstanceStatus::External {
                            return Err(error("not_controllable", "cannot stop an external sibling"));
                        }
                        if !matches!(state, InstanceStatus::Stopped | InstanceStatus::NeedsConfig) {
                            let child = self
                                .runtime
                                .stop(sibling, StopOpts::default())
                                .await
                                .map_err(runtime_error)?;
                            let token = op.cancel_token();
                            let result = tokio::select! { result = child.wait(Duration::from_secs(600)) => result, () = token.cancelled() => { child.cancel(); child.wait(Duration::from_secs(600)).await } };
                            let result = result
                                .ok_or_else(|| error("operation_failed", "stop operation disappeared"))?;
                            if result.status != OperationStatus::Succeeded {
                                return Err(result
                                    .error
                                    .unwrap_or_else(|| error("operation_failed", "could not stop sibling")));
                            }
                        }
                    }
                }
                let result = self
                    .instances_service
                    .remove_checkout(
                        id,
                        crate::instances::RemoveOpts {
                            force: opts.force,
                            delete_branch: opts.delete_branch,
                            stop: opts.stop,
                        },
                    )
                    .await
                    .map_err(internal)?;
                if result.removed {
                    self.runtime
                        .on_instance_removed(id)
                        .await
                        .map_err(runtime_error)?;
                }
                Ok(Some(
                    json!({"forgotten": result.forgotten, "removed": result.removed, "branch_deleted": result.branch_deleted}),
                ))
            }
        }
    }
}
async fn wait_finished(tracker: &OperationTracker, id: &OperationId) {
    while let Some(op) = tracker.wait(id, Duration::from_secs(86400)).await {
        if !matches!(op.status, OperationStatus::Queued | OperationStatus::Running) {
            break;
        }
    }
}

#[async_trait]
impl DaemonFacade for AppFacade {
    fn listener_interest(&self, active: bool) {
        self.discovery.interest(active);
    }
    async fn operation(&self, id: OperationId, wait: Duration) -> FacadeResult<Operation> {
        self.tracker
            .wait(&id, wait)
            .await
            .ok_or_else(|| error("operation_not_found", format!("operation {id} not found")))
    }
    async fn projects(&self, filter: ProjectFilter) -> FacadeResult<Vec<ProjectSummary>> {
        Ok(self
            .registry
            .projects()
            .into_iter()
            .filter(|p| {
                filter.enabled.as_deref().map_or(p.project.enabled, |v| {
                    v == "any" || (v == "true") == p.project.enabled
                }) && filter.tag.as_ref().is_none_or(|v| p.project.tags.contains(v))
                    && filter.kind.as_ref().is_none_or(|v| {
                        v.split(',').any(|k| {
                            serde_json::to_value(p.project.kind)
                                .ok()
                                .as_ref()
                                .and_then(Value::as_str)
                                == Some(k)
                        })
                    })
                    && filter.q.as_ref().is_none_or(|q| {
                        format!("{} {}", p.project.id, p.project.name)
                            .to_lowercase()
                            .contains(&q.to_lowercase())
                    })
            })
            .collect())
    }
    async fn project(&self, id: ProjectId) -> FacadeResult<ProjectDetail> {
        self.detail(&id)
    }
    async fn register_project(&self, request: RegisterProjectRequest) -> FacadeResult<ProjectSummary> {
        let root = request
            .path
            .canonicalize()
            .map_err(|e| error("bad_request", e.to_string()))?;
        if !root.is_dir() {
            return Err(error("bad_request", "project path must be a directory"));
        }
        let parsed = manifest::load(&root).map_err(internal)?;
        let has_manifest = parsed.is_some() || request.write_manifest;
        if request.write_manifest && parsed.is_some() {
            return Err(error("already_exists", "manifest already exists"));
        }
        let name = request
            .id
            .map(|i| i.to_string())
            .or_else(|| parsed.as_ref().and_then(|p| p.raw.id.clone()))
            .unwrap_or_else(|| {
                root.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        let id = ProjectId::from_name(&name).map_err(|e| error(e.code(), e.to_string()))?;
        if self
            .registry
            .state_snapshot()
            .projects
            .values()
            .any(|p| p.id == id || p.root == root || p.aliases.iter().any(|a| a == id.as_str()))
        {
            return Err(error(
                "already_exists",
                "project identity or path is already registered",
            ));
        }
        let mut overrides = request
            .overrides
            .map(serde_json::from_value::<RawManifest>)
            .transpose()
            .map_err(|e| error("validation_failed", e.to_string()))?
            .unwrap_or_default();
        if let Some(name) = request.name {
            overrides.name = Some(name);
        }
        if let Some(enabled) = request.enabled {
            overrides.enabled = Some(enabled);
        }
        let repo = self.git.probe(&root).await.map_err(internal)?.map(|p| {
            self.registry
                .upsert_repo(p.common_dir.clone(), RepoRecord { slug: id.to_string() });
            ProjectRepoRecord {
                common_dir: p.common_dir,
                subpath: p.subpath,
            }
        });
        let created_manifest = if request.write_manifest {
            let detection = lantern_core::detect::detect_dir(&root);
            let mut value = serde_json::Map::new();
            value.insert("id".into(), json!(id.as_str()));
            if let Some(template) = detection.template {
                value.insert("template".into(), json!(template));
            }
            let text = lantern_core::yaml::to_string(&Value::Object(value)).map_err(internal)?;
            use std::io::Write;
            let path = root.join("lantern.yaml");
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(internal)?;
            file.write_all(text.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(internal)?;
            Some((path, text))
        } else {
            None
        };
        let now = Timestamp::now();
        let registered = self.registry.upsert_project(ProjectRecord {
            id: id.clone(),
            aliases: vec![],
            root,
            repo,
            source: if has_manifest {
                ProjectSource::Manifest
            } else {
                ProjectSource::Manual
            },
            registered_at: now,
            last_seen_at: now,
            missing: false,
            overrides,
        });
        if let Err(error) = registered {
            if let Some((path, source)) = created_manifest
                && std::fs::read_to_string(&path).is_ok_and(|current| current == source)
            {
                let _ = std::fs::remove_file(path);
            }
            return Err(match error {
                crate::registry::Error::Conflict(_) => error_api_conflict(&error),
                _ => internal(error),
            });
        }
        self.registry.flush().await.map_err(internal)?;
        self.summary(&id)
    }
    async fn instances(&self, filter: InstanceFilter) -> FacadeResult<Vec<InstanceView>> {
        Ok(self
            .registry
            .instances()
            .into_iter()
            .filter(|v| {
                filter.project.as_ref().is_none_or(|id| &v.project == id)
                    && filter.status.as_ref().is_none_or(|s| {
                        s.split(',').any(|v2| {
                            serde_json::to_value(v.runtime.status)
                                .ok()
                                .as_ref()
                                .and_then(Value::as_str)
                                == Some(v2)
                        })
                    })
                    && filter.checkout.as_ref().is_none_or(|s| {
                        serde_json::to_value(v.checkout.kind)
                            .ok()
                            .as_ref()
                            .and_then(Value::as_str)
                            == Some(s.as_str())
                    })
                    && filter.q.as_ref().is_none_or(|s| v.id.to_string().contains(s))
            })
            .collect())
    }
    async fn instance(&self, id: InstanceId) -> FacadeResult<InstanceView> {
        self.view(&id)
    }
    async fn project_instances(&self, id: ProjectId) -> FacadeResult<Vec<InstanceView>> {
        self.summary(&id)?;
        Ok(self
            .registry
            .instances()
            .into_iter()
            .filter(|i| i.project == id)
            .collect())
    }
    async fn checkouts(&self, id: ProjectId) -> FacadeResult<Vec<InstanceView>> {
        self.project_instances(id).await
    }
    async fn start(&self, id: InstanceId) -> FacadeResult<OperationTicket> {
        self.enqueue(id, Action::Start, None)
    }
    async fn stop(&self, id: InstanceId) -> FacadeResult<OperationTicket> {
        self.enqueue(id, Action::Stop, None)
    }
    async fn restart(&self, id: InstanceId) -> FacadeResult<OperationTicket> {
        self.enqueue(id, Action::Restart, None)
    }
    async fn prepare(&self, id: InstanceId, request: PrepareRequest) -> FacadeResult<OperationTicket> {
        self.enqueue(id, Action::Prepare(request.force), None)
    }
    async fn remove_instance(
        &self,
        id: InstanceId,
        params: RemoveInstanceParams,
    ) -> FacadeResult<OperationTicket> {
        if id.is_primary() {
            return Err(error("cannot_remove_primary", "cannot remove primary instance"));
        }
        self.enqueue(id, Action::Remove, Some(params))
    }
    async fn logs(&self, id: InstanceId, params: LogParams) -> FacadeResult<LogPage> {
        self.view(&id)?;
        let limit = params.tail.unwrap_or(100).min(5000) as usize;
        let mut lines = self
            .runtime
            .log_history(&id, HistoryQuery { limit })
            .await
            .map_err(runtime_error)?;
        if let Some(stream) = params.stream {
            lines.retain(|l| l.stream == stream);
        }
        if let Some(grep) = params.grep {
            lines.retain(|l| l.line.contains(&grep));
        }
        if let Some(since) = params.since {
            let ts: Timestamp = since
                .parse()
                .map_err(|_| error("bad_request", "invalid since timestamp"))?;
            lines.retain(|l| l.ts >= ts);
        }
        Ok(LogPage {
            instance: id,
            buffered: lines.len() as u64,
            lines,
            truncated: false,
        })
    }
    async fn subscribe_logs(
        &self,
        id: InstanceId,
        tail: usize,
    ) -> FacadeResult<(Vec<LogLine>, broadcast::Receiver<LogLine>)> {
        self.view(&id)?;
        Ok(self.runtime.subscribe_logs(&id, tail))
    }
    async fn event_snapshot(&self) -> FacadeResult<Vec<Event>> {
        let mut events: Vec<_> = self
            .registry
            .projects()
            .into_iter()
            .map(|project| Event::ProjectUpdated { project })
            .collect();
        events.extend(
            self.registry
                .instances()
                .into_iter()
                .map(|instance| Event::InstanceUpdated { instance }),
        );
        let (seq, listeners) = self.registry.listeners();
        events.push(Event::ListenersChanged { seq, listeners });
        events.push(Event::SuggestionsChanged {
            suggestions: self.registry.suggestions(),
        });
        Ok(events)
    }
    async fn listeners(&self, filter: ListenerFilter) -> FacadeResult<Vec<Listener>> {
        let rows = if filter.include_hidden == Some(true) {
            let snapshot = self.discovery.cached();
            snapshot
                .into_iter()
                .flat_map(|s| s.listeners.into_iter().chain(s.hidden))
                .collect()
        } else {
            self.registry.listeners().1
        };
        Ok(rows
            .into_iter()
            .filter(|l| {
                filter.matched.is_none_or(|x| x == l.matched.is_some())
                    && filter
                        .project
                        .as_ref()
                        .is_none_or(|p| l.project.as_ref() == Some(p))
                    && filter.port.is_none_or(|p| l.port == p)
            })
            .collect())
    }
    async fn scan_listeners(&self) -> FacadeResult<Vec<Listener>> {
        Ok(self.discovery.scan().await.map_err(internal)?.listeners)
    }
    async fn services(&self) -> FacadeResult<Vec<ServiceStatus>> {
        Err(error("unavailable", "shared services are not assembled"))
    }
    async fn service(&self, _: String) -> FacadeResult<ServiceStatus> {
        Err(error("unavailable", "shared services are not assembled"))
    }
    async fn service_action(&self, _: String, _: ServiceAction) -> FacadeResult<ServiceStatus> {
        Err(error("unavailable", "shared services are not assembled"))
    }
    async fn templates(&self) -> FacadeResult<Vec<Value>> {
        Err(error("unavailable", "template management is not assembled"))
    }
    async fn template(&self, _: String) -> FacadeResult<Value> {
        Err(error("unavailable", "template management is not assembled"))
    }
    async fn create_template(&self, _: Value) -> FacadeResult<Value> {
        Err(error("unavailable", "template management is not assembled"))
    }
    async fn put_template(&self, _: String, _: Value) -> FacadeResult<(Value, bool)> {
        Err(error("unavailable", "template management is not assembled"))
    }
    async fn delete_template(&self, _: String) -> FacadeResult<()> {
        Err(error("unavailable", "template management is not assembled"))
    }
    async fn intel(&self, _: InstanceId, _: Option<u32>) -> FacadeResult<IntelSnapshot> {
        Err(error("unavailable", "project intelligence is not assembled"))
    }
    async fn refresh_intel(&self, _: InstanceId) -> FacadeResult<IntelSnapshot> {
        Err(error("unavailable", "project intelligence is not assembled"))
    }
    async fn refresh_all_intel(&self) -> FacadeResult<IntelRefreshReport> {
        Err(error("unavailable", "project intelligence is not assembled"))
    }
    async fn docs(&self, _: InstanceId) -> FacadeResult<Vec<DocEntry>> {
        Err(error("unavailable", "document discovery is not assembled"))
    }
    async fn doc(
        &self,
        _: InstanceId,
        _: String,
        _: Option<u64>,
        _: Option<u64>,
    ) -> FacadeResult<DocContent> {
        Err(error("unavailable", "document discovery is not assembled"))
    }
    async fn endpoints(
        &self,
        _: InstanceId,
        _: Option<String>,
        _: Option<u32>,
    ) -> FacadeResult<Vec<EndpointEntry>> {
        Err(error("unavailable", "endpoint discovery is not assembled"))
    }
    async fn profiles(&self) -> FacadeResult<Vec<Value>> {
        let profiles = lantern_core::profiles::Profiles::load_dir(&self.dirs.config.join("profiles"))
            .map_err(internal)?
            .0;
        let mut names: std::collections::BTreeSet<String> = lantern_core::profiles::BUILTIN_NAMES
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        names.extend(profiles.user.keys().cloned());
        names
            .into_iter()
            .map(|name| {
                profile_value(
                    &name,
                    profiles.get(&name).expect("listed profile"),
                    self.registry.active_profile().as_deref() == Some(&name),
                )
            })
            .collect()
    }
    async fn profile(&self, name: String) -> FacadeResult<Value> {
        let profile = load_profile(&self.dirs, &name)?;
        profile_value(
            &name,
            profile,
            self.registry.active_profile().as_deref() == Some(&name),
        )
    }
    async fn put_profile(&self, name: String, value: Value) -> FacadeResult<(Value, bool)> {
        if !lantern_core::profiles::valid_name(&name) {
            return Err(error("validation_failed", "invalid profile name"));
        }
        let profile: lantern_core::profiles::Profile =
            serde_json::from_value(value).map_err(|e| error("validation_failed", e.to_string()))?;
        if profile
            .env
            .keys()
            .any(|key| key == "PORT" || key == "HOST" || key.starts_with("LANTERN_"))
        {
            return Err(error(
                "validation_failed",
                "profile environment overrides a reserved Lantern variable",
            ));
        }
        let source = lantern_core::yaml::to_string(&profile).map_err(internal)?;
        let dir = self.dirs.config.join("profiles");
        let path = dir.join(format!("{name}.yaml"));
        let parsed = lantern_core::profiles::parse_profile(&source, &path);
        if !parsed.usable {
            return Err(error(
                "validation_failed",
                format!("invalid profile: {:?}", parsed.diagnostics),
            ));
        }
        let existed = lantern_core::profiles::Profiles::load_dir(&dir)
            .map_err(internal)?
            .0
            .get(&name)
            .is_some();
        atomic_config_write(&path, source.as_bytes()).map_err(internal)?;
        Ok((
            profile_value(
                &name,
                profile,
                self.registry.active_profile().as_deref() == Some(&name),
            )?,
            !existed,
        ))
    }
    async fn delete_profile(&self, name: String) -> FacadeResult<()> {
        if !lantern_core::profiles::valid_name(&name) {
            return Err(error("validation_failed", "invalid profile name"));
        }
        let dir = self.dirs.config.join("profiles");
        let yaml = dir.join(format!("{name}.yaml"));
        let yml = dir.join(format!("{name}.yml"));
        let user_file = if yaml.is_file() {
            yaml
        } else if yml.is_file() {
            yml
        } else {
            return Err(error("not_found", "no user profile exists with that name"));
        };
        if self.registry.active_profile().as_deref() == Some(&name)
            && lantern_core::profiles::builtin(&name).is_none()
        {
            return Err(error(
                "operation_in_progress",
                "activate another profile before deleting the active profile",
            ));
        }
        std::fs::remove_file(user_file).map_err(internal)
    }
    async fn activate_profile(&self, name: String) -> FacadeResult<Vec<OperationTicket>> {
        let profile = load_profile(&self.dirs, &name)?;
        if !profile.services.is_empty() || profile.stop_others {
            return Err(error("unavailable", "profile service control is not assembled"));
        }
        self.registry.set_active_profile(Some(name)).map_err(internal)?;
        self.registry.flush().await.map_err(internal)?;
        let mut selected = std::collections::BTreeSet::new();
        for target in profile.auto_start {
            if target == "*" {
                selected.extend(
                    self.registry
                        .instances()
                        .into_iter()
                        .filter(|i| {
                            i.id.is_primary()
                                && self.registry.project(&i.project).is_some_and(|p| p.enabled)
                                && i.manifest.run_cmd.is_some()
                        })
                        .map(|i| i.id),
                );
            } else if let Ok(mut id) = InstanceId::parse(&target) {
                if id.is_primary()
                    && let Some(project) = self.registry.lookup_project(&target)
                {
                    id = InstanceId::primary(&project);
                }
                if self.registry.instance(&id).is_some() {
                    selected.insert(id);
                } else {
                    tracing::warn!(%id, "profile target not found");
                }
            }
        }
        let mut operations = Vec::new();
        for id in selected {
            let state = self.view(&id)?.runtime.status;
            if !matches!(
                state,
                InstanceStatus::Running | InstanceStatus::Starting | InstanceStatus::Unhealthy
            ) {
                operations.push(self.enqueue(id, Action::Start, None)?);
            }
        }
        Ok(operations)
    }
    async fn suggestions(&self) -> FacadeResult<Vec<Suggestion>> {
        Ok(self.registry.suggestions())
    }
    async fn dismiss_suggestion(&self, path: PathBuf) -> FacadeResult<()> {
        self.registry.dismiss_suggestion(&path);
        Ok(())
    }
    async fn unregister_project(&self, id: ProjectId, params: UnregisterParams) -> FacadeResult<()> {
        self.summary(&id)?;
        let instances = self.project_instances(id.clone()).await?;
        for instance in &instances {
            if matches!(instance.runtime.status, InstanceStatus::External) {
                return Err(error(
                    "not_controllable",
                    "external processes cannot be stopped by Lantern",
                ));
            }
            if !matches!(
                instance.runtime.status,
                InstanceStatus::Stopped | InstanceStatus::NeedsConfig
            ) {
                if !params.stop {
                    return Err(error(
                        "operation_in_progress",
                        "stop project instances before unregistering",
                    ));
                }
                let ticket = self.enqueue(instance.id.clone(), Action::Stop, None)?;
                let result = self
                    .tracker
                    .wait(&ticket.id, Duration::from_secs(600))
                    .await
                    .ok_or_else(|| error("internal", "stop operation disappeared"))?;
                if result.status != OperationStatus::Succeeded {
                    return Err(result
                        .error
                        .unwrap_or_else(|| error("operation_failed", "could not stop instance")));
                }
            }
        }
        if params.remove_worktrees {
            for instance in instances
                .iter()
                .filter(|i| i.checkout.kind == lantern_core::model::CheckoutKind::Managed)
            {
                let ticket = self.enqueue(
                    instance.id.clone(),
                    Action::Remove,
                    Some(RemoveInstanceParams::default()),
                )?;
                let result = self
                    .tracker
                    .wait(&ticket.id, Duration::from_secs(600))
                    .await
                    .ok_or_else(|| error("internal", "remove operation disappeared"))?;
                if result.status != OperationStatus::Succeeded {
                    return Err(result
                        .error
                        .unwrap_or_else(|| error("operation_failed", "could not remove worktree")));
                }
            }
        }
        self.registry.remove_project(&id).map_err(internal)?;
        for instance in instances {
            self.runtime
                .on_instance_removed(&instance.id)
                .await
                .map_err(runtime_error)?;
        }
        self.registry.flush().await.map_err(internal)
    }
    async fn run_branch(
        &self,
        id: ProjectId,
        request: CreateInstanceRequest,
    ) -> FacadeResult<OperationTicket> {
        use crate::instances::{BranchTarget, FetchPolicy, Outcome, RunBranchRequest};
        self.summary(&id)?;
        let target = match request.target {
            RunTarget::Branch(name) => BranchTarget::Branch(name),
            RunTarget::NewBranch { name, base } => BranchTarget::NewBranch { name, base },
            RunTarget::Detached(rev) => BranchTarget::Detached(rev),
            RunTarget::Pr(pr) => BranchTarget::Pr(pr),
        };
        if matches!(target, BranchTarget::Pr(_)) {
            return Err(error(
                "validation_failed",
                "pull requests require origin verification or explicit confirmation",
            ));
        }
        let op = self.tracker.begin(OperationKind::CreateInstance, None);
        let op_id = op.id().clone();
        let app = self.clone();
        tokio::spawn(async move {
            let req = RunBranchRequest {
                project: id,
                target,
                start: request.start,
                pin: request.pin.unwrap_or(!request.start),
                fetch: if request.fetch {
                    FetchPolicy::Auto
                } else {
                    FetchPolicy::Never
                },
                detach: false,
                remote: None,
                pr_trust: None,
            };
            let result = match app.instances_service.run_branch_unprepared(req, &op).await {
                Ok(result) => {
                    let created = matches!(result.outcome, Outcome::CreatedWorktree(_));
                    let instance = result.instance;
                    let status = app.view(&instance).map(|v| v.runtime.status);
                    let action = if result.start {
                        Some(Action::Start)
                    } else if status
                        .is_ok_and(|s| matches!(s, InstanceStatus::Running | InstanceStatus::Unhealthy))
                    {
                        None
                    } else {
                        Some(Action::Prepare(false))
                    };
                    if let Some(action) = action {
                        op.step(if result.start { "Starting" } else { "Preparing" });
                        match app.enqueue(instance.clone(), action, None) {
                            Ok(ticket) => {
                                let cancel = op.cancel_token();
                                let result = tokio::select! {
                                    result = app.tracker.wait(&ticket.id, Duration::from_secs(86400)) => result,
                                    () = cancel.cancelled() => { app.tracker.cancel(&ticket.id); app.tracker.wait(&ticket.id, Duration::from_secs(86400)).await }
                                };
                                match result {
                                    Some(done) if done.status == OperationStatus::Succeeded => {
                                        Ok(json!({"instance":instance, "created":created}))
                                    }
                                    Some(done) => Err(done.error.unwrap_or_else(|| {
                                        error("operation_failed", "checkout operation did not complete")
                                    })),
                                    None => Err(error("operation_failed", "checkout operation disappeared")),
                                }
                            }
                            Err(e) => Err(e),
                        }
                    } else {
                        Ok(json!({"instance":instance, "created":created}))
                    }
                }
                Err(e) => Err(internal(e)),
            };
            let outcome = if op.cancel_token().is_cancelled() {
                OpOutcome::Cancelled
            } else {
                match result {
                    Ok(value) => OpOutcome::Succeeded(Some(value)),
                    Err(e) => OpOutcome::Failed(e),
                }
            };
            op.finish(outcome);
        });
        Ok(ticket(op_id, None))
    }
    async fn scan_projects(&self, request: ScanRequest) -> FacadeResult<ScanReport> {
        if let Some(roots) = request.roots {
            let mut settings = self.settings.workspaces.clone();
            settings.roots = roots.iter().map(|p| p.to_string_lossy().into_owned()).collect();
            let scan =
                crate::registry::scan::scan_roots(&settings, &self.home, &self.dirs.data.join("worktrees"));
            let mut report = ScanReport {
                registered: vec![],
                already_known: 0,
                skipped: scan.skipped,
            };
            for hit in scan.manifests {
                if self
                    .registry
                    .state_snapshot()
                    .projects
                    .values()
                    .any(|p| p.root == hit.path)
                {
                    report.already_known += 1;
                    continue;
                }
                let request = RegisterProjectRequest {
                    path: hit.path.clone(),
                    id: None,
                    name: None,
                    enabled: None,
                    overrides: None,
                    write_manifest: false,
                };
                match self.register_project(request).await {
                    Ok(project) => report.registered.push(project),
                    Err(e) => report.skipped.push(ScanSkip {
                        path: hit.path,
                        reason: e.message,
                    }),
                }
            }
            return Ok(report);
        }
        let report = self.registry.scan_workspace(&self.dirs).map_err(internal)?;
        Ok(ScanReport {
            registered: report
                .registered
                .iter()
                .filter_map(|id| self.summary(id).ok())
                .collect(),
            already_known: report.already_known,
            skipped: report.skipped,
        })
    }
    async fn reload_project(&self, id: ProjectId, request: ReloadRequest) -> FacadeResult<ProjectDetail> {
        if request.discard_overrides {
            let mut record = self
                .registry
                .project_record(&id)
                .ok_or_else(|| error("project_not_found", format!("project {id} not found")))?;
            record.overrides = RawManifest::default();
            self.registry.upsert_project(record).map_err(internal)?;
        } else {
            self.registry.reload_project(&id).map_err(internal)?;
        }
        for instance in self.registry.instances().into_iter().filter(|i| i.project == id) {
            self.runtime.on_manifest_changed(&instance.id);
        }
        self.detail(&id)
    }
    async fn patch_project(
        &self,
        id: ProjectId,
        request: PatchProjectRequest,
    ) -> FacadeResult<ProjectDetail> {
        let mut record = self
            .registry
            .project_record(&id)
            .ok_or_else(|| error("project_not_found", format!("project {id} not found")))?;
        if let Some(value) = request.overrides {
            record.overrides =
                serde_json::from_value(value).map_err(|e| error("validation_failed", e.to_string()))?;
        }
        if let Some(name) = request.name {
            record.overrides.name = Some(name);
        }
        if let Some(enabled) = request.enabled {
            record.overrides.enabled = Some(enabled);
        }
        if let Some(tags) = request.tags {
            record.overrides.tags = Some(tags);
        }
        let desired = request.id.unwrap_or_else(|| id.clone());
        if desired != id {
            let renamed = {
                let active = self
                    .active
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if active.keys().any(|instance| instance.project() == id) {
                    return Err(error("operation_in_progress", "project has an active operation"));
                }
                self.registry
                    .rename_project(&id, desired.clone())
                    .map_err(|e| error("operation_in_progress", e.to_string()))?
            };
            for (old, _) in renamed {
                self.runtime
                    .on_instance_removed(&old)
                    .await
                    .map_err(runtime_error)?;
            }
            let current = self
                .registry
                .project_record(&desired)
                .ok_or_else(|| error("project_not_found", "renamed project disappeared"))?;
            record.id = desired.clone();
            record.aliases = current.aliases;
        }
        self.registry.upsert_project(record).map_err(internal)?;
        let id = desired;
        for instance in self.registry.instances().into_iter().filter(|i| i.project == id) {
            self.runtime.on_manifest_changed(&instance.id);
        }
        self.detail(&id)
    }
    async fn sync_project(&self, id: ProjectId) -> FacadeResult<Vec<InstanceView>> {
        let project = self
            .registry
            .project(&id)
            .ok_or_else(|| error("project_not_found", format!("project {id} not found")))?;
        if let Some(repo) = project.repo {
            self.instances_service
                .refresh_repo(&repo.common_dir)
                .await
                .map_err(internal)?;
        }
        self.project_instances(id).await
    }
    async fn branches(&self, id: ProjectId) -> FacadeResult<Vec<BranchInfo>> {
        let project = self
            .registry
            .project(&id)
            .ok_or_else(|| error("project_not_found", format!("project {id} not found")))?;
        let repo = project
            .repo
            .ok_or_else(|| error("bad_request", "project is not a git repository"))?;
        Ok(self
            .git
            .repo(repo.common_dir)
            .branches(100_000)
            .await
            .map_err(internal)?
            .into_iter()
            .map(|b| BranchInfo {
                name: b.name,
                remote: b.remote,
                oid: b.oid,
                upstream: b.upstream,
                ahead: b.ahead,
                behind: b.behind,
                gone: b.gone,
                committed_at: b.committed_at,
                checked_out_at: b.checked_out_at,
            })
            .collect())
    }
    async fn dependencies(&self, project: Option<ProjectId>) -> FacadeResult<DependencyGraph> {
        let mut graph = std::collections::BTreeMap::<ProjectId, DependencyEntry>::new();
        for summary in self.registry.projects() {
            let id = summary.project.id;
            let deps = self
                .registry
                .manifest(&summary.primary)
                .map(|(m, _)| {
                    m.depends_on
                        .iter()
                        .filter_map(|d| self.registry.lookup_project(d))
                        .collect()
                })
                .unwrap_or_default();
            graph.insert(
                id,
                DependencyEntry {
                    depends_on: deps,
                    depended_by: vec![],
                },
            );
        }
        let edges: Vec<_> = graph
            .iter()
            .flat_map(|(id, row)| row.depends_on.iter().map(move |dep| (id.clone(), dep.clone())))
            .collect();
        for (id, dep) in edges {
            if let Some(entry) = graph.get_mut(&dep) {
                entry.depended_by.push(id);
            }
        }
        if let Some(project) = project {
            graph.retain(|id, _| *id == project);
        }
        Ok(DependencyGraph(graph))
    }
    async fn pin(&self, id: InstanceId, pinned: bool) -> FacadeResult<InstanceView> {
        self.instances_service.set_pinned(&id, pinned).map_err(internal)?;
        self.view(&id)
    }
    async fn rename_instance(
        &self,
        id: InstanceId,
        request: RenameInstanceRequest,
    ) -> FacadeResult<InstanceView> {
        let new = {
            let active = self
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if active.contains_key(&id) {
                return Err(error("operation_in_progress", "instance has an active operation"));
            }
            let view = self.view(&id)?;
            if !matches!(
                view.runtime.status,
                InstanceStatus::Stopped | InstanceStatus::NeedsConfig
            ) {
                return Err(error(
                    "operation_in_progress",
                    "stop the instance before renaming",
                ));
            }
            self.instances_service
                .rename_instance(&id, &request.slug)
                .map_err(internal)?
        };
        self.runtime
            .on_instance_removed(&id)
            .await
            .map_err(runtime_error)?;
        self.view(&new)
    }
    async fn removal_check(&self, id: InstanceId) -> FacadeResult<RemovalCheck> {
        use crate::instances::Blocker as B;
        let c = self
            .instances_service
            .removal_check(&id)
            .await
            .map_err(internal)?;
        let blockers = c
            .blockers
            .into_iter()
            .map(|b| match b {
                B::NotManaged => Blocker::NotManaged,
                B::Running => Blocker::Running,
                B::Locked(s) => Blocker::Locked(s),
                B::Dirty(n) => Blocker::Dirty(n),
                B::Unpushed(n) => Blocker::Unpushed(n),
                B::Orphaned(n) => Blocker::Orphaned(n),
            })
            .collect();
        Ok(RemovalCheck {
            blockers,
            dirty: c
                .dirty
                .into_iter()
                .map(|d| json!({"path":d.path, "kind":d.kind.to_string()}))
                .collect(),
            unpushed: c.unpushed,
            orphaned: c.orphaned,
            has_submodules: c.has_submodules,
            worktree_env: c.worktree_env,
            branch: c.branch.map(Value::String),
        })
    }
    async fn adopt(&self, id: InstanceId, request: AdoptRequest) -> FacadeResult<InstanceView> {
        let view = self.view(&id)?;
        if view.runtime.status != InstanceStatus::External {
            return Err(error("not_controllable", "instance has no external listener"));
        }
        if request
            .port
            .is_some_and(|port| view.runtime.external.as_ref().is_none_or(|e| e.port != port))
        {
            return Err(error(
                "bad_request",
                "requested port is not the current external listener",
            ));
        }
        self.registry.set_adopt(&id, true).map_err(internal)?;
        self.runtime.republish_all().await;
        self.view(&id)
    }
    async fn unadopt(&self, id: InstanceId) -> FacadeResult<InstanceView> {
        self.registry.set_adopt(&id, false).map_err(internal)?;
        self.runtime.republish_all().await;
        self.view(&id)
    }
    async fn ports(&self, project: Option<ProjectId>) -> FacadeResult<PortsView> {
        let mut entries = std::collections::BTreeMap::new();
        for i in self
            .registry
            .instances()
            .into_iter()
            .filter(|i| project.as_ref().is_none_or(|p| &i.project == p))
        {
            if let Some(port) = i.runtime.port.or_else(|| {
                self.registry
                    .state_snapshot()
                    .port_leases
                    .get(&i.id)
                    .map(|l| l.port)
            }) {
                entries.insert(
                    i.id,
                    PortEntry {
                        port,
                        status: i.runtime.status,
                        health_status: serde_json::to_value(i.runtime.health.state)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_else(|| "unknown".into()),
                        url: i.url,
                    },
                );
            }
        }
        let listeners = self.registry.listeners().1;
        let mut conflicts = Vec::new();
        if let Some(snapshot) = self.discovery.cached() {
            for (id, entry) in &entries {
                let expected = self.runtime.unit_name(id);
                if let Some(holder) = snapshot
                    .listeners
                    .iter()
                    .chain(&snapshot.hidden)
                    .find(|l| l.port == entry.port && l.unit.as_deref() != Some(expected.as_str()))
                {
                    conflicts.push(PortConflict {
                        instance: id.clone(),
                        holder: holder.into(),
                    });
                }
            }
        }
        Ok(PortsView {
            instances: entries,
            unmanaged: listeners.into_iter().filter(|l| !l.managed).collect(),
            conflicts,
        })
    }
    async fn stop_all(&self, request: StopAllRequest) -> FacadeResult<Vec<OperationTicket>> {
        if request.include_services {
            return Err(error("unavailable", "service control is not assembled"));
        }
        let mut ids: Vec<_> = self
            .registry
            .instances()
            .into_iter()
            .filter(|i| {
                !matches!(
                    i.runtime.status,
                    InstanceStatus::Stopped | InstanceStatus::NeedsConfig | InstanceStatus::External
                )
            })
            .map(|i| i.id)
            .collect();
        fn depth(
            app: &AppFacade,
            id: &InstanceId,
            seen: &mut std::collections::HashSet<InstanceId>,
        ) -> usize {
            if !seen.insert(id.clone()) {
                return 0;
            }
            let result = app
                .registry
                .manifest(id)
                .map(|(m, _)| {
                    m.depends_on
                        .iter()
                        .filter_map(|name| app.registry.lookup_project(name))
                        .map(|p| depth(app, &InstanceId::primary(&p), seen) + 1)
                        .max()
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            seen.remove(id);
            result
        }
        ids.sort_by_key(|id| std::cmp::Reverse(depth(self, id, &mut std::collections::HashSet::new())));
        let op = self.tracker.begin(OperationKind::StopAll, None);
        let op_id = op.id().clone();
        let app = self.clone();
        tokio::spawn(async move {
            let token = op.cancel_token();
            let mut failure = None;
            for id in ids {
                if token.is_cancelled() {
                    break;
                }
                match app.enqueue(id, Action::Stop, None) {
                    Ok(ticket) => {
                        let result = tokio::select! { result = app.tracker.wait(&ticket.id, Duration::from_secs(600)) => result, () = token.cancelled() => { app.tracker.cancel(&ticket.id); app.tracker.wait(&ticket.id, Duration::from_secs(600)).await } };
                        if !result
                            .as_ref()
                            .is_some_and(|op| op.status == OperationStatus::Succeeded)
                        {
                            failure = Some(
                                result
                                    .and_then(|op| op.error)
                                    .unwrap_or_else(|| error("operation_failed", "stop failed")),
                            );
                            break;
                        }
                    }
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                }
            }
            let outcome = if token.is_cancelled() {
                OpOutcome::Cancelled
            } else if let Some(e) = failure {
                OpOutcome::Failed(e)
            } else {
                OpOutcome::Succeeded(None)
            };
            op.finish(outcome);
        });
        Ok(vec![ticket(op_id, None)])
    }
    async fn check_health(&self, id: InstanceId) -> FacadeResult<lantern_core::model::HealthSummary> {
        self.runtime.check_health_now(&id).await.map_err(runtime_error)
    }
    async fn import_v1(&self, request: ImportV1Request) -> FacadeResult<Value> {
        let from = request.from.unwrap_or_else(|| self.home.join(".config/lantern"));
        let report = self
            .registry
            .migrate_v1(&from, &self.dirs, request.dry_run, request.force)
            .map_err(internal)?;
        if !request.dry_run {
            self.registry.flush().await.map_err(internal)?;
        }
        serde_json::to_value(report).map_err(internal)
    }
    async fn manifest(&self, id: InstanceId) -> FacadeResult<Value> {
        let (manifest, _) = self
            .registry
            .manifest(&id)
            .ok_or_else(|| error("instance_not_found", format!("instance {id} not found")))?;
        serde_json::to_value(manifest.as_ref()).map_err(internal)
    }
    async fn environment(&self) -> FacadeResult<Value> {
        let report = self.runtime.env_report();
        Ok(json!({"captured_at":report.captured_at, "path":report.path, "warning":report.warning}))
    }
    async fn refresh_environment(&self) -> FacadeResult<Value> {
        let report = self.runtime.refresh_shell_env().await.map_err(runtime_error)?;
        Ok(json!({"captured_at":report.captured_at, "path":report.path, "warning":report.warning}))
    }
    async fn worktree_gc(&self, id: ProjectId, apply: bool, fetch: bool) -> FacadeResult<Value> {
        let project = self
            .registry
            .project(&id)
            .ok_or_else(|| error("project_not_found", format!("project {id} not found")))?;
        if fetch {
            let repo = self.git.repo(
                project
                    .repo
                    .ok_or_else(|| error("bad_request", "project is not a git repository"))?
                    .common_dir,
            );
            let cancel = tokio_util::sync::CancellationToken::new();
            for remote in repo.remotes().await.map_err(internal)? {
                repo.fetch(&remote, Duration::from_secs(60), &cancel)
                    .await
                    .map_err(internal)?;
            }
        }
        let suggestions = self
            .instances_service
            .gc_suggestions(&id, u64::from(self.settings.worktrees.gc_hint_days))
            .await
            .map_err(internal)?;
        let rows: Vec<_> = suggestions.iter().map(|s| json!({"instance":s.instance, "reason":format!("{:?}",s.reason), "action":format!("{:?}",s.action)})).collect();
        let mut removed = Vec::new();
        let mut skipped = Vec::new();
        if apply {
            for suggestion in suggestions {
                if suggestion.action != crate::instances::GcAction::Remove {
                    skipped.push(
                        json!({"instance":suggestion.instance, "reason":"not a removable managed checkout"}),
                    );
                    continue;
                }
                match self.enqueue(suggestion.instance.clone(), Action::Remove, Some(RemoveInstanceParams::default())) {
                    Ok(ticket) => match self.tracker.wait(&ticket.id, Duration::from_secs(600)).await {
                        Some(op) if op.status == OperationStatus::Succeeded => removed.push(suggestion.instance),
                        Some(op) => skipped.push(json!({"instance":suggestion.instance,"reason":op.error.map_or_else(|| "operation failed".into(), |e| e.message)})),
                        None => skipped.push(json!({"instance":suggestion.instance,"reason":"operation disappeared"})),
                    },
                    Err(e) => skipped.push(json!({"instance":suggestion.instance,"reason":e.message})),
                }
            }
        }
        Ok(json!({"suggestions":rows, "removed":removed, "skipped":skipped}))
    }
    async fn schema(&self, name: String) -> FacadeResult<Value> {
        match name.as_str() {
            "manifest" => Ok(lantern_core::manifest::schema::manifest_json_schema()),
            "profile" => Ok(lantern_core::manifest::schema::profile_json_schema()),
            "config" => Ok(lantern_core::manifest::schema::config_json_schema()),
            _ => Err(error("not_found", "unknown schema")),
        }
    }
    async fn patch_settings(&self, patch: Value) -> FacadeResult<SettingsPatched> {
        if !patch.is_object() {
            return Err(error("validation_failed", "settings patch must be an object"));
        }
        let path = self.dirs.config.join("config.toml");
        let mut source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(internal(e)),
        };
        let schema = serde_json::to_value(Settings::default()).map_err(internal)?;
        let mut edits = Vec::new();
        collect_setting_edits("", &patch, &schema, &mut edits)?;
        for (key, value) in &edits {
            if value.is_null() {
                source = remove_setting(&source, key).map_err(internal)?;
            } else {
                source = lantern_core::settings::edit_setting(&source, key, json_to_toml(value)?)
                    .map_err(|e| error("validation_failed", e))?;
            }
        }
        let loaded = lantern_core::settings::load_str(&source, &path, &self.environment, &self.home, None)
            .map_err(|e| error("validation_failed", e.to_string()))?;
        atomic_config_write(&path, source.as_bytes()).map_err(internal)?;
        let restart_required = edits.into_iter().map(|(key, _)| key).collect();
        Ok(SettingsPatched {
            settings: serde_json::to_value(loaded.settings).map_err(internal)?,
            restart_required,
            overridden_by_env: loaded.overridden_by_env,
        })
    }
    async fn reload_settings(&self) -> FacadeResult<Value> {
        let loaded = lantern_core::settings::load(
            &self.dirs.config.join("config.toml"),
            &self.environment,
            &self.home,
            None,
        )
        .map_err(|e| error("validation_failed", e.to_string()))?;
        if loaded.settings != self.settings {
            return Err(error(
                "operation_failed",
                "settings were validated; restart the daemon to apply changes",
            ));
        }
        serde_json::to_value(loaded.settings).map_err(internal)
    }
    async fn settings(&self) -> FacadeResult<Value> {
        serde_json::to_value(&self.settings).map_err(internal)
    }
    async fn reconcile(&self) -> FacadeResult<ReconcileResponse> {
        let report = self.runtime.reconcile().await;
        if !report.errors.is_empty() {
            return Err(internal(report.errors.join("; ")));
        }
        Ok(ReconcileResponse { reconciled: true })
    }
    async fn system_health(&self) -> FacadeResult<SystemHealth> {
        let dirs = self.dirs.clone();
        let home = self.home.clone();
        let settings = self.settings.clone();
        let network_health = self.network_health.clone();
        let isolated = self.isolated_health;
        let search_path = self.environment.get("PATH").cloned().unwrap_or_default();
        let write_error = self.registry.subscribe_write_errors().borrow().clone();
        let runtime = self.system_runtime.clone();
        let cache = self.system_cache.clone();
        tokio::task::spawn_blocking(move || {
            use lantern_system::{
                command::{ProcessRunner, find_program},
                doctor::{self, DoctorCtx, LocalNetworkProbe},
                paths::LanternPaths,
            };
            let paths = LanternPaths::new(dirs, home, runtime, cache, None).map_err(internal)?;
            let runner = ProcessRunner {
                fake_root: paths.fake_root.is_some(),
            };
            let network = LocalNetworkProbe {
                paths: &paths,
                settings: &settings,
                runner: &runner,
            };
            let caddy = super::network::resolve_caddy(&settings.caddy.binary, &paths.dirs.data, &search_path)
                .unwrap_or_else(|| paths.dirs.data.join("bin/caddy"));
            let certutil = find_program(&["/usr/bin/certutil", "/usr/local/bin/certutil"]);
            let mut checks = if isolated {
                Vec::new()
            } else {
                doctor::run(&DoctorCtx {
                    paths: &paths,
                    settings: &settings,
                    runner: &runner,
                    network: &network,
                    caddy: &caddy,
                    certutil: certutil.as_deref(),
                    search_path: std::env::split_paths(&search_path).collect(),
                })
            };
            if let Some(health) = network_health {
                checks.extend(health.checks());
            }
            if let Some(message) = write_error {
                checks.push(Check {
                    id: "state.persistence".into(),
                    title: "State persistence".into(),
                    status: CheckStatus::Warn,
                    detail: message,
                    fix: None,
                });
            }
            let ok = checks
                .iter()
                .all(|c| matches!(c.status, CheckStatus::Ok | CheckStatus::Skipped));
            Ok(SystemHealth {
                ok,
                checked_at: Timestamp::now(),
                checks,
            })
        })
        .await
        .map_err(internal)?
    }
    async fn system_info(&self) -> FacadeResult<SystemInfo> {
        Ok(SystemInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            api_version: API_VERSION,
            pid: std::process::id(),
            boot_id: self.boot_id.clone(),
            started_at: self.started_at,
            listen: format!("127.0.0.1:{}", self.settings.daemon.port),
            tld: self.settings.domains.tld.clone(),
            paths: json!({"config":self.dirs.config, "state":self.dirs.state, "data":self.dirs.data}),
        })
    }
}

fn load_profile(dirs: &Dirs, name: &str) -> FacadeResult<lantern_core::profiles::Profile> {
    if !lantern_core::profiles::valid_name(name) {
        return Err(error("validation_failed", "invalid profile name"));
    }
    lantern_core::profiles::Profiles::load_dir(&dirs.config.join("profiles"))
        .map_err(internal)?
        .0
        .get(name)
        .ok_or_else(|| error("not_found", format!("profile {name} not found")))
}
fn profile_value(name: &str, profile: lantern_core::profiles::Profile, active: bool) -> FacadeResult<Value> {
    let mut value = serde_json::to_value(profile).map_err(internal)?;
    if let Some(map) = value.as_object_mut() {
        map.insert("name".into(), json!(name));
        map.insert("active".into(), json!(active));
    }
    Ok(value)
}
fn atomic_config_write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("configuration path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".lantern-write-{}-{}",
        std::process::id(),
        Timestamp::now().as_nanosecond()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn checkout_root(path: &std::path::Path, subpath: &std::path::Path) -> PathBuf {
    let mut root = path.to_path_buf();
    for _ in subpath.components() {
        root.pop();
    }
    root
}

fn collect_setting_edits(
    prefix: &str,
    patch: &Value,
    schema: &Value,
    out: &mut Vec<(String, Value)>,
) -> FacadeResult<()> {
    if let Some(map) = patch.as_object() {
        for (key, value) in map {
            if key.contains('.') || key.is_empty() {
                return Err(error("validation_failed", "invalid setting key"));
            }
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            let child = schema
                .get(key)
                .cloned()
                .or_else(|| (prefix == "runtime.env").then(|| Value::String(String::new())))
                .ok_or_else(|| error("validation_failed", format!("unknown setting {path}")))?;
            if value.is_object() && child.is_object() {
                collect_setting_edits(&path, value, &child, out)?;
            } else {
                out.push((path, value.clone()));
            }
        }
        Ok(())
    } else {
        Err(error("validation_failed", "settings patch must be an object"))
    }
}
fn json_to_toml(value: &Value) -> FacadeResult<toml_edit::Value> {
    match value {
        Value::Bool(v) => Ok(toml_edit::Value::from(*v)),
        Value::String(v) => Ok(toml_edit::Value::from(v.clone())),
        Value::Number(v) => {
            if let Some(i) = v.as_i64() {
                Ok(toml_edit::Value::from(i))
            } else if let Some(f) = v.as_f64() {
                Ok(toml_edit::Value::from(f))
            } else {
                Err(error("validation_failed", "number outside TOML range"))
            }
        }
        Value::Array(values) => {
            let mut array = toml_edit::Array::new();
            for value in values {
                array.push(json_to_toml(value)?);
            }
            Ok(toml_edit::Value::Array(array))
        }
        _ => Err(error("validation_failed", "unsupported settings value")),
    }
}
fn remove_setting(source: &str, key: &str) -> Result<String, String> {
    let mut doc = source
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| e.to_string())?;
    let mut table = doc.as_table_mut();
    let mut parts = key.split('.').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            table.remove(part);
            break;
        }
        let Some(next) = table.get_mut(part).and_then(toml_edit::Item::as_table_mut) else {
            return Ok(doc.to_string());
        };
        table = next;
    }
    Ok(doc.to_string())
}

fn error_api_conflict(e: &crate::registry::Error) -> ApiError {
    error("already_exists", e.to_string())
}
