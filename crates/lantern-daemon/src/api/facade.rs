//! Consumer-defined seam between HTTP handlers and daemon subsystems.

use async_trait::async_trait;
use lantern_core::{
    api::*,
    ids::{InstanceId, ProjectId},
    model::{HealthSummary, Listener, LogLine},
};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use tokio::sync::broadcast;

/// A facade failure carries the exact stable wire error and its structured details.
pub type FacadeResult<T> = Result<T, ApiError>;

/// An operation already registered in the shared tracker; its worker retains the OpHandle.
#[derive(Debug, Clone)]
pub struct OperationTicket {
    /// The id returned by the worker's OpHandle, including when joining existing work.
    pub id: OperationId,
    /// The best currently available instance view, if any.
    pub instance: Option<InstanceView>,
    /// Whether run-branch created a checkout, when known.
    pub created: Option<bool>,
    /// Whether removal only forgot a user-owned checkout, when known.
    pub forgotten: Option<bool>,
}

/// Project-list query filters.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProjectFilter {
    /// Comma-separated kinds.
    pub kind: Option<String>,
    /// Filter by tag.
    pub tag: Option<String>,
    /// `true`, `false`, or `any`; absent defaults to true.
    pub enabled: Option<String>,
    /// Search text.
    pub q: Option<String>,
}
/// Instance-list query filters.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InstanceFilter {
    /// Project restriction.
    pub project: Option<ProjectId>,
    /// Comma-separated statuses.
    pub status: Option<String>,
    /// Checkout kind.
    pub checkout: Option<String>,
    /// Search text.
    pub q: Option<String>,
}
/// Listener-list query filters.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListenerFilter {
    /// Matched or unmatched only.
    pub matched: Option<bool>,
    /// Project restriction.
    pub project: Option<ProjectId>,
    /// Port restriction.
    pub port: Option<u16>,
    /// Include hidden listeners when requested.
    pub include_hidden: Option<bool>,
}
/// Shared-service action.
#[derive(Debug, Clone, Copy)]
pub enum ServiceAction {
    /// Start a service.
    Start,
    /// Stop a service.
    Stop,
    /// Restart a service.
    Restart,
}

fn unavailable() -> ApiError {
    ApiError {
        code: "unavailable".into(),
        message: "daemon subsystem is unavailable".into(),
        details: Value::Object(Default::default()),
    }
}

