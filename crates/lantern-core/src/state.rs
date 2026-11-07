//! Versioned state types and pure migrations; persistence belongs to the registry.
use crate::{
    Timestamp,
    ids::{InstanceId, ProjectId},
    manifest::raw::RawManifest,
    model::{CheckoutKind, ExitInfo, InstanceStatus, ProjectSource},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

/// Current state file version.
pub const STATE_VERSION: u32 = 1;
/// Daemon-owned persisted state (02 D4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateFile {
    pub version: u32,
    pub saved_at: Timestamp,
    pub active_profile: Option<String>,
    pub repos: BTreeMap<PathBuf, RepoRecord>,
    pub projects: BTreeMap<ProjectId, ProjectRecord>,
    pub instances: BTreeMap<InstanceId, InstanceRecord>,
    pub port_leases: BTreeMap<InstanceId, PortLease>,
    pub dismissed_suggestions: BTreeSet<PathBuf>,
    pub imports: BTreeMap<String, ImportRecord>,
}
/// Persisted git repository metadata; the common directory is its map key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRecord {
    pub slug: String,
}
/// A project's stable git identity, excluding its derived git root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRepoRecord {
    pub common_dir: PathBuf,
    pub subpath: PathBuf,
}
/// Persisted project metadata and sparse overrides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub id: ProjectId,
    pub aliases: Vec<String>,
    pub root: PathBuf,
    pub repo: Option<ProjectRepoRecord>,
    pub source: ProjectSource,
    pub registered_at: Timestamp,
    pub last_seen_at: Timestamp,
    pub missing: bool,
    #[serde(default, skip_serializing_if = "RawManifest::is_empty")]
    pub overrides: RawManifest,
}
/// Creation phase for a checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutPhase {
    Creating,
    Ready,
}
/// Persisted checkout data nested beneath its instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckoutRecord {
    pub path: PathBuf,
    pub kind: CheckoutKind,
    pub phase: CheckoutPhase,
    pub created_at: Timestamp,
    pub created_branch: bool,
    pub materialized: Vec<PathBuf>,
    pub setup: Option<SetupStamp>,
    pub missing_since: Option<Timestamp>,
}
/// Successful setup hashes and completion time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupStamp {
    pub lock_hash: String,
    pub commands_hash: String,
    pub finished_at: Timestamp,
}
/// Persisted per-instance intent and reconciliation hints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceRecord {
    pub project: ProjectId,
    pub checkout: CheckoutRecord,
    pub pinned: bool,
    pub adopt: bool,
    pub activated: bool,
    pub first_started: Option<Timestamp>,
    pub last_started: Option<Timestamp>,
    pub last_runtime: Option<LastRuntime>,
}
impl InstanceRecord {
    /// A pinned or previously started checkout has a stable claim.
    pub fn claimed(&self) -> bool {
        self.pinned || self.first_started.is_some()
    }
}
/// Runtime state retained only as a reconciliation hint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastRuntime {
    pub status: InstanceStatus,
    pub port: Option<u16>,
    pub unit: Option<String>,
    pub main_pid: Option<u32>,
    pub started_at: Option<Timestamp>,
    pub last_exit: Option<ExitInfo>,
    pub restarts: u32,
    pub message: Option<String>,
}
/// Sticky instance port assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortLease {
    pub port: u16,
    pub leased_at: Timestamp,
}
/// Record of a completed import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportRecord {
    pub at: Timestamp,
    pub from: PathBuf,
    pub projects: u32,
}
/// State parsing and migration failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("state.json is a Lantern v1 file; run 'lantern migrate-v1'")]
    V1File,
    #[error("state.json has no valid version")]
    MissingVersion,
    #[error(
        "state.json is version {found}; this lantern supports up to {supported}. Upgrade lantern, or restore state.json.bak"
    )]
    FutureVersion { found: u64, supported: u32 },
    #[error("no migration exists from state version {0}")]
    UnsupportedVersion(u64),
    #[error("invalid state.json: {0}")]
    Json(#[from] serde_json::Error),
}
/// Apply all available version-to-version migrations without IO.
pub fn migrate(v: serde_json::Value) -> Result<serde_json::Value, Error> {
    let Some(version) = v.get("version").and_then(serde_json::Value::as_u64) else {
        return if v.get("port_assignments").is_some() {
            Err(Error::V1File)
        } else {
            Err(Error::MissingVersion)
        };
    };
    if version > u64::from(STATE_VERSION) {
        return Err(Error::FutureVersion {
            found: version,
            supported: STATE_VERSION,
        });
    }
    if version != u64::from(STATE_VERSION) {
        return Err(Error::UnsupportedVersion(version));
    }
    Ok(v)
}
/// Parse and migrate a state file.
pub fn from_str(src: &str) -> Result<StateFile, Error> {
    let value = serde_json::from_str(src)?;
    Ok(serde_json::from_value(migrate(value)?)?)
}
/// Render a state file as pretty JSON.
pub fn to_string(state: &StateFile) -> Result<String, Error> {
    Ok(serde_json::to_string_pretty(state)? + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn golden_round_trip_and_identity_migration() {
        let src = include_str!("../tests/fixtures/state/v1.json");
        let original: serde_json::Value = serde_json::from_str(src).unwrap();
        assert_eq!(migrate(original).unwrap().get("version").unwrap(), 1);
        let state = from_str(src).unwrap();
        assert!(state.instances.values().next().unwrap().claimed());
        let again = from_str(&to_string(&state).unwrap()).unwrap();
        assert_eq!(again, state);
        assert!(!to_string(&state).unwrap().contains("adopted"));
    }
    #[test]
    fn future_and_v1_are_refused() {
        assert!(matches!(
            migrate(serde_json::json!({"version":2})),
            Err(Error::FutureVersion { .. })
        ));
        assert!(matches!(
            migrate(serde_json::json!({"port_assignments":{}})),
            Err(Error::V1File)
        ));
        assert!(matches!(
            migrate(serde_json::json!({"version":0})),
            Err(Error::UnsupportedVersion(0))
        ));
    }
}
