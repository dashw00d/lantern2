//! Adapters connecting the independently testable runtime and checkout services.

use std::{
    collections::BTreeMap,
    net::TcpListener,
    path::Path,
    sync::{Arc, OnceLock, Weak},
    time::Duration,
};

use lantern_core::{
    Timestamp,
    api::OperationStatus,
    ids::{InstanceId, ProjectId},
    manifest::ResolvedManifest,
    model::{Instance, InstanceStatus, Project, RuntimeState},
    settings::Settings,
    state::InstanceRecord,
};
use tokio_util::sync::CancellationToken;

use crate::{
    discovery::PortBinding,
    instances::{InstanceService, InstanceStore, OneShot, OneShotFuture, OneShotSpec, ProjectConfig},
    operations::OpHandle,
    registry::RegistryHandle,
    runtime::{
        OneShotLaunch, Runtime,
        ports::{PortAvailability, PortLeases, PortRange},
        seams::{InstancePreparer, PortProbe, PrepareFuture, PreparedInstance, RuntimeStore, StoreError},
    },
};

/// Registry-backed implementation of both consumers' storage interfaces.
pub struct RegistryAdapter {
    registry: RegistryHandle,
    probe: Arc<dyn PortProbe>,
    settings: Settings,
    profiles_dir: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for RegistryAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryAdapter").finish_non_exhaustive()
    }
}
impl RegistryAdapter {
    /// Use the same registry and fresh port probe as the supervisor.
    pub fn new(registry: RegistryHandle, probe: Arc<dyn PortProbe>, settings: Settings) -> Self {
        Self {
            registry,
            probe,
            settings,
            profiles_dir: None,
        }
    }

    /// Use an explicit profile directory to supply active launch overlays.
    pub fn with_profile_dir(mut self, path: std::path::PathBuf) -> Self {
        self.profiles_dir = Some(path);
        self
    }
    /// Forward checkout setup staleness into the registry's live instance view.
    pub fn set_setup_stale(&self, id: &InstanceId, stale: bool) -> Result<(), String> {
        self.registry
            .set_setup_stale(id, stale)
            .map_err(|e| e.to_string())
    }
    /// Wait for all earlier registry mutations to be durably written.
    pub async fn flush_registry(&self) -> Result<(), String> {
        self.registry.flush().await
    }
    fn active_profile(&self) -> Result<Option<lantern_core::profiles::Profile>, String> {
        let Some(name) = self.registry.active_profile() else {
            return Ok(None);
        };
        let profiles = if let Some(path) = &self.profiles_dir {
            lantern_core::profiles::Profiles::load_dir(path)
                .map_err(|e| e.to_string())?
                .0
        } else {
            lantern_core::profiles::Profiles::default()
        };
        let profile = profiles
            .get(&name)
            .ok_or_else(|| format!("active profile {name} was not found or is invalid"))?;
        if profile
            .env
            .keys()
            .any(|key| key == "PORT" || key == "HOST" || key.starts_with("LANTERN_"))
        {
            return Err("profile environment overrides a reserved Lantern variable".into());
        }
        Ok(Some(profile))
    }
    fn ensure_port(&self, id: &InstanceId) -> Result<u16, String> {
        let settings = &self.settings;
        let range = self
            .active_profile()?
            .and_then(|p| p.port_range)
            .unwrap_or([settings.ports.range_start, settings.ports.range_end]);
        self.registry.update_port_leases(|leases| {
            if let Some(port) = leases.table().get(id) {
                return Ok(*port);
            }
            let reserved = [
                settings.daemon.port,
                settings.proxy.http_port,
                settings.proxy.https_port,
                settings
                    .dns
                    .listen
                    .parse::<std::net::SocketAddr>()
                    .map_or(5354, |address| address.port()),
                2019,
            ];
            let result = leases
                .try_allocate(
                    id,
                    PortRange {
                        start: range[0],
                        end: range[1],
                    },
                    &reserved,
                    |port| match self.probe.port_binding(port, None)? {
                        PortBinding::HeldByOther(holder) => Ok(PortAvailability::Busy(Some(holder))),
                        PortBinding::BoundByUnit { .. } => Ok(PortAvailability::Busy(None)),
                        PortBinding::Free => match TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)) {
                            Ok(_) => Ok(PortAvailability::Free),
                            Err(e)
                                if e.kind() == std::io::ErrorKind::AddrInUse
                                    || e.kind() == std::io::ErrorKind::PermissionDenied =>
                            {
                                Ok(PortAvailability::Busy(None))
                            }
                            Err(e) => Err(e),
                        },
                    },
                )
                .map_err(|e| format!("port allocation failed: {e:?}"))?;
            Ok(result.port)
        })
    }

    fn project_config(&self, id: &ProjectId) -> Option<ProjectConfig> {
        let project = self.registry.project(id)?;
        let state = self.registry.state_snapshot();
        let repo_slug = project
            .repo
            .as_ref()
            .and_then(|repo| state.repos.get(&repo.common_dir))
            .map(|r| r.slug.clone())
            .unwrap_or_else(|| id.to_string());
        let primary_domain = self.registry.instance(&InstanceId::primary(id))?.domain;
        Some(ProjectConfig {
            project,
            repo_slug,
            primary_domain,
        })
    }
}

