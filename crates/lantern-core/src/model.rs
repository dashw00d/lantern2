//! Domain model (docs/01-architecture.md §6 as amended by docs/02-decisions.md).
//!
//! Controller-owned. Lanes: request changes with `ask, controller:` on the board. Wire format:
//! enums serialize as `snake_case` strings; timestamps are RFC 3339 UTC.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::ids::{InstanceId, ProjectId};
use crate::manifest::ResolvedManifest;

// ---------------------------------------------------------------- projects

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectKind {
    #[default]
    Project,
    Service,
    Tool,
    Capability,
    Website,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSource {
    Manifest,
    Detected,
    Manual,
}

/// Monorepos: a project may be a subdirectory of its repo; several projects may share a repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRepo {
    /// Canonical git common dir: the repo's identity.
    pub common_dir: PathBuf,
    /// Primary worktree top level (derived at runtime, not persisted in state.json).
    pub git_root: PathBuf,
    /// Project dir relative to `git_root` (empty for the repo root).
    pub subpath: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    /// Legacy (v1) names accepted on lookup, e.g. by MCP tools.
    pub aliases: Vec<String>,
    /// Display name.
    pub name: String,
    /// Project dir in the primary checkout (absolute, canonical).
    pub root: PathBuf,
    /// `None` = not a git project.
    pub repo: Option<ProjectRepo>,
    pub kind: ProjectKind,
    pub enabled: bool,
    pub tags: Vec<String>,
    pub registered_at: Timestamp,
    pub source: ProjectSource,
}

// ---------------------------------------------------------------- checkouts & instances

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutKind {
    Primary,
    Worktree,
    Managed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkout {
    /// Project dir inside this checkout = `<git_root>/<subpath>`.
    pub path: PathBuf,
    pub git_root: Option<PathBuf>,
    pub kind: CheckoutKind,
    /// `None` = detached HEAD (or not a git project).
    pub branch: Option<String>,
    /// Commit sha.
    pub head: Option<String>,
    /// Best-effort, refreshed lazily.
    pub dirty: Option<bool>,
    /// `git worktree lock` reason, if locked.
    pub locked: Option<String>,
    /// Worktree no longer listed by git, or its path is gone.
    pub missing: bool,
}

/// How an instance is served (docs/14-config.md). `Php`/`Static` are route-only: no unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServeMode {
    Proxy,
    Php,
    Static,
    #[default]
    Unknown,
}

