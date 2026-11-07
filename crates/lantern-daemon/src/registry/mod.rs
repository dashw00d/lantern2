//! Single owner of persisted project records and ephemeral daemon snapshots.

pub mod migrate_v1;
pub mod scan;
pub mod state_io;

use crate::{
    discovery::matcher::{CheckoutIndex, CheckoutRef},
    events::EventBus,
    runtime::ports::PortLeases,
};
use lantern_core::{
    Timestamp,
    api::{Event, InstanceView, ProjectSummary, Risk, RoutingMeta, ServiceStatus, Suggestion},
    detect::{self, Templates},
    dirs::Dirs,
    ids::{IdError, InstanceId, ProjectId},
    manifest::{self, Diagnostic, ResolveInput, ResolvedManifest, Severity},
    model::{
        Checkout, CheckoutKind, ExternalState, Instance, InstanceStatus, Listener, Project, ProjectRepo,
        RuntimeState,
    },
    settings::Settings,
    state::{
        CheckoutPhase, CheckoutRecord, InstanceRecord, LastRuntime, PortLease, ProjectRecord, RepoRecord,
        SetupStamp, StateFile,
    },
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::sync::watch;

/// Failures from registry operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    State(#[from] state_io::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Id(#[from] IdError),
    #[error("project {0} was not found")]
    ProjectNotFound(ProjectId),
    #[error("instance {0} was not found")]
    InstanceNotFound(InstanceId),
    #[error("project id, alias, or path conflicts with an existing project: {0}")]
    Conflict(String),
    #[error("instance id does not belong to its project")]
    WrongProject,
    #[error("primary instance cannot be removed independently")]
    Primary,
    #[error("invalid profile name: {0}")]
    InvalidProfile(String),
}

/// A discovery notification after appearance and disappearance delays have elapsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalChange {
    /// An unmanaged listener appeared for an instance.
    Appeared {
        instance: InstanceId,
        external: ExternalState,
    },
    /// A known unmanaged listener changed.
    Updated {
        instance: InstanceId,
        external: ExternalState,
    },
    /// The unmanaged listener disappeared after its grace period.
    Gone { instance: InstanceId },
}

#[derive(Debug, Clone)]
struct CachedManifest {
    value: Arc<ResolvedManifest>,
    rev: String,
}

#[derive(Debug)]
struct State {
    file: StateFile,
    io: state_io::StateIo,
    settings: Settings,
    templates: Templates,
    home: PathBuf,
    bus: EventBus,
    index: watch::Sender<CheckoutIndex>,
    manifests: BTreeMap<InstanceId, CachedManifest>,
    runtime: BTreeMap<InstanceId, RuntimeState>,
    checkouts: BTreeMap<InstanceId, Checkout>,
    setup_stale: BTreeMap<InstanceId, bool>,
    listeners: Vec<Listener>,
    listener_seq: u64,
    services: Vec<ServiceStatus>,
    suggestions: Vec<Suggestion>,
    listener_suggestions: BTreeMap<PathBuf, Suggestion>,
    suggestion_checked: BTreeMap<PathBuf, std::time::Instant>,
}

/// Cheap, thread-safe handle; all persisted and runtime mutations are serialized here.
#[derive(Debug, Clone)]
pub struct RegistryHandle {
    inner: Arc<Mutex<State>>,
}