impl RuntimeStore for RegistryAdapter {
    fn instance(&self, id: &InstanceId) -> Result<Option<Instance>, StoreError> {
        Ok(self.registry.instance(id))
    }
    fn instances(&self) -> Result<Vec<Instance>, StoreError> {
        Ok(self
            .registry
            .instances()
            .into_iter()
            .filter_map(|i| self.registry.instance(&i.id))
            .collect())
    }
    fn project(&self, id: &ProjectId) -> Result<Option<Project>, StoreError> {
        Ok(self.registry.project(id))
    }
    fn port_leases(&self) -> Result<BTreeMap<InstanceId, u16>, StoreError> {
        Ok(self
            .registry
            .state_snapshot()
            .port_leases
            .into_iter()
            .map(|(id, lease)| (id, lease.port))
            .collect())
    }
    fn compare_port_leases(
        &self,
        expected: &BTreeMap<InstanceId, u16>,
        replacement: BTreeMap<InstanceId, u16>,
    ) -> Result<bool, StoreError> {
        self.registry.update_port_leases(|leases| {
            if leases.table() != expected {
                return Ok(false);
            }
            *leases = PortLeases::from_table(replacement);
            Ok(true)
        })
    }
    fn update_runtime(
        &self,
        id: &InstanceId,
        update: Box<dyn FnOnce(&mut RuntimeState) + Send>,
    ) -> Result<(), StoreError> {
        self.registry
            .update_runtime(id, update)
            .map_err(|e| StoreError(e.to_string()))
    }
    fn activated(&self, id: &InstanceId) -> Result<bool, StoreError> {
        self.registry
            .instance_record(id)
            .map(|r| r.activated)
            .ok_or_else(|| StoreError(format!("instance {id} not found")))
    }
    fn set_activated(&self, id: &InstanceId, activated: bool) -> Result<(), StoreError> {
        self.registry
            .set_activated(id, activated)
            .map_err(|e| StoreError(e.to_string()))
    }
    fn record_started(&self, id: &InstanceId) -> Result<(), StoreError> {
        self.registry
            .record_started(id, Timestamp::now())
            .map_err(|e| StoreError(e.to_string()))
    }
    fn profile_environment(&self) -> Result<BTreeMap<String, String>, StoreError> {
        self.active_profile()
            .map(|p| p.map_or_else(BTreeMap::new, |p| p.env))
            .map_err(StoreError)
    }
    fn profile_port_range(&self) -> Result<Option<[u16; 2]>, StoreError> {
        self.active_profile()
            .map(|p| p.and_then(|p| p.port_range))
            .map_err(StoreError)
    }
    fn resolve_dependency(&self, name: &str) -> Result<Option<InstanceId>, StoreError> {
        Ok(self
            .registry
            .lookup_project(name)
            .map(|p| InstanceId::primary(&p)))
    }
}

