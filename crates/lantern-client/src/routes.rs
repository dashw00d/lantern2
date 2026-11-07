//! Typed REST route methods.

use crate::{Client, ClientError, REQUEST_TIMEOUT, Wait, append_query};
use lantern_core::{
    api::*,
    ids::{InstanceId, ProjectId},
    model::{HealthSummary, Listener},
};
use reqwest::{Method, RequestBuilder};
use serde::Serialize;
use serde_json::Value;
use std::{path::PathBuf, time::Duration};

/// Optional project-list filters; absent fields use daemon defaults.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProjectFilter {
    /// Comma-separated project kinds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Match a tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// `true`, `false`, or `any`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<String>,
    /// Search text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
}
/// Optional instance-list filters.
#[derive(Debug, Clone, Default, Serialize)]
pub struct InstanceFilter {
    /// Match a project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectId>,
    /// Comma-separated statuses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Checkout kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkout: Option<String>,
    /// Search text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
}
/// Listener filters, including optional hidden results.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ListenerFilter {
    /// Restrict to matched or unmatched listeners.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched: Option<bool>,
    /// Match a project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectId>,
    /// Match a port.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Include hidden listeners.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_hidden: Option<bool>,
}
/// Optional operation-list filters.
#[derive(Debug, Clone, Default, Serialize)]
pub struct OperationFilter {
    /// Match an instance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<InstanceId>,
    /// Match an operation status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<OperationStatus>,
    /// Maximum returned operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}
/// Options for creating or reusing an instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions {
    /// Fetch remote branches before resolving the target.
    pub fetch: bool,
    /// Start the instance after preparing it.
    pub start: bool,
    /// Explicit pin choice, or use the daemon's default.
    pub pin: Option<bool>,
}
impl Default for RunOptions {
    fn default() -> Self {
        Self {
            fetch: true,
            start: true,
            pin: None,
        }
    }
}
/// Action to perform on a shared service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    /// Start the service.
    Start,
    /// Stop the service.
    Stop,
    /// Restart the service.
    Restart,
}
impl ServiceAction {
    fn path(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}

impl Client {
    async fn list<T: serde::de::DeserializeOwned>(&self, req: RequestBuilder) -> Result<Vec<T>, ClientError> {
        let result: List<T> = self.json(req, REQUEST_TIMEOUT).await?;
        Ok(result.items)
    }
    async fn operation_response(
        &self,
        req: RequestBuilder,
        wait: Wait,
    ) -> Result<OperationResponse, ClientError> {
        let (req, deadline) = wait.apply(req)?;
        self.json(req, deadline).await
    }
    async fn operations_response(
        &self,
        req: RequestBuilder,
        wait: Wait,
    ) -> Result<Vec<Operation>, ClientError> {
        let (req, deadline) = wait.apply(req)?;
        let result: OperationsResponse = self.json(req, deadline).await?;
        Ok(result.operations)
    }
    /// Read daemon runtime information.
    pub async fn system(&self) -> Result<SystemInfo, ClientError> {
        self.json(self.request(Method::GET, &["api", "system"]), REQUEST_TIMEOUT)
            .await
    }
    /// Read the current doctor checks.
    pub async fn health(&self) -> Result<SystemHealth, ClientError> {
        self.json(
            self.request(Method::GET, &["api", "system", "health"]),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Reconcile the registry and routing state.
    pub async fn reconcile(&self) -> Result<ReconcileResponse, ClientError> {
        self.json(
            self.request(Method::POST, &["api", "system", "reconcile"]),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Stop all instances, optionally including shared services.
    pub async fn stop_all_with_options(
        &self,
        options: StopAllRequest,
        wait: Wait,
    ) -> Result<Vec<Operation>, ClientError> {
        self.operations_response(
            self.request(Method::POST, &["api", "system", "stop-all"])
                .json(&options),
            wait,
        )
        .await
    }
    /// Stop all instances while leaving shared services running.
    pub async fn stop_all(&self, wait: Wait) -> Result<Vec<Operation>, ClientError> {
        self.stop_all_with_options(
            StopAllRequest {
                include_services: false,
            },
            wait,
        )
        .await
    }
    /// Read settings; config lane types will replace this JSON value at integration.
    pub async fn settings(&self) -> Result<Value, ClientError> {
        self.json(self.request(Method::GET, &["api", "settings"]), REQUEST_TIMEOUT)
            .await
    }
    /// Apply a JSON merge patch to settings.
    pub async fn patch_settings(&self, patch: Value) -> Result<SettingsPatched, ClientError> {
        self.json(
            self.request(Method::PATCH, &["api", "settings"]).json(&patch),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Reload settings from disk.
    pub async fn reload_settings(&self) -> Result<Value, ClientError> {
        self.json(
            self.request(Method::POST, &["api", "settings", "reload"]),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// List projects using optional filters.
    pub async fn projects(&self, filter: ProjectFilter) -> Result<Vec<ProjectSummary>, ClientError> {
        self.list(append_query(
            self.request(Method::GET, &["api", "projects"]),
            &filter,
        ))
        .await
    }
    /// Register a project.
    pub async fn register_project(&self, req: RegisterProjectRequest) -> Result<ProjectSummary, ClientError> {
        self.json(
            self.request(Method::POST, &["api", "projects"]).json(&req),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Scan workspace roots; none selects the configured roots.
    pub async fn scan(&self, roots: Option<Vec<PathBuf>>) -> Result<ScanReport, ClientError> {
        self.json(
            self.request(Method::POST, &["api", "projects", "scan"])
                .json(&ScanRequest { roots }),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Fetch a project and its instances.
    pub async fn project(&self, id: &ProjectId) -> Result<ProjectDetail, ClientError> {
        self.json(
            self.request(Method::GET, &["api", "projects", id.as_str()]),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Patch a project, including its optional id rename.
    pub async fn patch_project(
        &self,
        id: &ProjectId,
        req: PatchProjectRequest,
    ) -> Result<ProjectDetail, ClientError> {
        self.json(
            self.request(Method::PATCH, &["api", "projects", id.as_str()])
                .json(&req),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Unregister a project using the specified removal options.
    pub async fn unregister_project(
        &self,
        id: &ProjectId,
        params: UnregisterParams,
    ) -> Result<(), ClientError> {
        self.empty(append_query(
            self.request(Method::DELETE, &["api", "projects", id.as_str()]),
            &params,
        ))
        .await
    }
    /// Reload a project's manifest.
    pub async fn reload_project(
        &self,
        id: &ProjectId,
        req: ReloadRequest,
    ) -> Result<ProjectDetail, ClientError> {
        self.json(
            self.request(Method::POST, &["api", "projects", id.as_str(), "reload"])
                .json(&req),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Refresh project worktrees immediately.
    pub async fn sync_project(&self, id: &ProjectId) -> Result<Vec<InstanceView>, ClientError> {
        self.list(self.request(Method::POST, &["api", "projects", id.as_str(), "sync"]))
            .await
    }
    /// List a project's known instances.
    pub async fn project_instances(&self, id: &ProjectId) -> Result<Vec<InstanceView>, ClientError> {
        self.list(self.request(Method::GET, &["api", "projects", id.as_str(), "instances"]))
            .await
    }
    /// List all project checkouts, including unpinned ones.
    pub async fn checkouts(&self, id: &ProjectId) -> Result<Vec<InstanceView>, ClientError> {
        self.list(self.request(Method::GET, &["api", "projects", id.as_str(), "checkouts"]))
            .await
    }
    /// List project branches.
    pub async fn branches(&self, id: &ProjectId) -> Result<Vec<BranchInfo>, ClientError> {
        self.list(self.request(Method::GET, &["api", "projects", id.as_str(), "branches"]))
            .await
    }
    /// Create or reuse a branch instance with explicit options and wait behavior.
    pub async fn run_branch(
        &self,
        id: &ProjectId,
        target: RunTarget,
        options: RunOptions,
        wait: Wait,
    ) -> Result<OperationResponse, ClientError> {
        let req = CreateInstanceRequest {
            target,
            fetch: options.fetch,
            start: options.start,
            pin: options.pin,
        };
        self.operation_response(
            self.request(Method::POST, &["api", "projects", id.as_str(), "instances"])
                .json(&req),
            wait,
        )
        .await
    }
    /// Fetch the dependency graph, optionally scoped to a project.
    pub async fn dependencies(&self, project: Option<&ProjectId>) -> Result<DependencyGraph, ClientError> {
        let mut req = self.request(Method::GET, &["api", "dependencies"]);
        if let Some(id) = project {
            req = req.query(&[("project", id.as_str())]);
        }
        self.json(req, REQUEST_TIMEOUT).await
    }
    /// List instances using optional filters.
    pub async fn instances(&self, filter: InstanceFilter) -> Result<Vec<InstanceView>, ClientError> {
        self.list(append_query(
            self.request(Method::GET, &["api", "instances"]),
            &filter,
        ))
        .await
    }
    /// Read a single instance.
    pub async fn instance(&self, id: &InstanceId) -> Result<InstanceView, ClientError> {
        self.json(
            self.request(Method::GET, &["api", "instances", id.as_str()]),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Patch an instance's pin state.
    pub async fn patch_instance(
        &self,
        id: &InstanceId,
        req: PatchInstanceRequest,
    ) -> Result<InstanceView, ClientError> {
        self.json(
            self.request(Method::PATCH, &["api", "instances", id.as_str()])
                .json(&req),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Pin or unpin an instance via its dedicated route.
    pub async fn pin(&self, id: &InstanceId, pinned: bool) -> Result<InstanceView, ClientError> {
        let action = if pinned { "pin" } else { "unpin" };
        self.json(
            self.request(Method::POST, &["api", "instances", id.as_str(), action]),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Relabel a stopped instance.
    pub async fn rename_instance(&self, id: &InstanceId, slug: String) -> Result<InstanceView, ClientError> {
        self.json(
            self.request(Method::POST, &["api", "instances", id.as_str(), "rename"])
                .json(&RenameInstanceRequest { slug }),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Inspect blockers before removing or forgetting an instance.
    pub async fn removal_check(&self, id: &InstanceId) -> Result<RemovalCheck, ClientError> {
        self.json(
            self.request(Method::GET, &["api", "instances", id.as_str(), "removal-check"]),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Remove a managed instance, or forget a user-created checkout.
    pub async fn remove_instance(
        &self,
        id: &InstanceId,
        params: RemoveInstanceParams,
        wait: Wait,
    ) -> Result<OperationResponse, ClientError> {
        self.operation_response(
            append_query(
                self.request(Method::DELETE, &["api", "instances", id.as_str()]),
                &params,
            ),
            wait,
        )
        .await
    }
    async fn instance_action(
        &self,
        id: &InstanceId,
        action: &str,
        wait: Wait,
    ) -> Result<OperationResponse, ClientError> {
        self.operation_response(
            self.request(Method::POST, &["api", "instances", id.as_str(), action])
                .json(&serde_json::json!({})),
            wait,
        )
        .await
    }
    /// Start an instance.
    pub async fn start(&self, id: &InstanceId, wait: Wait) -> Result<OperationResponse, ClientError> {
        self.instance_action(id, "start", wait).await
    }
    /// Stop an instance.
    pub async fn stop(&self, id: &InstanceId, wait: Wait) -> Result<OperationResponse, ClientError> {
        self.instance_action(id, "stop", wait).await
    }
    /// Restart an instance.
    pub async fn restart(&self, id: &InstanceId, wait: Wait) -> Result<OperationResponse, ClientError> {
        self.instance_action(id, "restart", wait).await
    }
    /// Prepare an instance, optionally repeating completed setup.
    pub async fn prepare(
        &self,
        id: &InstanceId,
        force: bool,
        wait: Wait,
    ) -> Result<OperationResponse, ClientError> {
        self.operation_response(
            self.request(Method::POST, &["api", "instances", id.as_str(), "prepare"])
                .json(&PrepareRequest { force }),
            wait,
        )
        .await
    }
    /// CLI-friendly alias for prepare.
    pub async fn setup(
        &self,
        id: &InstanceId,
        force: bool,
        wait: Wait,
    ) -> Result<OperationResponse, ClientError> {
        self.prepare(id, force, wait).await
    }
    /// Adopt an external listener, optionally selecting its port.
    pub async fn adopt(&self, id: &InstanceId, port: Option<u16>) -> Result<InstanceView, ClientError> {
        self.json(
            self.request(Method::POST, &["api", "instances", id.as_str(), "adopt"])
                .json(&AdoptRequest { port }),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Stop routing an adopted external listener.
    pub async fn unadopt(&self, id: &InstanceId) -> Result<InstanceView, ClientError> {
        self.json(
            self.request(Method::POST, &["api", "instances", id.as_str(), "unadopt"]),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Read a JSON log snapshot.
    pub async fn logs(&self, id: &InstanceId, params: LogParams) -> Result<LogPage, ClientError> {
        if params.format == Some(LogFormat::Text) {
            return Err(ClientError::Protocol("use logs_text for text log format".into()));
        }
        self.json(
            append_query(
                self.request(Method::GET, &["api", "instances", id.as_str(), "logs"]),
                &params,
            ),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Read a text log snapshot.
    pub async fn logs_text(&self, id: &InstanceId, mut params: LogParams) -> Result<String, ClientError> {
        params.format = Some(LogFormat::Text);
        self.response(
            append_query(
                self.request(Method::GET, &["api", "instances", id.as_str(), "logs"]),
                &params,
            ),
            Some(REQUEST_TIMEOUT),
        )
        .await?
        .text()
        .await
        .map_err(ClientError::from_reqwest)
    }
    /// Trigger an immediate instance health check.
    pub async fn check_health(&self, id: &InstanceId) -> Result<HealthSummary, ClientError> {
        self.json(
            self.request(
                Method::POST,
                &["api", "instances", id.as_str(), "health", "check"],
            ),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Read the unredacted resolved manifest as JSON pending the config lane's final report type.
    pub async fn manifest(&self, id: &InstanceId) -> Result<Value, ClientError> {
        self.json(
            self.request(Method::GET, &["api", "instances", id.as_str(), "manifest"]),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Read an instance's intel snapshot.
    pub async fn intel(&self, id: &InstanceId, max_age: Option<u32>) -> Result<IntelSnapshot, ClientError> {
        let mut req = self.request(Method::GET, &["api", "instances", id.as_str(), "intel"]);
        if let Some(n) = max_age {
            req = req.query(&[("max_age", n)]);
        }
        self.json(req, REQUEST_TIMEOUT).await
    }
    /// Refresh one instance's intel.
    pub async fn refresh_intel(&self, id: &InstanceId) -> Result<IntelSnapshot, ClientError> {
        self.json(
            self.request(
                Method::POST,
                &["api", "instances", id.as_str(), "intel", "refresh"],
            ),
            Duration::from_secs(35),
        )
        .await
    }
    /// Refresh intel across instances.
    pub async fn refresh_all_intel(&self) -> Result<IntelRefreshReport, ClientError> {
        self.json(
            self.request(Method::POST, &["api", "intel", "refresh"]),
            Duration::from_secs(35),
        )
        .await
    }
    /// List indexed documents.
    pub async fn docs(&self, id: &InstanceId) -> Result<Vec<DocEntry>, ClientError> {
        self.list(self.request(Method::GET, &["api", "instances", id.as_str(), "docs"]))
            .await
    }
    /// Read a document as a paged JSON response.
    pub async fn doc(
        &self,
        id: &InstanceId,
        path: &str,
        offset: Option<u64>,
        max_chars: Option<u64>,
    ) -> Result<DocContent, ClientError> {
        let mut segments = vec!["api", "instances", id.as_str(), "docs"];
        if path.is_empty() || path.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
            return Err(ClientError::Protocol("invalid document path".into()));
        }
        segments.extend(path.split('/'));
        let mut req = self.request(Method::GET, &segments).query(&[("format", "json")]);
        if let Some(n) = offset {
            req = req.query(&[("offset", n)]);
        }
        if let Some(n) = max_chars {
            req = req.query(&[("max_chars", n)]);
        }
        self.json(req, REQUEST_TIMEOUT).await
    }
    /// List indexed endpoints, optionally filtering and limiting results.
    pub async fn endpoints(
        &self,
        id: &InstanceId,
        query: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<EndpointEntry>, ClientError> {
        let mut req = self.request(Method::GET, &["api", "instances", id.as_str(), "endpoints"]);
        if let Some(q) = query {
            req = req.query(&[("q", q)]);
        }
        if let Some(n) = limit {
            req = req.query(&[("limit", n)]);
        }
        self.list(req).await
    }
    /// List retained operations.
    pub async fn operations(&self, filter: OperationFilter) -> Result<Vec<Operation>, ClientError> {
        self.list(append_query(
            self.request(Method::GET, &["api", "operations"]),
            &filter,
        ))
        .await
    }
    /// Fetch an operation, optionally waiting for it to finish.
    pub async fn operation(
        &self,
        id: &OperationId,
        wait: Option<Duration>,
    ) -> Result<Operation, ClientError> {
        validate_segment(&id.0)?;
        let w = Wait {
            wait: wait.is_some(),
            timeout: wait,
        };
        let (req, deadline) = w.apply(self.request(Method::GET, &["api", "operations", &id.0]))?;
        self.json(req, deadline).await
    }
    /// Request best-effort cancellation of an operation.
    pub async fn cancel_operation(&self, id: &OperationId) -> Result<Operation, ClientError> {
        validate_segment(&id.0)?;
        self.json(
            self.request(Method::POST, &["api", "operations", &id.0, "cancel"]),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// List the current visible listener snapshot.
    pub async fn listeners(&self) -> Result<Vec<Listener>, ClientError> {
        self.listeners_filtered(ListenerFilter::default()).await
    }
    /// List listeners with explicit filters.
    pub async fn listeners_filtered(&self, filter: ListenerFilter) -> Result<Vec<Listener>, ClientError> {
        self.list(append_query(
            self.request(Method::GET, &["api", "listeners"]),
            &filter,
        ))
        .await
    }
    /// Scan for listeners immediately.
    pub async fn scan_listeners(&self) -> Result<Vec<Listener>, ClientError> {
        self.list(self.request(Method::POST, &["api", "listeners", "scan"]))
            .await
    }
    /// Read port assignments, unmanaged listeners, and conflicts.
    pub async fn ports(&self, project: Option<&ProjectId>) -> Result<PortsView, ClientError> {
        let mut req = self.request(Method::GET, &["api", "ports"]);
        if let Some(id) = project {
            req = req.query(&[("project", id.as_str())]);
        }
        self.json(req, REQUEST_TIMEOUT).await
    }
    /// List shared services.
    pub async fn services(&self) -> Result<Vec<ServiceStatus>, ClientError> {
        self.list(self.request(Method::GET, &["api", "services"])).await
    }
    /// Read one shared service.
    pub async fn service_status(&self, name: &str) -> Result<ServiceStatus, ClientError> {
        validate_segment(name)?;
        self.json(
            self.request(Method::GET, &["api", "services", name]),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Start, stop, or restart a shared service.
    pub async fn service(&self, name: &str, action: ServiceAction) -> Result<ServiceStatus, ClientError> {
        validate_segment(name)?;
        self.json(
            self.request(Method::POST, &["api", "services", name, action.path()]),
            Duration::from_secs(35),
        )
        .await
    }
    /// List profiles as config-owned JSON values.
    pub async fn profiles(&self) -> Result<Vec<Value>, ClientError> {
        self.list(self.request(Method::GET, &["api", "profiles"])).await
    }
    /// Read one profile.
    pub async fn profile(&self, name: &str) -> Result<Value, ClientError> {
        validate_segment(name)?;
        self.json(
            self.request(Method::GET, &["api", "profiles", name]),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Create or update a profile.
    pub async fn put_profile(&self, name: &str, profile: Value) -> Result<Value, ClientError> {
        validate_segment(name)?;
        self.json(
            self.request(Method::PUT, &["api", "profiles", name])
                .json(&profile),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Delete a profile.
    pub async fn delete_profile(&self, name: &str) -> Result<(), ClientError> {
        validate_segment(name)?;
        self.empty(self.request(Method::DELETE, &["api", "profiles", name]))
            .await
    }
    /// Activate a profile.
    pub async fn activate_profile(&self, name: &str, wait: Wait) -> Result<Vec<Operation>, ClientError> {
        validate_segment(name)?;
        self.operations_response(
            self.request(Method::POST, &["api", "profiles", name, "activate"]),
            wait,
        )
        .await
    }
    /// List templates as config-owned JSON values.
    pub async fn templates(&self) -> Result<Vec<Value>, ClientError> {
        self.list(self.request(Method::GET, &["api", "templates"])).await
    }
    /// Read one template.
    pub async fn template(&self, name: &str) -> Result<Value, ClientError> {
        validate_segment(name)?;
        self.json(
            self.request(Method::GET, &["api", "templates", name]),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Create a template.
    pub async fn create_template(&self, template: Value) -> Result<Value, ClientError> {
        self.json(
            self.request(Method::POST, &["api", "templates"]).json(&template),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Update a template.
    pub async fn put_template(&self, name: &str, template: Value) -> Result<Value, ClientError> {
        validate_segment(name)?;
        self.json(
            self.request(Method::PUT, &["api", "templates", name])
                .json(&template),
            REQUEST_TIMEOUT,
        )
        .await
    }
    /// Delete a template.
    pub async fn delete_template(&self, name: &str) -> Result<(), ClientError> {
        validate_segment(name)?;
        self.empty(self.request(Method::DELETE, &["api", "templates", name]))
            .await
    }
    /// List detected project suggestions.
    pub async fn suggestions(&self) -> Result<Vec<Suggestion>, ClientError> {
        self.list(self.request(Method::GET, &["api", "suggestions"]))
            .await
    }
    /// Dismiss a detected project suggestion.
    pub async fn dismiss_suggestion(&self, path: PathBuf) -> Result<(), ClientError> {
        self.empty(
            self.request(Method::POST, &["api", "suggestions", "dismiss"])
                .json(&DismissSuggestionRequest { path }),
        )
        .await
    }
    /// Import v1 state and configuration; the report shape is supplied by config.
    pub async fn import_v1(&self, req: ImportV1Request) -> Result<Value, ClientError> {
        self.json(
            self.request(Method::POST, &["api", "import", "v1"]).json(&req),
            Duration::from_secs(35),
        )
        .await
    }
    /// Read a named JSON schema.
    pub async fn schema(&self, name: &str) -> Result<Value, ClientError> {
        validate_segment(name)?;
        self.json(
            self.request(Method::GET, &["api", "schema", name]),
            REQUEST_TIMEOUT,
        )
        .await
    }
}

fn validate_segment(name: &str) -> Result<(), ClientError> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(ClientError::Protocol(
            "empty or dot path segment is not valid".into(),
        ));
    }
    Ok(())
}