impl RegistryHandle {
    /// Open the locked state store. `home` is explicit and is never read from the process environment.
    pub fn open(
        dirs: &Dirs,
        settings: Settings,
        templates: Templates,
        home: PathBuf,
        bus: EventBus,
    ) -> Result<(Self, state_io::Recovery), Error> {
        let (io, file, recovery) = state_io::StateIo::open(dirs)?;
        let (index, _) = watch::channel(CheckoutIndex::new([], &home));
        let mut state = State {
            file,
            io,
            settings,
            templates,
            home,
            bus,
            index,
            manifests: BTreeMap::new(),
            runtime: BTreeMap::new(),
            checkouts: BTreeMap::new(),
            setup_stale: BTreeMap::new(),
            listeners: Vec::new(),
            listener_seq: 0,
            services: Vec::new(),
            suggestions: Vec::new(),
            listener_suggestions: BTreeMap::new(),
            suggestion_checked: BTreeMap::new(),
        };
        let ids: Vec<_> = state.file.instances.keys().cloned().collect();
        for id in ids {
            state.rebuild_checkout(&id);
            state.resolve_manifest(&id)?;
        }
        state.publish_index();
        Ok((
            Self {
                inner: Arc::new(Mutex::new(state)),
            },
            recovery,
        ))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Flush every queued persisted mutation; call after registration, lease changes and shutdown.
    pub async fn flush(&self) -> Result<(), String> {
        let io = self.lock().io.clone();
        io.flush().await
    }
    /// Flush and stop the state writer, releasing its exclusive lock.
    pub async fn shutdown(&self) -> Result<(), String> {
        let io = self.lock().io.clone();
        io.shutdown().await
    }
    /// Subscribe to nonfatal persistence failures and their subsequent recovery.
    pub fn subscribe_write_errors(&self) -> watch::Receiver<Option<String>> {
        self.lock().io.subscribe_errors()
    }
    /// Copy the complete persisted state for inspection or integration.
    pub fn state_snapshot(&self) -> StateFile {
        self.lock().file.clone()
    }
    /// Follow immutable checkout index replacements for discovery.
    pub fn subscribe_checkout_index(&self) -> watch::Receiver<CheckoutIndex> {
        self.lock().index.subscribe()
    }
    /// Resolve an exact project id or legacy alias.
    pub fn lookup_project(&self, name: &str) -> Option<ProjectId> {
        let s = self.lock();
        if let Ok(id) = ProjectId::parse(name)
            && s.file.projects.contains_key(&id)
        {
            return Some(id);
        }
        s.file
            .projects
            .values()
            .find(|p| p.aliases.iter().any(|a| a == name))
            .map(|p| p.id.clone())
    }
    /// Return one persisted project.
    pub fn project_record(&self, id: &ProjectId) -> Option<ProjectRecord> {
        self.lock().file.projects.get(id).cloned()
    }
    /// Return one persisted instance.
    pub fn instance_record(&self, id: &InstanceId) -> Option<InstanceRecord> {
        self.lock().file.instances.get(id).cloned()
    }
    /// Return the currently active profile name, if any.
    pub fn active_profile(&self) -> Option<String> {
        self.lock().file.active_profile.clone()
    }
    /// Persist the active profile name; profile existence and activation effects belong to the caller.
    pub fn set_active_profile(&self, name: Option<String>) -> Result<(), Error> {
        if let Some(name) = &name
            && !lantern_core::profiles::valid_name(name)
        {
            return Err(Error::InvalidProfile(name.clone()));
        }
        let mut s = self.lock();
        if s.file.active_profile != name {
            s.file.active_profile = name;
            s.dirty();
        }
        Ok(())
    }
    /// Return all persisted checkouts belonging to a repository.
    pub fn checkouts(&self, repo: &Path) -> Vec<(InstanceId, InstanceRecord)> {
        let s = self.lock();
        s.file
            .instances
            .iter()
            .filter(|(_, i)| {
                s.file
                    .projects
                    .get(&i.project)
                    .and_then(|p| p.repo.as_ref())
                    .is_some_and(|r| r.common_dir == repo)
            })
            .map(|(id, i)| (id.clone(), i.clone()))
            .collect()
    }
    /// Register a repository keyed by common directory, retaining its original stable slug.
    pub fn upsert_repo(&self, common_dir: PathBuf, record: RepoRecord) {
        let mut s = self.lock();
        if let std::collections::btree_map::Entry::Vacant(entry) = s.file.repos.entry(common_dir) {
            entry.insert(record);
            s.dirty();
        }
    }
    /// Remove a repository record once no project refers to it.
    pub fn remove_repo(&self, common_dir: &Path) -> Result<(), Error> {
        let mut s = self.lock();
        if s.file
            .projects
            .values()
            .any(|p| p.repo.as_ref().is_some_and(|r| r.common_dir == common_dir))
        {
            return Err(Error::Conflict(common_dir.display().to_string()));
        }
        if s.file.repos.remove(common_dir).is_some() {
            s.dirty();
        }
        Ok(())
    }
    /// Register or replace a project and ensure it has a primary instance.
    pub fn upsert_project(&self, project: ProjectRecord) -> Result<(), Error> {
        ProjectId::parse(project.id.as_str())?;
        let mut s = self.lock();
        for p in s.file.projects.values().filter(|p| p.id != project.id) {
            if p.root == project.root
                || p.id.as_str() == project.id.as_str()
                || p.aliases
                    .iter()
                    .any(|a| a == project.id.as_str() || project.aliases.contains(a))
                || project.aliases.iter().any(|a| a == p.id.as_str())
            {
                return Err(Error::Conflict(project.id.to_string()));
            }
        }
        let id = project.id.clone();
        let primary = InstanceId::primary(&id);
        let root = project.root.clone();
        let original_file = s.file.clone();
        let original_checkouts = s.checkouts.clone();
        let original_manifests = s.manifests.clone();
        if let Some(previous) = s.file.projects.get(&id)
            && previous.root != root
            && let Some(primary_record) = s.file.instances.get_mut(&primary)
        {
            primary_record.checkout.path = root.clone();
        }
        s.file.projects.insert(id.clone(), project);
        s.file
            .instances
            .entry(primary.clone())
            .or_insert_with(|| InstanceRecord {
                project: id.clone(),
                checkout: CheckoutRecord {
                    path: root,
                    kind: CheckoutKind::Primary,
                    phase: CheckoutPhase::Ready,
                    created_at: Timestamp::now(),
                    created_branch: false,
                    materialized: vec![],
                    setup: None,
                    missing_since: None,
                },
                pinned: false,
                adopt: false,
                activated: false,
                first_started: None,
                last_started: None,
                last_runtime: None,
            });
        let ids: Vec<_> = s
            .file
            .instances
            .iter()
            .filter(|(_, i)| i.project == id)
            .map(|(id, _)| id.clone())
            .collect();
        for instance in &ids {
            s.rebuild_checkout(instance);
            if let Err(error) = s.resolve_manifest(instance) {
                s.file = original_file;
                s.checkouts = original_checkouts;
                s.manifests = original_manifests;
                return Err(error);
            }
        }
        s.dirty();
        s.publish_index();
        s.publish_project(&id);
        for instance in ids {
            s.publish_instance(&instance);
        }
        Ok(())
    }
    /// Unregister a project and its instances and release their leases (does not remove files).
    pub fn remove_project(&self, id: &ProjectId) -> Result<(), Error> {
        let mut s = self.lock();
        if s.file.projects.remove(id).is_none() {
            return Err(Error::ProjectNotFound(id.clone()));
        }
        let ids: Vec<_> = s
            .file
            .instances
            .iter()
            .filter(|(_, r)| &r.project == id)
            .map(|(i, _)| i.clone())
            .collect();
        for instance in ids {
            s.remove_instance_inner(&instance);
        }
        s.dirty();
        s.publish_index();
        s.bus.publish(Event::ProjectRemoved { id: id.clone() });
        Ok(())
    }
    /// Rename a stopped project and every instance atomically, preserving aliases and leases.
    pub fn rename_project(
        &self,
        old: &ProjectId,
        new: ProjectId,
    ) -> Result<Vec<(InstanceId, InstanceId)>, Error> {
        ProjectId::parse(new.as_str())?;
        let mut s = self.lock();
        let mut project = s
            .file
            .projects
            .get(old)
            .cloned()
            .ok_or_else(|| Error::ProjectNotFound(old.clone()))?;
        if &new == old {
            return Ok(Vec::new());
        }
        if s.file
            .projects
            .values()
            .any(|p| p.id != *old && (p.id == new || p.aliases.iter().any(|a| a == new.as_str())))
        {
            return Err(Error::Conflict(new.to_string()));
        }
        let ids: Vec<_> = s
            .file
            .instances
            .keys()
            .filter(|id| id.project() == *old)
            .cloned()
            .collect();
        for id in &ids {
            if s.runtime
                .get(id)
                .is_some_and(|r| !matches!(r.status, InstanceStatus::Stopped | InstanceStatus::NeedsConfig))
            {
                return Err(Error::Conflict(format!("{id} is not stopped")));
            }
        }
        let pairs = ids
            .into_iter()
            .map(|id| {
                let renamed = match id.slug() {
                    Some(slug) => InstanceId::derived(&new, slug)?,
                    None => InstanceId::primary(&new),
                };
                Ok((id, renamed))
            })
            .collect::<Result<Vec<_>, IdError>>()?;
        let backup_file = s.file.clone();
        let backup_runtime = s.runtime.clone();
        let backup_checkouts = s.checkouts.clone();
        let backup_manifests = s.manifests.clone();
        let backup_stale = s.setup_stale.clone();
        s.file.projects.remove(old);
        project.id = new.clone();
        if !project.aliases.iter().any(|a| a == old.as_str()) {
            project.aliases.push(old.to_string());
        }
        s.file.projects.insert(new.clone(), project);
        for (from, to) in &pairs {
            let mut record = s.file.instances.remove(from).expect("collected instance");
            record.project = new.clone();
            s.file.instances.insert(to.clone(), record);
            if let Some(value) = s.file.port_leases.remove(from) {
                s.file.port_leases.insert(to.clone(), value);
            }
            if let Some(value) = s.checkouts.remove(from) {
                s.checkouts.insert(to.clone(), value);
            }
            if let Some(value) = s.runtime.remove(from) {
                s.runtime.insert(to.clone(), value);
            }
            s.manifests.remove(from);
            if let Some(value) = s.setup_stale.remove(from) {
                s.setup_stale.insert(to.clone(), value);
            }
            if let Err(error) = s.resolve_manifest(to) {
                s.file = backup_file;
                s.runtime = backup_runtime;
                s.checkouts = backup_checkouts;
                s.manifests = backup_manifests;
                s.setup_stale = backup_stale;
                return Err(error);
            }
        }
        s.dirty();
        s.publish_index();
        s.bus.publish(Event::ProjectRemoved { id: old.clone() });
        for (from, to) in &pairs {
            s.bus.publish(Event::InstanceRemoved { id: from.clone() });
            s.publish_instance(to);
        }
        s.publish_project(&new);
        Ok(pairs)
    }
    /// Insert or replace a project checkout/instance record.
    pub fn upsert_instance(&self, id: InstanceId, record: InstanceRecord) -> Result<(), Error> {
        if id.project() != record.project {
            return Err(Error::WrongProject);
        }
        let mut s = self.lock();
        if !s.file.projects.contains_key(&record.project) {
            return Err(Error::ProjectNotFound(record.project));
        }
        let project = record.project.clone();
        let original_file = s.file.clone();
        let original_checkouts = s.checkouts.clone();
        s.file.instances.insert(id.clone(), record);
        s.rebuild_checkout(&id);
        if let Err(error) = s.resolve_manifest(&id) {
            s.file = original_file;
            s.checkouts = original_checkouts;
            return Err(error);
        }
        s.dirty();
        s.publish_index();
        s.publish_instance(&id);
        s.publish_project(&project);
        Ok(())
    }
    /// Remove a non-primary instance and its lease without deleting any files.
    pub fn remove_instance(&self, id: &InstanceId) -> Result<(), Error> {
        if id.is_primary() {
            return Err(Error::Primary);
        }
        let mut s = self.lock();
        if !s.file.instances.contains_key(id) {
            return Err(Error::InstanceNotFound(id.clone()));
        }
        let project = id.project();
        s.remove_instance_inner(id);
        s.dirty();
        s.publish_index();
        s.publish_project(&project);
        Ok(())
    }
    /// Replace git-derived checkout metadata and publish the new discovery index.
    pub fn update_checkout(&self, id: &InstanceId, checkout: Checkout) -> Result<(), Error> {
        let mut s = self.lock();
        let original_file = s.file.clone();
        let original_checkouts = s.checkouts.clone();
        let record = s
            .file
            .instances
            .get_mut(id)
            .ok_or_else(|| Error::InstanceNotFound(id.clone()))?;
        record.checkout.path = checkout.path.clone();
        record.checkout.kind = checkout.kind;
        s.checkouts.insert(id.clone(), checkout);
        if let Err(error) = s.resolve_manifest(id) {
            s.file = original_file;
            s.checkouts = original_checkouts;
            return Err(error);
        }
        s.dirty();
        s.publish_index();
        s.publish_instance(id);
        Ok(())
    }
    /// Change an instance's pinned intent.
    pub fn set_pinned(&self, id: &InstanceId, pinned: bool) -> Result<(), Error> {
        self.update_instance(id, |r| r.pinned = pinned)
    }
    /// Change persistent adoption intent and refresh the discovery index.
    pub fn set_adopt(&self, id: &InstanceId, adopt: bool) -> Result<(), Error> {
        self.update_instance(id, |r| r.adopt = adopt)
    }
    /// Change the persistent site-only activation flag.
    pub fn set_activated(&self, id: &InstanceId, activated: bool) -> Result<(), Error> {
        self.update_instance(id, |r| r.activated = activated)
    }
    /// Record a successful start, claim the instance and automatically pin it.
    pub fn record_started(&self, id: &InstanceId, at: Timestamp) -> Result<(), Error> {
        self.update_instance(id, |r| {
            r.first_started.get_or_insert(at);
            r.last_started = Some(at);
            r.pinned = true;
        })
    }
    /// Apply a persisted instance mutation, then publish its snapshots.
    pub fn update_instance(&self, id: &InstanceId, f: impl FnOnce(&mut InstanceRecord)) -> Result<(), Error> {
        let mut s = self.lock();
        let original_file = s.file.clone();
        let original_checkouts = s.checkouts.clone();
        let record = s
            .file
            .instances
            .get_mut(id)
            .ok_or_else(|| Error::InstanceNotFound(id.clone()))?;
        f(record);
        if record.project != id.project() {
            s.file = original_file;
            return Err(Error::WrongProject);
        }
        s.rebuild_checkout(id);
        if let Err(error) = s.resolve_manifest(id) {
            s.file = original_file;
            s.checkouts = original_checkouts;
            return Err(error);
        }
        s.dirty();
        s.publish_index();
        s.publish_instance(id);
        s.publish_project(&id.project());
        Ok(())
    }
    /// Record a successful preparation and its materialized files.
    pub fn record_prepared(
        &self,
        id: &InstanceId,
        stamp: SetupStamp,
        materialized: Vec<PathBuf>,
    ) -> Result<(), Error> {
        self.update_instance(id, |record| {
            record.checkout.setup = Some(stamp);
            record.checkout.materialized = materialized;
            record.checkout.phase = CheckoutPhase::Ready;
        })?;
        self.set_setup_stale(id, false)
    }
    /// Set the derived, non-persisted setup staleness supplied by the instances lane.
    pub fn set_setup_stale(&self, id: &InstanceId, stale: bool) -> Result<(), Error> {
        let mut s = self.lock();
        if !s.file.instances.contains_key(id) {
            return Err(Error::InstanceNotFound(id.clone()));
        }
        if s.setup_stale.insert(id.clone(), stale) != Some(stale) {
            s.publish_instance(id);
        }
        Ok(())
    }
    /// Rename a stopped non-primary instance, retaining its record and lease.
    pub fn rename_instance(&self, old: &InstanceId, new: InstanceId) -> Result<(), Error> {
        if old.is_primary() || new.is_primary() || old.project() != new.project() {
            return Err(Error::WrongProject);
        }
        let mut s = self.lock();
        if !s.file.instances.contains_key(old) {
            return Err(Error::InstanceNotFound(old.clone()));
        }
        if s.file.instances.contains_key(&new) {
            return Err(Error::Conflict(new.to_string()));
        }
        if s.runtime
            .get(old)
            .is_some_and(|r| r.status != InstanceStatus::Stopped)
        {
            return Err(Error::Conflict(format!("{old} is not stopped")));
        }
        let record = s.file.instances.remove(old).expect("checked instance");
        s.file.instances.insert(new.clone(), record);
        if let Some(value) = s.file.port_leases.remove(old) {
            s.file.port_leases.insert(new.clone(), value);
        }
        if let Some(value) = s.checkouts.remove(old) {
            s.checkouts.insert(new.clone(), value);
        }
        if let Some(value) = s.runtime.remove(old) {
            s.runtime.insert(new.clone(), value);
        }
        if let Some(value) = s.manifests.remove(old) {
            s.manifests.insert(new.clone(), value);
        }
        if let Some(value) = s.setup_stale.remove(old) {
            s.setup_stale.insert(new.clone(), value);
        }
        s.dirty();
        s.publish_index();
        s.bus.publish(Event::InstanceRemoved { id: old.clone() });
        s.publish_instance(&new);
        s.publish_project(&new.project());
        Ok(())
    }
    /// Atomically update runtime state and persist only the reconciliation hint.
    pub fn update_runtime(&self, id: &InstanceId, f: impl FnOnce(&mut RuntimeState)) -> Result<(), Error> {
        let mut s = self.lock();
        if !s.file.instances.contains_key(id) {
            return Err(Error::InstanceNotFound(id.clone()));
        }
        let runtime = s.runtime.entry(id.clone()).or_default();
        let before = runtime.clone();
        f(runtime);
        let after = runtime.clone();
        if before == after {
            return Ok(());
        }
        let hint = LastRuntime {
            status: after.status,
            port: after.port,
            unit: after.unit.clone(),
            main_pid: after.main_pid,
            started_at: after.started_at,
            last_exit: after.last_exit.clone(),
            restarts: after.restarts,
            message: after.message.clone(),
        };
        let record = s.file.instances.get_mut(id).expect("checked instance");
        if record.last_runtime.as_ref() != Some(&hint) {
            record.last_runtime = Some(hint);
            s.dirty();
        }
        let health_changed = before.health.state != after.health.state;
        let mut old = before;
        old.health = after.health.clone();
        if old != after || health_changed {
            s.publish_instance(id);
            s.publish_project(&id.project());
        }
        Ok(())
    }
    /// Apply discovery's settled unmanaged-listener change, leaving managed states untouched.
    pub fn apply_external(&self, change: ExternalChange) -> Result<(), Error> {
        let (id, external) = match change {
            ExternalChange::Appeared { instance, external }
            | ExternalChange::Updated { instance, external } => (instance, Some(external)),
            ExternalChange::Gone { instance } => (instance, None),
        };
        self.update_runtime(&id, move |runtime| {
            if let Some(external) = external {
                if matches!(
                    runtime.status,
                    InstanceStatus::Stopped
                        | InstanceStatus::Crashed
                        | InstanceStatus::NeedsConfig
                        | InstanceStatus::External
                ) {
                    runtime.status = InstanceStatus::External;
                    runtime.port = Some(external.port);
                    runtime.main_pid = external.pid;
                    runtime.upstream = Some(external.upstream);
                    runtime.message = Some("started outside Lantern".into());
                    runtime.issue = None;
                    runtime.external = Some(external);
                }
            } else if runtime.status == InstanceStatus::External {
                runtime.status = InstanceStatus::Stopped;
                runtime.port = None;
                runtime.main_pid = None;
                runtime.upstream = None;
                runtime.message = None;
                runtime.external = None;
            }
        })
    }
    /// Run a transactional sticky-port allocation against the registry-owned lease table.
    pub fn update_port_leases<T, E>(&self, f: impl FnOnce(&mut PortLeases) -> Result<T, E>) -> Result<T, E> {
        let mut s = self.lock();
        let mut leases = PortLeases::from_table(
            s.file
                .port_leases
                .iter()
                .map(|(id, l)| (id.clone(), l.port))
                .collect(),
        );
        let result = f(&mut leases)?;
        let now = Timestamp::now();
        let next = leases
            .table()
            .iter()
            .map(|(id, port)| {
                (
                    id.clone(),
                    s.file
                        .port_leases
                        .get(id)
                        .filter(|l| l.port == *port)
                        .cloned()
                        .unwrap_or(PortLease {
                            port: *port,
                            leased_at: now,
                        }),
                )
            })
            .collect();
        if s.file.port_leases != next {
            s.file.port_leases = next;
            s.dirty();
        }
        Ok(result)
    }
    /// Re-resolve every checkout for a project after a manifest or override change.
    pub fn reload_project(&self, id: &ProjectId) -> Result<(), Error> {
        let mut s = self.lock();
        if !s.file.projects.contains_key(id) {
            return Err(Error::ProjectNotFound(id.clone()));
        }
        let ids: Vec<_> = s
            .file
            .instances
            .iter()
            .filter(|(_, r)| &r.project == id)
            .map(|(id, _)| id.clone())
            .collect();
        let original_manifests = s.manifests.clone();
        for instance in &ids {
            if let Err(error) = s.resolve_manifest(instance) {
                s.manifests = original_manifests;
                return Err(error);
            }
        }
        s.publish_project(id);
        for instance in ids {
            s.publish_instance(&instance);
        }
        Ok(())
    }
    /// Return the full cached resolved manifest and its stable revision hash.
    pub fn manifest(&self, id: &InstanceId) -> Option<(Arc<ResolvedManifest>, String)> {
        self.lock()
            .manifests
            .get(id)
            .map(|m| (m.value.clone(), m.rev.clone()))
    }
    /// Return a full in-process instance for the supervisor, including its cached manifest.
    pub fn instance(&self, id: &InstanceId) -> Option<Instance> {
        let s = self.lock();
        let view = s.instance_view(id)?;
        let manifest = s.manifests.get(id)?.value.clone();
        Some(Instance {
            id: view.id,
            project: view.project,
            checkout: view.checkout,
            domain: view.domain,
            manifest,
            runtime: view.runtime,
            pinned: view.pinned,
            adopt: view.adopt,
            setup_stale: view.setup_stale,
        })
    }
    /// Return a full in-process project record for consumers.
    pub fn project(&self, id: &ProjectId) -> Option<Project> {
        self.lock().project_summary(id).map(|s| s.project)
    }
    /// Return all instance views for an SSE snapshot.
    pub fn instances(&self) -> Vec<InstanceView> {
        let s = self.lock();
        s.file
            .instances
            .keys()
            .filter_map(|id| s.instance_view(id))
            .collect()
    }
    /// Return all project summaries for an SSE snapshot, including missing records.
    pub fn projects(&self) -> Vec<ProjectSummary> {
        let s = self.lock();
        s.file
            .projects
            .keys()
            .filter_map(|id| s.project_summary(id))
            .collect()
    }
    /// Return the current listener sequence and visible snapshot.
    pub fn listeners(&self) -> (u64, Vec<Listener>) {
        let s = self.lock();
        (s.listener_seq, s.listeners.clone())
    }
    /// Replace the visible listener snapshot and publish if it changed.
    pub fn set_listeners(&self, seq: u64, listeners: Vec<Listener>) {
        let mut s = self.lock();
        if s.listener_seq != seq || s.listeners != listeners {
            s.listener_seq = seq;
            s.listeners = listeners.clone();
            s.bus.publish(Event::ListenersChanged { seq, listeners });
        }
    }
    /// Return the current service snapshot.
    pub fn services(&self) -> Vec<ServiceStatus> {
        self.lock().services.clone()
    }
    /// Replace one service snapshot and publish it when changed.
    pub fn set_service(&self, service: ServiceStatus) {
        let mut s = self.lock();
        if let Some(old) = s.services.iter_mut().find(|v| v.name == service.name) {
            if *old == service {
                return;
            }
            *old = service.clone();
        } else {
            s.services.push(service.clone());
        }
        s.bus.publish(Event::ServiceUpdated { service });
    }
    /// Return the current in-memory suggestions.
    pub fn suggestions(&self) -> Vec<Suggestion> {
        self.lock().suggestions.clone()
    }
}

impl State {
    fn dirty(&self) {
        self.io.mark_dirty(self.file.clone());
    }
    fn remove_instance_inner(&mut self, id: &InstanceId) {
        self.file.instances.remove(id);
        self.file.port_leases.remove(id);
        self.runtime.remove(id);
        self.checkouts.remove(id);
        self.setup_stale.remove(id);
        self.manifests.remove(id);
        self.bus.publish(Event::InstanceRemoved { id: id.clone() });
    }
    fn rebuild_checkout(&mut self, id: &InstanceId) {
        if let Some(record) = self.file.instances.get(id) {
            let old = self.checkouts.get(id);
            self.checkouts.insert(
                id.clone(),
                Checkout {
                    path: record.checkout.path.clone(),
                    git_root: old.and_then(|c| c.git_root.clone()),
                    kind: record.checkout.kind,
                    branch: old.and_then(|c| c.branch.clone()),
                    head: old.and_then(|c| c.head.clone()),
                    dirty: old.and_then(|c| c.dirty),
                    locked: old.and_then(|c| c.locked.clone()),
                    missing: record.checkout.missing_since.is_some()
                        || self
                            .file
                            .projects
                            .get(&record.project)
                            .is_some_and(|p| p.missing && id.is_primary()),
                },
            );
        }
    }
    fn resolve_manifest(&mut self, id: &InstanceId) -> Result<(), Error> {
        let record = self
            .file
            .instances
            .get(id)
            .ok_or_else(|| Error::InstanceNotFound(id.clone()))?;
        let project = self
            .file
            .projects
            .get(&record.project)
            .ok_or_else(|| Error::ProjectNotFound(record.project.clone()))?;
        let primary = manifest::load(&project.root)?;
        let checkout = if id.is_primary() {
            None
        } else {
            manifest::load(&record.checkout.path)?
        };
        let detection = detect::detect_dir(&record.checkout.path);
        let mut resolved = manifest::resolve(ResolveInput {
            dir_name: project.id.as_str(),
            is_primary: id.is_primary(),
            primary: primary.as_ref(),
            checkout: checkout.as_ref(),
            overrides: &project.overrides,
            templates: &self.templates,
            detection: &detection,
            settings: &self.settings,
        });
        if let Some(primary) = &primary
            && let Some(new_id) = primary.raw.id.as_ref()
            && new_id != project.id.as_str()
            && !project.aliases.iter().any(|alias| alias == new_id)
        {
            resolved.diagnostics.push(Diagnostic {
                severity: Severity::Warning,
                code: "id_changed".into(),
                path: "id".into(),
                file: primary.file.clone(),
                line: None,
                column: None,
                message: format!("registered id {} differs from manifest id {new_id}", project.id),
                expected: Some(project.id.to_string()),
                help: Some("Rename the project explicitly to change its registered id.".into()),
            });
        }
        // Project identity remains stable even if a primary file was edited or overrides omit id.
        resolved.id = project.id.clone();
        let bytes = serde_json::to_vec(&resolved).expect("resolved manifest serializes");
        let mut hash = 0xcbf29ce484222325_u64;
        for byte in bytes {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
        self.manifests.insert(
            id.clone(),
            CachedManifest {
                value: Arc::new(resolved),
                rev: format!("{hash:016x}"),
            },
        );
        Ok(())
    }
    fn publish_index(&self) {
        let refs = self
            .checkouts
            .iter()
            .filter(|(_, c)| !c.missing)
            .filter_map(|(id, c)| {
                self.file.instances.get(id).map(|i| CheckoutRef {
                    instance: id.clone(),
                    project: i.project.clone(),
                    kind: c.kind,
                    branch: c.branch.clone(),
                    path: c.path.clone(),
                })
            });
        let mut index = CheckoutIndex::new(refs, self.home.clone());
        index.any_adopted = self.file.instances.values().any(|i| i.adopt);
        index.workspace_roots = self
            .settings
            .workspaces
            .roots
            .iter()
            .map(|root| expand_home(root, &self.home))
            .collect();
        self.index.send_replace(index);
    }
    fn instance_view(&self, id: &InstanceId) -> Option<InstanceView> {
        let r = self.file.instances.get(id)?;
        let m = self.manifests.get(id)?;
        let checkout = self.checkouts.get(id)?.clone();
        let mut runtime = self.runtime.get(id).cloned().unwrap_or_default();
        if checkout.missing && runtime.status == InstanceStatus::Stopped {
            runtime.status = InstanceStatus::NeedsConfig;
            runtime.message = Some("checkout path not found".into());
        }
        let domain = if id.is_primary() {
            m.value
                .domain
                .clone()
                .unwrap_or_else(|| format!("{}.{}", r.project, self.settings.domains.tld))
        } else {
            format!("{}.{}.{}", id.slug()?, r.project, self.settings.domains.tld)
        };
        Some(InstanceView {
            id: id.clone(),
            project: r.project.clone(),
            checkout,
            url: format!("https://{domain}"),
            domain,
            pinned: r.pinned,
            adopt: r.adopt,
            visible: id.is_primary()
                || r.pinned
                || self.runtime.get(id).is_some_and(|r| {
                    !matches!(r.status, InstanceStatus::Stopped | InstanceStatus::NeedsConfig)
                }),
            setup_stale: self.setup_stale.get(id).copied().unwrap_or(false),
            runtime,
            manifest: m.value.summary(m.rev.clone()),
        })
    }
    fn project_summary(&self, id: &ProjectId) -> Option<ProjectSummary> {
        let r = self.file.projects.get(id)?;
        let primary = InstanceId::primary(id);
        let instance = self.instance_view(&primary)?;
        let m = &self.manifests.get(&primary)?.value;
        let repo = r.repo.as_ref().map(|repo| {
            let mut root = r.root.clone();
            for _ in repo.subpath.components() {
                root.pop();
            }
            ProjectRepo {
                common_dir: repo.common_dir.clone(),
                git_root: root,
                subpath: repo.subpath.clone(),
            }
        });
        let risk = m.routing.risk.as_deref().map(|r| match r {
            "low" => Risk::Low,
            "medium" => Risk::Medium,
            "high" => Risk::High,
            "critical" => Risk::Critical,
            _ => Risk::Unknown,
        });
        Some(ProjectSummary {
            project: Project {
                id: id.clone(),
                aliases: r.aliases.clone(),
                name: m.name.clone(),
                root: r.root.clone(),
                repo,
                kind: m.kind,
                enabled: m.enabled,
                tags: m.tags.clone(),
                registered_at: r.registered_at,
                source: r.source,
            },
            primary,
            status: instance.runtime.status,
            url: instance.url,
            port: instance.runtime.port,
            instance_count: self.file.instances.values().filter(|i| &i.project == id).count() as u32,
            docs_count: m.docs.len() as u32,
            endpoints_count: m.endpoints.len() as u32,
            routing: Some(RoutingMeta {
                triggers: m.routing.triggers.clone(),
                risk,
                requires_confirmation: m.routing.requires_confirmation,
                max_concurrent: Some(m.routing.max_concurrent),
                agents: m.routing.agents.clone(),
            }),
        })
    }
    fn publish_instance(&self, id: &InstanceId) {
        if let Some(instance) = self.instance_view(id) {
            self.bus.publish(Event::InstanceUpdated { instance });
        }
    }
    fn publish_project(&self, id: &ProjectId) {
        if let Some(project) = self.project_summary(id) {
            self.bus.publish(Event::ProjectUpdated { project });
        }
    }
}

fn expand_home(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    }
}