impl InstanceStore for RegistryAdapter {
    fn set_setup_stale(&self, id: &InstanceId, stale: bool) -> Result<(), String> {
        RegistryAdapter::set_setup_stale(self, id, stale)
    }
    fn set_checkout_git(
        &self,
        id: &InstanceId,
        git_root: Option<std::path::PathBuf>,
        branch: Option<String>,
        head: Option<String>,
        locked: Option<String>,
    ) -> Result<(), String> {
        self.registry
            .set_checkout_git(id, git_root, branch, head, locked)
            .map_err(|e| e.to_string())
    }
    fn flush(&self) -> crate::instances::InstanceStoreFlushFuture<'_> {
        Box::pin(self.flush_registry())
    }
    fn project(&self, id: &ProjectId) -> Result<Option<ProjectConfig>, String> {
        Ok(self.project_config(id))
    }
    fn projects_in_repo(&self, common_dir: &Path) -> Result<Vec<ProjectConfig>, String> {
        Ok(self
            .registry
            .projects()
            .into_iter()
            .filter(|p| {
                p.project
                    .repo
                    .as_ref()
                    .is_some_and(|r| r.common_dir == common_dir)
            })
            .filter_map(|p| self.project_config(&p.project.id))
            .collect())
    }
    fn records(&self) -> Result<Vec<(InstanceId, InstanceRecord)>, String> {
        Ok(self.registry.state_snapshot().instances.into_iter().collect())
    }
    fn upsert(&self, id: &InstanceId, record: InstanceRecord) -> Result<(), String> {
        let had_lease = self.registry.state_snapshot().port_leases.contains_key(id);
        self.ensure_port(id)?;
        if let Err(e) = self.registry.upsert_instance(id.clone(), record) {
            if !had_lease {
                self.registry
                    .update_port_leases(|leases| {
                        leases.release(id);
                        Ok::<_, ()>(())
                    })
                    .ok();
            }
            return Err(e.to_string());
        }
        Ok(())
    }
    fn remove(&self, id: &InstanceId) -> Result<(), String> {
        self.registry.remove_instance(id).map_err(|e| e.to_string())
    }
    fn rename(&self, old: &InstanceId, new: &InstanceId) -> Result<(), String> {
        self.registry
            .rename_instance(old, new.clone())
            .map_err(|e| e.to_string())
    }
    fn manifest(&self, id: &InstanceId) -> Result<Arc<ResolvedManifest>, String> {
        self.registry
            .manifest(id)
            .map(|(m, _)| m)
            .ok_or_else(|| format!("instance {id} not found"))
    }
    fn status(&self, id: &InstanceId) -> Result<InstanceStatus, String> {
        self.registry
            .instance(id)
            .map(|i| i.runtime.status)
            .ok_or_else(|| format!("instance {id} not found"))
    }
    fn port(&self, id: &InstanceId) -> Result<Option<u16>, String> {
        self.ensure_port(id).map(Some)
    }
}

/// Late binding avoids a strong reference cycle between preparation and the runtime.
#[derive(Debug, Default)]
pub struct RuntimeOneShot {
    runtime: OnceLock<Weak<Runtime>>,
}
impl RuntimeOneShot {
    /// Install the runtime after constructing the preparation service.
    pub fn bind(&self, runtime: &Arc<Runtime>) -> Result<(), &'static str> {
        self.runtime
            .set(Arc::downgrade(runtime))
            .map_err(|_| "runtime already bound")
    }
}
impl OneShot for RuntimeOneShot {
    fn run(&self, spec: OneShotSpec) -> OneShotFuture<'_> {
        Box::pin(async move {
            let runtime = self
                .runtime
                .get()
                .and_then(Weak::upgrade)
                .ok_or("runtime is unavailable")?;
            let timeout = spec.timeout;
            let launch = runtime
                .prepare_oneshot(
                    &spec.instance,
                    OneShotLaunch {
                        verb: "setup".into(),
                        cwd: spec.cwd,
                        shell_command: spec.shell_command,
                        env: spec.env,
                        timeout,
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            let operation = runtime.run_oneshot(launch);
            let result = tokio::select! {
                result = operation.wait(timeout + Duration::from_secs(30)) => result,
                () = spec.cancel.cancelled() => { operation.cancel(); operation.wait(timeout + Duration::from_secs(30)).await }
            };
            match result {
                Some(snapshot)
                    if snapshot.status == OperationStatus::Succeeded && !spec.cancel.is_cancelled() =>
                {
                    Ok(())
                }
                Some(snapshot)
                    if matches!(
                        snapshot.status,
                        OperationStatus::Queued | OperationStatus::Running
                    ) =>
                {
                    operation.cancel();
                    let _ = operation.wait(Duration::from_secs(30)).await;
                    Err("setup timed out while stopping".into())
                }
                Some(snapshot) => Err(snapshot
                    .error
                    .map_or_else(|| "setup did not complete".into(), |e| e.message)),
                None => {
                    operation.cancel();
                    Err("setup operation was lost".into())
                }
            }
        })
    }
}

/// Runtime preparation hook backed by the instance service.
#[derive(Debug)]
pub struct CheckoutPreparer(pub Arc<InstanceService>);
impl InstancePreparer for CheckoutPreparer {
    fn ensure_ready<'a>(
        &'a self,
        instance: &'a Instance,
        _port: Option<u16>,
        cancel: CancellationToken,
        progress: &'a OpHandle,
    ) -> PrepareFuture<'a> {
        Box::pin(async move {
            let prepared = self
                .0
                .ensure_ready_with_progress(&instance.id, cancel, progress)
                .await
                .map_err(|e| e.to_string())?;
            Ok(PreparedInstance {
                manifest: prepared.manifest,
                worktree_env: prepared.worktree_env,
            })
        })
    }
}
