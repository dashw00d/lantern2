//! Shared REST and SSE wire contracts. This module has no runtime or network dependencies.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::{
    Timestamp,
    ids::{InstanceId, ProjectId},
    model::{
        Checkout, InstanceStatus, Listener, LogLine, Project, ProjectKind, RuntimeState, ServeMode, Stream,
    },
};

/// Version of the REST and SSE protocol supported by this build.
pub const API_VERSION: u32 = 1;

macro_rules! wire_struct {
    ($(#[$attr:meta])* $name:ident { $($(#[$field_attr:meta])* $field:ident: $ty:ty),* $(,)? }) => {
        $(#[$attr])*
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        pub struct $name {
            $(#[doc = concat!("Wire field `", stringify!($field), "`.")]
              $(#[$field_attr])* pub $field: $ty,)*
        }
    };
}
macro_rules! wire_enum {
    ($(#[$attr:meta])* $name:ident { $($variant:ident),* $(,)? }) => {
        $(#[$attr])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $(#[doc = concat!("Wire value `", stringify!($variant), "`.")] $variant,)* }
    };
}

/// A list response; REST lists are wrapped in an `items` object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct List<T> {
    /// The items in the response.
    pub items: Vec<T>,
}

/// Stable error code strings; unknown codes remain representable in [`ApiError`].
pub mod error_code {
    macro_rules! codes { ($($name:ident = $value:literal),* $(,)?) => { $(#[doc = concat!("Error code `", $value, "`.")] pub const $name: &str = $value;)* }; }
    codes! {
        BAD_REQUEST = "bad_request", INVALID_JSON = "invalid_json", VALIDATION_FAILED = "validation_failed",
        NOT_FOUND = "not_found", PROJECT_NOT_FOUND = "project_not_found", INSTANCE_NOT_FOUND = "instance_not_found",
        SERVICE_NOT_FOUND = "service_not_found", OPERATION_NOT_FOUND = "operation_not_found", DOC_NOT_FOUND = "doc_not_found",
        ALREADY_EXISTS = "already_exists", OPERATION_IN_PROGRESS = "operation_in_progress", CANNOT_REMOVE_PRIMARY = "cannot_remove_primary",
        DIRTY_WORKTREE = "dirty_worktree", PORT_UNAVAILABLE = "port_unavailable", BUILTIN_TEMPLATE = "builtin_template",
        RESERVED_ID = "reserved_id", NEEDS_CONFIG = "needs_config", BRANCH_INVALID = "branch_invalid",
        FORBIDDEN_HOST = "forbidden_host", FORBIDDEN_ORIGIN = "forbidden_origin", UNSUPPORTED_MEDIA_TYPE = "unsupported_media_type",
        PAYLOAD_TOO_LARGE = "payload_too_large", METHOD_NOT_ALLOWED = "method_not_allowed", TIMEOUT = "timeout",
        UNAVAILABLE = "unavailable", OPERATION_FAILED = "operation_failed", INTERNAL = "internal",
        NOT_CONTROLLABLE = "not_controllable"
    }
}
wire_struct! { /// Standard failure envelope.
    ErrorEnvelope { error: ApiError }
}
wire_struct! { /// A stable code, human-readable explanation, and optional structured details.
    ApiError { code: String, message: String, #[serde(default)] details: Value }
}

wire_struct! { /// Diagnostic totals grouped by severity.
    DiagnosticCounts { errors: u32, warnings: u32, info: u32 }
}
wire_struct! { /// Safe, compact manifest information published on the event bus.
    ManifestSummary { id: ProjectId, serve: ServeMode, run_cmd: Option<String>, #[serde(default)] open_path: Option<String>, #[serde(default)] description: Option<String>, diagnostics: DiagnosticCounts, manifest_rev: String }
}
wire_enum! { /// Risk attached to an endpoint or routing policy.
    Risk { Low, Medium, High, Critical, Unknown }
}
wire_struct! { /// Routing information safe to expose in project summaries.
    RoutingMeta { triggers: Vec<String>, risk: Option<Risk>, requires_confirmation: bool, max_concurrent: Option<u32>, agents: Vec<String> }
}
/// Summary of a project and its primary instance, flattened on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectSummary {
    /// The project domain record.
    #[serde(flatten)]
    pub project: Project,
    /// Id of the primary instance.
    pub primary: InstanceId,
    /// Status of the primary instance.
    pub status: InstanceStatus,
    /// URL of the primary instance.
    pub url: String,
    /// Port of the primary instance, when assigned.
    pub port: Option<u16>,
    /// Number of known instances.
    pub instance_count: u32,
    /// Number of known documents.
    pub docs_count: u32,
    /// Number of known endpoints.
    pub endpoints_count: u32,
    /// Routing metadata, when available.
    pub routing: Option<RoutingMeta>,
}
/// Project detail with the summary flattened on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectDetail {
    /// Project summary.
    #[serde(flatten)]
    pub summary: ProjectSummary,
    /// All current instances.
    pub instances: Vec<InstanceView>,
    /// Direct project dependencies.
    pub depends_on: Vec<ProjectId>,
    /// Compact manifest.
    pub manifest: ManifestSummary,
}
wire_struct! { /// A compact project returned to MCP consumers.
    ProjectBrief { name: String, id: ProjectId, path: PathBuf, domain: String, url: String, kind: ProjectKind,
        status: InstanceStatus, port: Option<u16>, description: Option<String>, tags: Vec<String>, enabled: bool,
        instances: u32, branches: Vec<String>, docs_count: u32, endpoints_count: u32 }
}
wire_struct! { /// An instance without the full, potentially sensitive manifest.
    InstanceView { id: InstanceId, project: ProjectId, checkout: Checkout, domain: String, url: String,
        pinned: bool, adopt: bool, visible: bool, setup_stale: bool, runtime: RuntimeState, manifest: ManifestSummary }
}
wire_struct! { /// A detected project proposal; config owns the richer internal detection data.
    Suggestion { path: PathBuf, proposed_id: ProjectId, framework: String, confidence: Confidence }
}
wire_enum! { /// Confidence in a detected project.
    Confidence { High, Medium, Low }
}

// Config-owned structures are represented as JSON until the integration supplies concrete types.
wire_struct! { /// Register a project and optionally write its suggested manifest.
    RegisterProjectRequest { path: PathBuf, id: Option<ProjectId>, name: Option<String>, enabled: Option<bool>, overrides: Option<Value>,
        #[serde(default)] write_manifest: bool }
}
wire_struct! { /// Change project metadata, overrides, or its id.
    PatchProjectRequest { id: Option<ProjectId>, name: Option<String>, enabled: Option<bool>, tags: Option<Vec<String>>, overrides: Option<Value> }
}
/// Options for unregistering a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnregisterParams {
    /// Stop its instances; defaults to true.
    #[serde(default = "default_true")]
    pub stop: bool,
    /// Also remove managed worktrees; defaults to false.
    #[serde(default)]
    pub remove_worktrees: bool,
}
impl Default for UnregisterParams {
    fn default() -> Self {
        Self {
            stop: true,
            remove_worktrees: false,
        }
    }
}
fn default_true() -> bool {
    true
}
wire_struct! { /// Workspace roots to scan; none uses configured roots.
    ScanRequest { roots: Option<Vec<PathBuf>> }
}
wire_struct! { /// A path skipped by a workspace scan and its explanation.
    ScanSkip { path: PathBuf, reason: String }
}
wire_struct! { /// Outcome of a workspace scan.
    ScanReport { registered: Vec<ProjectSummary>, already_known: u32, skipped: Vec<ScanSkip> }
}
wire_struct! { /// Options for reloading a project manifest.
    ReloadRequest { #[serde(default)] discard_overrides: bool }
}

/// A branch, newly created branch, revision, or pull request to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunTarget {
    /// Existing branch name.
    Branch(String),
    /// New branch name and optional base revision.
    NewBranch {
        /// New branch name.
        name: String,
        /// Optional base revision.
        base: Option<String>,
    },
    /// Revision to check out without a branch.
    Detached(String),
    /// Pull request number.
    Pr(u32),
}
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum RunTargetWire {
    NewBranch {
        new_branch: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
    },
    Branch {
        branch: String,
    },
    Detached {
        detached: String,
    },
    Pr {
        pr: u32,
    },
}
impl Serialize for RunTarget {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let wire = match self {
            Self::Branch(branch) => RunTargetWire::Branch {
                branch: branch.clone(),
            },
            Self::NewBranch { name, base } => RunTargetWire::NewBranch {
                new_branch: name.clone(),
                base: base.clone(),
            },
            Self::Detached(detached) => RunTargetWire::Detached {
                detached: detached.clone(),
            },
            Self::Pr(pr) => RunTargetWire::Pr { pr: *pr },
        };
        wire.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for RunTarget {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match RunTargetWire::deserialize(deserializer)? {
            RunTargetWire::Branch { branch } => Self::Branch(branch),
            RunTargetWire::NewBranch { new_branch, base } => Self::NewBranch {
                name: new_branch,
                base,
            },
            RunTargetWire::Detached { detached } => Self::Detached(detached),
            RunTargetWire::Pr { pr } => Self::Pr(pr),
        })
    }
}
/// Create or reuse a checkout, optionally preparing and starting it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CreateInstanceRequest {
    /// Target to run; serializes in the canonical nested form.
    pub target: RunTarget,
    /// Fetch before choosing the branch, default true.
    pub fetch: bool,
    /// Start after creation, default true.
    pub start: bool,
    /// Explicit pin choice, or daemon default.
    pub pin: Option<bool>,
}
impl<'de> Deserialize<'de> for CreateInstanceRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Input {
            target: Option<RunTarget>,
            branch: Option<String>,
            base: Option<String>,
            #[serde(default = "default_true")]
            fetch: bool,
            #[serde(default = "default_true")]
            start: bool,
            pin: Option<bool>,
        }
        let i = Input::deserialize(deserializer)?;
        let target = match (i.target, i.branch, i.base) {
            (Some(t), None, None) => t,
            (None, Some(name), Some(base)) => RunTarget::NewBranch {
                name,
                base: Some(base),
            },
            (None, Some(branch), None) => RunTarget::Branch(branch),
            (None, None, _) => return Err(serde::de::Error::missing_field("target or branch")),
            _ => {
                return Err(serde::de::Error::custom(
                    "target cannot be combined with flat branch or base",
                ));
            }
        };
        Ok(Self {
            target,
            fetch: i.fetch,
            start: i.start,
            pin: i.pin,
        })
    }
}
wire_struct! { /// Pin-state patch for the instance route.
    PatchInstanceRequest { pinned: bool }
}
wire_struct! { /// Empty request body for pinning an instance.
    PinRequest {}
}
wire_struct! { /// Empty request body for unpinning an instance.
    UnpinRequest {}
}
wire_struct! { /// Relabel an instance using a new slug.
    RenameInstanceRequest { slug: String }
}
wire_struct! { /// Re-run the prepare pipeline; force also repeats completed setup.
    PrepareRequest { #[serde(default)] force: bool }
}
wire_struct! { /// Select a listener to adopt when there are multiple matches.
    AdoptRequest { port: Option<u16> }
}
wire_struct! { /// Dismiss a suggestion at its canonical path.
    DismissSuggestionRequest { path: PathBuf }
}
wire_struct! { /// Request a v1 import from a selected source.
    ImportV1Request { from: Option<PathBuf>, #[serde(default = "default_true")] dry_run: bool, #[serde(default)] force: bool }
}
wire_struct! { /// Request a named JSON schema.
    SchemaRequest { name: String }
}
wire_struct! { /// Stop all instances and optionally shared services.
    StopAllRequest { #[serde(default)] include_services: bool }
}
/// Wait query parameters. Timeout is in seconds, with server default 60 and maximum 600.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WaitParams {
    /// Wait for the operation; false returns immediately.
    #[serde(default)]
    pub wait: bool,
    /// Maximum wait in seconds.
    pub timeout: Option<u32>,
}
/// Removal query parameters. User-created worktrees are only forgotten, never deleted.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RemoveInstanceParams {
    /// Override dirty/unpushed checks for managed worktrees.
    #[serde(default)]
    pub force: bool,
    /// Delete an eligible branch after removing a managed worktree.
    #[serde(default)]
    pub delete_branch: bool,
    /// Stop the instance first if necessary.
    #[serde(default)]
    pub stop: bool,
}
wire_struct! { /// A branch in a project's repository.
    BranchInfo { name: String, remote: Option<String>, oid: String, upstream: Option<String>, ahead: u32,
        behind: u32, gone: bool, committed_at: i64, checked_out_at: Option<PathBuf> }
}
/// Reasons a managed checkout cannot be removed without resolving the condition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Blocker {
    /// Checkout is not managed.
    NotManaged,
    /// Instance is running.
    Running,
    /// Worktree lock reason.
    Locked(String),
    /// Number of modified paths.
    Dirty(usize),
    /// Number of unpushed commits.
    Unpushed(u32),
    /// Number of orphaned commits.
    Orphaned(u32),
}
wire_struct! { /// Preflight details shown before removing a checkout.
    RemovalCheck { blockers: Vec<Blocker>, dirty: Vec<Value>, unpushed: u32, orphaned: u32, has_submodules: bool,
        worktree_env: BTreeMap<String, String>, branch: Option<Value> }
}

/// Opaque operation identifier, represented as a string on the wire.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(pub String);
impl std::fmt::Display for OperationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl From<String> for OperationId {
    fn from(value: String) -> Self {
        Self(value)
    }
}
wire_enum! { /// Kind of a long-running operation.
    OperationKind { Start, Stop, Restart, Prepare, CreateInstance, RemoveInstance, StopAll, ProfileActivate, ApiCall }
}
wire_enum! { /// State of an operation or one of its steps.
    OperationStatus { Queued, Running, Succeeded, Failed, Cancelled }
}
wire_struct! { /// One named progress step of an operation.
    OperationStep { name: String, status: OperationStatus, started_at: Option<Timestamp>, finished_at: Option<Timestamp> }
}
wire_struct! { /// A retained operation, including its terminal result or error.
    Operation { id: OperationId, kind: OperationKind, instance: Option<InstanceId>, status: OperationStatus, noop: bool,
        step: Option<String>, steps: Vec<OperationStep>, created_at: Timestamp, finished_at: Option<Timestamp>,
        error: Option<ApiError>, result: Option<Value> }
}
wire_struct! { /// A single operation response; 202 can mean it is still running after a wait.
    OperationResponse { operation: Operation, instance: Option<InstanceView>, created: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")] forgotten: Option<bool> }
}
wire_struct! { /// Result of an action that starts multiple operations.
    OperationsResponse { operations: Vec<Operation> }
}

wire_enum! { /// Snapshot log output format.
    LogFormat { Json, Text }
}
wire_struct! { /// Filters for a log snapshot.
    LogParams { tail: Option<u32>, since: Option<String>, stream: Option<Stream>, grep: Option<String>, format: Option<LogFormat> }
}
wire_struct! { /// A snapshot of instance log lines.
    LogPage { instance: InstanceId, lines: Vec<LogLine>, buffered: u64, truncated: bool }
}
wire_struct! { /// An allocated instance port.
    PortEntry { port: u16, status: InstanceStatus, health_status: String, url: String }
}
wire_struct! { /// A conflict between an instance lease and a discovered holder.
    PortConflict { instance: InstanceId, holder: crate::model::PortHolder }
}
wire_struct! { /// Port leases, unmanaged listeners and conflicts.
    PortsView { instances: BTreeMap<InstanceId, PortEntry>, unmanaged: Vec<Listener>, conflicts: Vec<PortConflict> }
}
wire_enum! { /// Origin of an intel item.
    IntelSource { Manual, Discovered }
}
wire_struct! { /// An indexed document.
    DocEntry { path: String, kind: String, source: IntelSource, exists: bool, size: Option<u64>, mtime: Option<Timestamp> }
}
wire_struct! { /// A page of inert document text.
    DocContent { path: String, content: String, offset: u64, total_chars: u64, truncated: bool }
}
wire_struct! { /// An endpoint parameter hint.
    ParamHint { name: String, r#in: String, required: bool }
}
wire_struct! { /// An indexed API endpoint.
    EndpointEntry { method: String, path: String, description: Option<String>, category: Option<String>, risk: Risk,
        body_hint: Option<String>, params: Vec<ParamHint>, source: IntelSource }
}
wire_struct! { /// Document-discovery counts and source patterns.
    DocsMeta { enabled: bool, count: u32, source_count: u32, sources: Vec<String>, skipped_large: u32, truncated: bool }
}
wire_struct! { /// A failed intel source.
    IntelError { source: String, error: String }
}
wire_struct! { /// Endpoint-discovery counts and source URLs.
    ApiMeta { enabled: bool, count: u32, source_count: u32, sources: Vec<String>, errors: Vec<IntelError> }
}
wire_struct! { /// Intel discovery state.
    DiscoveryMeta { refreshed_at: Option<Timestamp>, docs: DocsMeta, api: ApiMeta }
}
wire_struct! { /// The current intel snapshot for an instance.
    IntelSnapshot { instance: InstanceId, refreshed_at: Option<Timestamp>, stale: bool,
        docs: Vec<DocEntry>, endpoints: Vec<EndpointEntry>, discovery: DiscoveryMeta }
}
wire_struct! { /// Outcome of refreshing intel across instances.
    IntelRefreshReport { refreshed: u32, errors: Vec<IntelError>, took_ms: u64 }
}
wire_struct! { /// Direct and reverse dependencies of a project.
    DependencyEntry { depends_on: Vec<ProjectId>, depended_by: Vec<ProjectId> }
}
/// Dependency graph keyed by project id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DependencyGraph(pub BTreeMap<ProjectId, DependencyEntry>);
wire_struct! { /// Cheap daemon liveness and version response.
    ApiRoot { name: String, version: String, api_version: u32, mcp: String, links: BTreeMap<String, String> }
}
wire_struct! { /// Runtime information about the daemon.
    SystemInfo { version: String, api_version: u32, pid: u32, boot_id: String, started_at: Timestamp, listen: String, tld: String, paths: Value }
}
wire_struct! { /// Result of system reconciliation.
    ReconcileResponse { reconciled: bool }
}
wire_struct! { /// The patched settings and fields that require a daemon restart.
    SettingsPatched { settings: Value, restart_required: Vec<String>, #[serde(default)] overridden_by_env: Vec<String> }
}

wire_enum! { /// Status of an individual doctor check (02 D20).
    CheckStatus { Ok, Warn, Fail, Skipped }
}
wire_struct! { /// Suggested command for fixing a doctor check.
    Fix { description: String, command: String, needs_root: bool }
}
wire_struct! { /// One doctor check and its optional fix.
    Check { id: String, title: String, status: CheckStatus, detail: String, fix: Option<Fix> }
}
wire_struct! { /// Overall system health and its checks.
    SystemHealth { ok: bool, checked_at: Timestamp, checks: Vec<Check> }
}
wire_enum! { /// Known shared-service names.
    ServiceName { Mailpit, Redis, Postgres, PhpFpm }
}
wire_enum! { /// State of a shared service.
    ServiceState { Running, Stopped, Starting, Failed, NotInstalled }
}
wire_enum! { /// Whether a shared service is managed by Lantern or the system.
    ServiceMode { User, System }
}
wire_struct! { /// Current state and control information for a shared service.
    ServiceStatus { name: ServiceName, state: ServiceState, mode: ServiceMode, ports: BTreeMap<String, u16>,
        ui_url: Option<String>, unit: Option<String>, controllable: bool, detail: Option<String> }
}

mod events;
pub use events::{Event, EventParams, WireEvent};

#[cfg(test)]
mod tests;
