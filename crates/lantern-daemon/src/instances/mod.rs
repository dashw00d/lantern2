//! Checkout discovery, branch operations, preparation and safe removal.

use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use lantern_core::{
    ids::{InstanceId, ProjectId},
    manifest::ResolvedManifest,
    model::{Project, Stream},
    state::InstanceRecord,
};
use tokio_util::sync::CancellationToken;

/// Project metadata needed to derive instances in a shared repository.
#[derive(Debug, Clone)]
pub struct ProjectConfig {
    /// Registered project and its primary git root and subpath.
    pub project: Project,
    /// Stable directory name shared by projects in this repository.
    pub repo_slug: String,
    /// The primary instance's resolved domain.
    pub primary_domain: String,
}

/// Synchronous registry operations required by the instance service.
/// Mutations must persist state, allocate a port when needed, and publish changed views.
pub trait InstanceStore: Send + Sync {
    /// Look up a registered project.
    fn project(&self, id: &ProjectId) -> Result<Option<ProjectConfig>, String>;
    /// All registered projects sharing a canonical git common directory.
    fn projects_in_repo(&self, common_dir: &Path) -> Result<Vec<ProjectConfig>, String>;
    /// All persisted instance records, including missing records for revival and domain collisions.
    fn records(&self) -> Result<Vec<(InstanceId, InstanceRecord)>, String>;
    /// Insert or replace a record and publish its new view.
    fn upsert(&self, id: &InstanceId, record: InstanceRecord) -> Result<(), String>;
    /// Remove a record, release its lease and publish its removal.
    fn remove(&self, id: &InstanceId) -> Result<(), String>;
    /// Atomically move an identity, preserving its record, lease and related runtime state.
    fn rename(&self, old: &InstanceId, new: &InstanceId) -> Result<(), String>;
    /// Resolve the checkout's manifest with primary fallback, rejecting invalid checkout files.
    fn manifest(&self, id: &InstanceId) -> Result<Arc<ResolvedManifest>, String>;
    /// Obtain the sticky port lease created with the record.
    fn port(&self, id: &InstanceId) -> Result<Option<u16>, String>;
}

/// One setup command, rendered for the shell with values supplied only in the environment.
#[derive(Debug, Clone)]
pub struct OneShotSpec {
    /// Instance whose journal receives the output.
    pub instance: InstanceId,
    /// Command working directory.
    pub cwd: PathBuf,
    /// Shell-rendered command; variable data is never interpolated into it.
    pub shell_command: String,
    /// Process environment, including LANTERN template variables.
    pub env: BTreeMap<String, String>,
    /// Per-command timeout.
    pub timeout: Duration,
    /// Cancellation stops the whole one-shot unit.
    pub cancel: CancellationToken,
    /// Journal stream for all lines, always `Stream::Setup` here.
    pub stream: Stream,
}

/// Future returned by the runtime's one-shot adapter.
pub type OneShotFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Run a setup unit with the runtime's toolchain and journal streaming.
pub trait OneShot: Send + Sync {
    /// Run to successful exit, propagating failure, timeout and cancellation.
    fn run(&self, spec: OneShotSpec) -> OneShotFuture<'_>;
}

/// Dependency detection and setup stamp helpers.
pub mod deps;
/// Safe file materialization helpers.
pub mod files;

/// Instance operation failure.
#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
    /// Git operation failed.
    #[error(transparent)]
    Git(#[from] crate::git::GitError),
    /// Filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Registry or runtime adapter failed.
    #[error("adapter: {0}")]
    Adapter(String),
    /// Requested project or instance does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// Project has no git repository.
    #[error("not a git repository")]
    NotARepo,
    /// Invalid or escaping file pattern or destination.
    #[error("unsafe path: {0}")]
    UnsafePath(String),
    /// File limits were exceeded.
    #[error("copy limit exceeded; consider link instead")]
    CopyLimit,
    /// Cancellation was requested.
    #[error("operation cancelled")]
    Cancelled,
    /// State or safety policy refused an operation.
    #[error("{0}")]
    Refused(String),
}