/// HTTP's adapter seam. Implementations start work independently of request cancellation.
/// Optional/default methods report unavailable until the controller wires that subsystem.
#[async_trait]
pub trait DaemonFacade: Send + Sync + 'static {
    /// List projects.
    async fn projects(&self, _filter: ProjectFilter) -> FacadeResult<Vec<ProjectSummary>> {
        Err(unavailable())
    }
    /// Read a project.
    async fn project(&self, _id: ProjectId) -> FacadeResult<ProjectDetail> {
        Err(unavailable())
    }
    /// Register a project.
    async fn register_project(&self, _request: RegisterProjectRequest) -> FacadeResult<ProjectSummary> {
        Err(unavailable())
    }
    /// Patch or rename a project.
    async fn patch_project(
        &self,
        _id: ProjectId,
        _request: PatchProjectRequest,
    ) -> FacadeResult<ProjectDetail> {
        Err(unavailable())
    }
    /// Unregister a project.
    async fn unregister_project(&self, _id: ProjectId, _params: UnregisterParams) -> FacadeResult<()> {
        Err(unavailable())
    }
    /// Scan workspaces.
    async fn scan_projects(&self, _request: ScanRequest) -> FacadeResult<ScanReport> {
        Err(unavailable())
    }
    /// Reload a project's manifest.
    async fn reload_project(&self, _id: ProjectId, _request: ReloadRequest) -> FacadeResult<ProjectDetail> {
        Err(unavailable())
    }
    /// Refresh a project's checkouts.
    async fn sync_project(&self, _id: ProjectId) -> FacadeResult<Vec<InstanceView>> {
        Err(unavailable())
    }
    /// List a project's instances.
    async fn project_instances(&self, _id: ProjectId) -> FacadeResult<Vec<InstanceView>> {
        Err(unavailable())
    }
    /// List all project checkouts.
    async fn checkouts(&self, _id: ProjectId) -> FacadeResult<Vec<InstanceView>> {
        Err(unavailable())
    }
    /// List repository branches.
    async fn branches(&self, _id: ProjectId) -> FacadeResult<Vec<BranchInfo>> {
        Err(unavailable())
    }
    /// Read dependency graph.
    async fn dependencies(&self, _project: Option<ProjectId>) -> FacadeResult<DependencyGraph> {
        Err(unavailable())
    }
    /// List instances.
    async fn instances(&self, _filter: InstanceFilter) -> FacadeResult<Vec<InstanceView>> {
        Err(unavailable())
    }
    /// Read an instance.
    async fn instance(&self, _id: InstanceId) -> FacadeResult<InstanceView> {
        Err(unavailable())
    }
    /// Start an instance.
    async fn start(&self, _id: InstanceId) -> FacadeResult<OperationTicket> {
        Err(unavailable())
    }
    /// Stop an instance.
    async fn stop(&self, _id: InstanceId) -> FacadeResult<OperationTicket> {
        Err(unavailable())
    }
    /// Restart an instance.
    async fn restart(&self, _id: InstanceId) -> FacadeResult<OperationTicket> {
        Err(unavailable())
    }
    /// Prepare an instance.
    async fn prepare(&self, _id: InstanceId, _request: PrepareRequest) -> FacadeResult<OperationTicket> {
        Err(unavailable())
    }
    /// Run a branch.
    async fn run_branch(
        &self,
        _id: ProjectId,
        _request: CreateInstanceRequest,
    ) -> FacadeResult<OperationTicket> {
        Err(unavailable())
    }
    /// Remove or forget an instance.
    async fn remove_instance(
        &self,
        _id: InstanceId,
        _params: RemoveInstanceParams,
    ) -> FacadeResult<OperationTicket> {
        Err(unavailable())
    }
    /// Change pin state.
    async fn pin(&self, _id: InstanceId, _pinned: bool) -> FacadeResult<InstanceView> {
        Err(unavailable())
    }
    /// Rename an instance.
    async fn rename_instance(
        &self,
        _id: InstanceId,
        _request: RenameInstanceRequest,
    ) -> FacadeResult<InstanceView> {
        Err(unavailable())
    }
    /// Inspect removal blockers.
    async fn removal_check(&self, _id: InstanceId) -> FacadeResult<RemovalCheck> {
        Err(unavailable())
    }
    /// Adopt a discovered listener.
    async fn adopt(&self, _id: InstanceId, _request: AdoptRequest) -> FacadeResult<InstanceView> {
        Err(unavailable())
    }
    /// Remove adoption.
    async fn unadopt(&self, _id: InstanceId) -> FacadeResult<InstanceView> {
        Err(unavailable())
    }
    /// Read filtered historical logs.
    async fn logs(&self, _id: InstanceId, _params: LogParams) -> FacadeResult<LogPage> {
        Err(unavailable())
    }
    /// Atomically subscribe and return backlog with no gap or duplication.
    async fn subscribe_logs(
        &self,
        _id: InstanceId,
        _tail: usize,
    ) -> FacadeResult<(Vec<LogLine>, broadcast::Receiver<LogLine>)> {
        Err(unavailable())
    }
    /// Read a complete state-event snapshot after the caller subscribes to the bus.
    async fn event_snapshot(&self) -> FacadeResult<Vec<Event>> {
        Err(unavailable())
    }
    /// List cached listeners.
    async fn listeners(&self, _filter: ListenerFilter) -> FacadeResult<Vec<Listener>> {
        Err(unavailable())
    }
    /// Scan listeners immediately.
    async fn scan_listeners(&self) -> FacadeResult<Vec<Listener>> {
        Err(unavailable())
    }
    /// Read port allocations and conflicts.
    async fn ports(&self, _project: Option<ProjectId>) -> FacadeResult<PortsView> {
        Err(unavailable())
    }
    /// List shared services.
    async fn services(&self) -> FacadeResult<Vec<ServiceStatus>> {
        Err(unavailable())
    }
    /// Read a shared service.
    async fn service(&self, _name: String) -> FacadeResult<ServiceStatus> {
        Err(unavailable())
    }
    /// Control a shared service.
    async fn service_action(&self, _name: String, _action: ServiceAction) -> FacadeResult<ServiceStatus> {
        Err(unavailable())
    }
    /// List profiles.
    async fn profiles(&self) -> FacadeResult<Vec<Value>> {
        Err(unavailable())
    }
    /// Read a profile.
    async fn profile(&self, _name: String) -> FacadeResult<Value> {
        Err(unavailable())
    }
    /// Create or replace a profile; boolean indicates creation.
    async fn put_profile(&self, _name: String, _value: Value) -> FacadeResult<(Value, bool)> {
        Err(unavailable())
    }
    /// Delete a profile.
    async fn delete_profile(&self, _name: String) -> FacadeResult<()> {
        Err(unavailable())
    }
    /// Activate a profile, starting tracked operations.
    async fn activate_profile(&self, _name: String) -> FacadeResult<Vec<OperationTicket>> {
        Err(unavailable())
    }
    /// List templates.
    async fn templates(&self) -> FacadeResult<Vec<Value>> {
        Err(unavailable())
    }
    /// Read a template.
    async fn template(&self, _name: String) -> FacadeResult<Value> {
        Err(unavailable())
    }
    /// Create a template.
    async fn create_template(&self, _value: Value) -> FacadeResult<Value> {
        Err(unavailable())
    }
    /// Replace a template; boolean indicates creation.
    async fn put_template(&self, _name: String, _value: Value) -> FacadeResult<(Value, bool)> {
        Err(unavailable())
    }
    /// Delete a template.
    async fn delete_template(&self, _name: String) -> FacadeResult<()> {
        Err(unavailable())
    }
    /// List workspace suggestions.
    async fn suggestions(&self) -> FacadeResult<Vec<Suggestion>> {
        Err(unavailable())
    }
    /// Dismiss a workspace suggestion.
    async fn dismiss_suggestion(&self, _path: PathBuf) -> FacadeResult<()> {
        Err(unavailable())
    }
    /// Import v1 settings and projects.
    async fn import_v1(&self, _request: ImportV1Request) -> FacadeResult<Value> {
        Err(unavailable())
    }
    /// Get a named schema.
    async fn schema(&self, _name: String) -> FacadeResult<Value> {
        Err(unavailable())
    }
    /// Read settings.
    async fn settings(&self) -> FacadeResult<Value> {
        Err(unavailable())
    }
    /// Apply a settings merge patch.
    async fn patch_settings(&self, _patch: Value) -> FacadeResult<SettingsPatched> {
        Err(unavailable())
    }
    /// Reload settings.
    async fn reload_settings(&self) -> FacadeResult<Value> {
        Err(unavailable())
    }
    /// Read daemon system information.
    async fn system_info(&self) -> FacadeResult<SystemInfo> {
        Err(unavailable())
    }
    /// Read system health.
    async fn system_health(&self) -> FacadeResult<SystemHealth> {
        Err(unavailable())
    }
    /// Reconcile daemon state.
    async fn reconcile(&self) -> FacadeResult<ReconcileResponse> {
        Err(unavailable())
    }
    /// Stop running instances and optionally services.
    async fn stop_all(&self, _request: StopAllRequest) -> FacadeResult<Vec<OperationTicket>> {
        Err(unavailable())
    }
    /// Run a health check now.
    async fn check_health(&self, _id: InstanceId) -> FacadeResult<HealthSummary> {
        Err(unavailable())
    }
    /// Read a full instance manifest.
    async fn manifest(&self, _id: InstanceId) -> FacadeResult<Value> {
        Err(unavailable())
    }
    /// Read an intel snapshot.
    async fn intel(&self, _id: InstanceId, _max_age: Option<u32>) -> FacadeResult<IntelSnapshot> {
        Err(unavailable())
    }
    /// Refresh one instance's intel.
    async fn refresh_intel(&self, _id: InstanceId) -> FacadeResult<IntelSnapshot> {
        Err(unavailable())
    }
    /// Refresh all intel.
    async fn refresh_all_intel(&self) -> FacadeResult<IntelRefreshReport> {
        Err(unavailable())
    }
    /// List documents.
    async fn docs(&self, _id: InstanceId) -> FacadeResult<Vec<DocEntry>> {
        Err(unavailable())
    }
    /// Read a document page.
    async fn doc(
        &self,
        _id: InstanceId,
        _path: String,
        _offset: Option<u64>,
        _max_chars: Option<u64>,
    ) -> FacadeResult<DocContent> {
        Err(unavailable())
    }
    /// List endpoints.
    async fn endpoints(
        &self,
        _id: InstanceId,
        _q: Option<String>,
        _limit: Option<u32>,
    ) -> FacadeResult<Vec<EndpointEntry>> {
        Err(unavailable())
    }
}