/// A runnable unit = one Project × one Checkout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Instance {
    pub id: InstanceId,
    pub project: ProjectId,
    pub checkout: Checkout,
    /// `myapp.glow` | `feature-x.myapp.glow`.
    pub domain: String,
    /// Read from *this checkout* (fallback: the primary's). Never sent whole on the event bus (02 D7).
    pub manifest: Arc<ResolvedManifest>,
    pub runtime: RuntimeState,
    /// Show even when stopped (non-primary instances).
    pub pinned: bool,
    /// Route this instance's hand-started server when discovery sees it (02 D9).
    pub adopt: bool,
    /// Lockfiles or setup commands changed since the last prepare.
    pub setup_stale: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceStatus {
    #[default]
    Stopped,
    /// Creating worktree / running setup steps.
    Preparing,
    /// Process spawned, waiting for readiness.
    Starting,
    Running,
    /// Running but health checks failing.
    Unhealthy,
    Stopping,
    /// Exited unexpectedly; may be retrying (see `RuntimeState::next_retry_at`).
    Crashed,
    /// Can't run: no run cmd / invalid manifest / port mismatch.
    NeedsConfig,
    /// A matched listener Lantern did not start.
    External,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RuntimeState {
    pub status: InstanceStatus,
    pub port: Option<u16>,
    /// systemd unit name when managed, e.g. `lantern-inst-myapp_feature-x.service`.
    pub unit: Option<String>,
    pub main_pid: Option<u32>,
    pub started_at: Option<Timestamp>,
    pub last_exit: Option<ExitInfo>,
    pub restarts: u32,
    pub next_retry_at: Option<Timestamp>,
    pub health: HealthSummary,
    /// Human-readable reason for the current status.
    pub message: Option<String>,
    /// Machine-readable cause behind `message`.
    pub issue: Option<RuntimeIssue>,
    /// Where readiness found the listener; Caddy routes here.
    pub upstream: Option<SocketAddr>,
    /// Ephemeral: the matched hand-started server (status `External`).
    pub external: Option<ExternalState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitKind {
    /// Exited with a status code.
    Exited,
    /// Killed by a signal.
    Signaled,
    /// systemd stop/start timeout.
    Timeout,
    OomKill,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitInfo {
    pub at: Timestamp,
    pub kind: ExitKind,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub uptime_secs: Option<u64>,
    /// systemd `Result=` value (`exit-code`, `signal`, `oom-kill`, `timeout`, ...).
    pub unit_result: Option<String>,
    /// Exit was discovered after the fact (e.g. at daemon boot), not observed live.
    pub observed_late: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    #[default]
    Unknown,
    Healthy,
    Failing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthSample {
    pub at: Timestamp,
    pub ok: bool,
    pub status: Option<u16>,
    pub latency_ms: Option<u32>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HealthSummary {
    pub state: HealthState,
    pub checked_at: Option<Timestamp>,
    pub latency_ms: Option<u32>,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
    /// Newest first, max 10.
    pub recent: Vec<HealthSample>,
}

/// Machine-readable cause of a status, so clients can offer a fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueCode {
    /// The server listens somewhere other than its leased port.
    PortMismatch,
    /// The leased port is held by a foreign process.
    PortConflict,
    PortsExhausted,
    /// Version manager could not select the requested toolchain (exit 78).
    ToolchainMissing,
    /// Exit 126/127.
    CommandNotFound,
    SpawnFailed,
    ReadinessTimeout,
    CrashLoop,
    HealthFailing,
    DependencyFailed,
    ManifestInvalid,
    SetupFailed,
    RouteFailed,
    SystemdUnavailable,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeIssue {
    pub code: IssueCode,
    pub detail: String,
    /// Suggested fix, e.g. "run `lantern env refresh`".
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scheme {
    Http,
    Https,
}

/// A matched hand-started server (docs/12-discovery.md §3.11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalState {
    pub port: u16,
    pub upstream: SocketAddr,
    pub scheme: Scheme,
    pub pid: Option<u32>,
    pub label: String,
    pub extra_ports: Vec<u16>,
    pub since: Timestamp,
}

// ---------------------------------------------------------------- listeners (docs/12-discovery.md §3.2)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenerClass {
    Project,
    DevServer,
    Docker,
    Infra,
    Other,
    Foreign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MatchSource {
    Unit,
    Cwd,
    AncestorCwd { depth: u8 },
    CmdlinePath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exposure {
    Loopback,
    AllInterfaces,
    Interface,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeInfo {
    pub http: bool,
    pub status: Option<u16>,
    pub title: Option<String>,
    pub server: Option<String>,
    pub powered_by: Option<String>,
    pub hint: Option<String>,
    pub tls: bool,
    pub reached_via: Option<SocketAddr>,
    pub at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerInfo {
    /// Best-effort label from the docker-proxy command line (v2.0 does not use the Docker API).
    pub label: String,
    pub container_port: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HideOrigin {
    Default,
    User,
    Lantern,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiddenReason {
    pub origin: HideOrigin,
    pub why: String,
}

/// An observed TCP LISTEN socket (v4/v6, loopback and non-loopback).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listener {
    /// Primary bind address.
    pub addr: SocketAddr,
    /// `None`: another user's/root's socket, or owner not readable.
    pub pid: Option<u32>,
    pub exe: Option<PathBuf>,
    /// ALWAYS redacted before leaving `discovery/`.
    pub cmdline: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub matched: Option<InstanceId>,
    /// Process is inside a `lantern-inst-*` unit.
    pub managed: bool,
    pub port: u16,
    /// Same port + same owner, e.g. `0.0.0.0` and `::`.
    pub other_addrs: Vec<SocketAddr>,
    pub uid: u32,
    pub exposure: Exposure,
    pub started_at: Option<Timestamp>,
    pub project: Option<ProjectId>,
    pub branch: Option<String>,
    pub matched_by: Option<MatchSource>,
    pub unit: Option<String>,
    pub class: ListenerClass,
    /// "Vite", "uvicorn", "node server.mjs", ...
    pub label: String,
    pub probe: Option<ProbeInfo>,
    pub docker: Option<DockerInfo>,
    /// A route currently points here.
    pub adopted: bool,
    /// Set only in `include_hidden` responses.
    pub hidden: Option<HiddenReason>,
}

// ---------------------------------------------------------------- logs (02 D6/D7)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    /// Reserved: journald merges a unit's stdout and stderr.
    Stderr,
    /// Lantern's own lines and `deploy.*` one-shots.
    System,
    /// `setup` one-shots.
    Setup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    pub instance: InstanceId,
    pub stream: Stream,
    pub line: String,
    pub ts: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_are_snake_case_on_the_wire() {
        assert_eq!(
            serde_json::to_string(&InstanceStatus::NeedsConfig).unwrap(),
            "\"needs_config\""
        );
        assert_eq!(
            serde_json::to_string(&CheckoutKind::Managed).unwrap(),
            "\"managed\""
        );
        assert_eq!(
            serde_json::to_string(&MatchSource::AncestorCwd { depth: 2 }).unwrap(),
            r#"{"kind":"ancestor_cwd","depth":2}"#
        );
    }

    #[test]
    fn runtime_state_default_is_stopped() {
        let r = RuntimeState::default();
        assert_eq!(r.status, InstanceStatus::Stopped);
        let json = serde_json::to_string(&r).unwrap();
        let back: RuntimeState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }
}
