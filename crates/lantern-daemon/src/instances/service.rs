//! Checkout reconciliation, branch planning and the preparation pipeline.
use super::{InstanceError, InstanceStore, OneShot, OneShotSpec, ProjectConfig, deps, files};
use crate::{
    git::{AddMode, Git, RepoHandle, Worktree},
    operations::OpHandle,
};
use lantern_core::{
    Timestamp,
    ids::{BRANCH_SLUG_MAX, InstanceId, ProjectId, branch_slug, detached_slug, instance_domain},
    manifest::ResolvedManifest,
    model::{CheckoutKind, InstanceStatus, Stream},
    state::{CheckoutPhase, CheckoutRecord, InstanceRecord, SetupStamp},
    template::VarContext,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Semaphore};

/// User-selected branch target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchTarget {
    /// Existing local or remote branch.
    Branch(String),
    /// Explicit new branch and optional base.
    NewBranch {
        /// Name for the new branch.
        name: String,
        /// Commit-ish base, or default branch when absent.
        base: Option<String>,
    },
    /// Detached commit-ish.
    Detached(String),
    /// GitHub-style pull ref fetched from the selected remote.
    Pr(u32),
}
/// Fetch policy for branch discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchPolicy {
    /// Fetch only when a requested branch was not found.
    Auto,
    /// Refresh remote refs before resolution.
    Always,
    /// Never fetch except a requested PR.
    Never,
}
/// Trust evidence supplied by the caller before executing PR code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrTrust {
    /// The caller verified that the PR's head is in the base repository.
    SameRepository,
    /// The user explicitly confirmed running foreign PR code.
    ConfirmedForeign,
}
/// Request to reuse or create a branch checkout.
#[derive(Debug, Clone)]
pub struct RunBranchRequest {
    /// Registered project.
    pub project: ProjectId,
    /// Branch, new branch, revision, or PR.
    pub target: BranchTarget,
    /// Ask the controller to hand off to runtime start.
    pub start: bool,
    /// Pin the requested project's checkout.
    pub pin: bool,
    /// Remote fetch behavior.
    pub fetch: FetchPolicy,
    /// For an existing branch, create a detached checkout instead of reuse.
    pub detach: bool,
    /// Explicit remote selection, useful for ambiguity and PRs.
    pub remote: Option<String>,
    /// Verified PR origin or explicit user confirmation; required for PR targets.
    pub pr_trust: Option<PrTrust>,
}
/// Result of resolving a run-branch request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Existing primary checkout.
    ReusedPrimary,
    /// Existing non-primary checkout.
    ReusedCheckout,
    /// Newly created managed worktree.
    CreatedWorktree(AddMode),
}
/// Result and runtime hand-off requested by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunBranchResponse {
    /// Actual instance identity, possibly a primary or frozen identity.
    pub instance: InstanceId,
    /// Resulting domain.
    pub domain: String,
    /// How the checkout was resolved.
    pub outcome: Outcome,
    /// Controller should start this prepared instance.
    pub start: bool,
}
/// Prepared manifest and extra environment for the runtime start.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// Checkout-resolved manifest.
    pub manifest: Arc<ResolvedManifest>,
    /// Expanded non-primary worktree environment.
    pub worktree_env: BTreeMap<String, String>,
}
/// Checkout observation associated with a persisted instance.
#[derive(Debug, Clone)]
pub struct CheckoutObservation {
    /// Actual worktree data from git.
    pub worktree: Worktree,
    /// Project directory inside the worktree.
    pub project_path: PathBuf,
}
#[derive(Debug, Default)]
struct RefreshState {
    fp: Option<u64>,
    ticks: u64,
    last: Option<Instant>,
}

/// Instance service, with injected store, one-shot runtime, and managed directory.
pub struct InstanceService {
    pub(crate) git: Git,
    pub(crate) store: Arc<dyn InstanceStore>,
    pub(crate) runner: Arc<dyn OneShot>,
    pub(crate) worktrees_dir: PathBuf,
    pub(crate) timeout: Duration,
    missing_retention_days: u64,
    pub(crate) setup_slots: Semaphore,
    pub(crate) mutation: Mutex<()>,
    observations: StdMutex<HashMap<InstanceId, CheckoutObservation>>,
    prepare_locks: StdMutex<HashMap<InstanceId, Arc<Mutex<()>>>>,
    refresh: StdMutex<HashMap<PathBuf, RefreshState>>,
}
impl std::fmt::Debug for InstanceService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstanceService")
            .field("worktrees_dir", &self.worktrees_dir)
            .finish_non_exhaustive()
    }
}

impl InstanceService {
    /// Construct a service with an explicit managed directory and setup limits.
    pub fn new(
        git: Git,
        store: Arc<dyn InstanceStore>,
        runner: Arc<dyn OneShot>,
        worktrees_dir: PathBuf,
        max_concurrent_setups: usize,
        timeout: Duration,
    ) -> Self {
        Self {
            git,
            store,
            runner,
            worktrees_dir,
            timeout,
            missing_retention_days: 30,
            setup_slots: Semaphore::new(max_concurrent_setups.max(1)),
            mutation: Mutex::new(()),
            observations: StdMutex::new(HashMap::new()),
            prepare_locks: StdMutex::new(HashMap::new()),
            refresh: StdMutex::new(HashMap::new()),
        }
    }
    /// Override the default 30-day retention for unpinned missing records.
    pub fn with_missing_retention_days(mut self, days: u64) -> Self {
        self.missing_retention_days = days;
        self
    }
    pub(crate) fn project(&self, id: &ProjectId) -> Result<ProjectConfig, InstanceError> {
        self.store
            .project(id)
            .map_err(InstanceError::Adapter)?
            .ok_or_else(|| InstanceError::NotFound(id.to_string()))
    }
    pub(crate) fn record(&self, id: &InstanceId) -> Result<InstanceRecord, InstanceError> {
        self.store
            .records()
            .map_err(InstanceError::Adapter)?
            .into_iter()
            .find(|(i, _)| i == id)
            .map(|(_, r)| r)
            .ok_or_else(|| InstanceError::NotFound(id.to_string()))
    }
    pub(crate) fn repo(&self, p: &ProjectConfig) -> Result<RepoHandle, InstanceError> {
        Ok(self.git.repo(
            p.project
                .repo
                .as_ref()
                .ok_or(InstanceError::NotARepo)?
                .common_dir
                .clone(),
        ))
    }
    /// Choose five seconds with listeners or thirty seconds when idle.
    pub fn refresh_interval(has_listeners: bool) -> Duration {
        Duration::from_secs(if has_listeners { 5 } else { 30 })
    }
    /// Poll fingerprint and refresh on change, on force, or every twelfth tick.
    pub async fn tick(&self, common_dir: &Path, force: bool) -> Result<bool, InstanceError> {
        let mut known = Vec::new();
        for (_, record) in self.store.records().map_err(InstanceError::Adapter)? {
            if let Some(project) = self
                .store
                .project(&record.project)
                .map_err(InstanceError::Adapter)?
                && let Some(repo) = project.project.repo.as_ref()
                && repo.common_dir == common_dir
                && let Ok(root) = strip_subpath(&record.checkout.path, &repo.subpath)
            {
                known.push(root);
            }
        }
        known.sort();
        known.dedup();
        let fp = self.git.repo(common_dir.to_path_buf()).fingerprint(&known);
        let should = {
            let mut all = self.refresh.lock().expect("refresh lock");
            let state = all.entry(common_dir.to_path_buf()).or_default();
            state.ticks += 1;
            force || fp.is_none() || state.fp != fp || state.ticks.is_multiple_of(12)
        };
        if should {
            self.refresh_repo(common_dir).await?;
            let mut all = self.refresh.lock().expect("refresh lock");
            let state = all.entry(common_dir.to_path_buf()).or_default();
            state.fp = fp;
            state.last = Some(Instant::now());
        } else {
            self.publish_setup_staleness(common_dir)?;
        }
        Ok(should)
    }
    /// Run the polling policy until cancelled; callers provide current repos and listener demand.
    pub async fn watch_repos(
        &self,
        mut repos: tokio::sync::watch::Receiver<Vec<PathBuf>>,
        mut listeners: tokio::sync::watch::Receiver<bool>,
        cancel: tokio_util::sync::CancellationToken,
    ) {
        loop {
            let current = repos.borrow().clone();
            for common in current {
                if cancel.is_cancelled() {
                    return;
                }
                if let Err(error) = self.tick(&common, false).await {
                    tracing::warn!(path = %common.display(), %error, "checkout refresh failed");
                }
            }
            let interval = Self::refresh_interval(*listeners.borrow());
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(interval) => {},
                result = repos.changed() => if result.is_err() { return; },
                result = listeners.changed() => if result.is_err() { return; },
            }
        }
    }
    /// Discovery hint for an unknown checkout; repeated hints are limited to one per second.
    pub async fn hint_path(&self, path: &Path) -> Result<(), InstanceError> {
        let Some(probe) = self.git.probe(path).await? else {
            return Ok(());
        };
        let should_refresh = {
            let mut states = self.refresh.lock().expect("refresh lock");
            let state = states.entry(probe.common_dir.clone()).or_default();
            if state
                .last
                .is_some_and(|time| time.elapsed() < Duration::from_secs(1))
            {
                false
            } else {
                state.last = Some(Instant::now());
                true
            }
        };
        if should_refresh {
            self.refresh_repo(&probe.common_dir).await?;
        }
        Ok(())
    }
    /// Refresh a repo immediately (use before destructive or start operations).
    pub async fn refresh_repo(&self, common_dir: &Path) -> Result<(), InstanceError> {
        let _guard = self.mutation.lock().await;
        self.refresh_unlocked(common_dir).await
    }
    pub(crate) async fn refresh_unlocked(&self, common: &Path) -> Result<(), InstanceError> {
        let repo = self.git.repo(common.to_path_buf());
        let mut trees = repo.list_worktrees().await?;
        self.recover_creating(common, &trees)?;
        if self.prune_missing_managed(common, &repo, &trees).await? {
            trees = repo.list_worktrees().await?;
        }
        let mut projects = self
            .store
            .projects_in_repo(common)
            .map_err(InstanceError::Adapter)?;
        projects.sort_by(|a, b| a.project.id.cmp(&b.project.id));
        // Only a persisted Lantern-created record proves management; directory location alone
        // cannot justify deleting a user-created worktree. Sibling projects share that provenance.
        let mut managed_roots = HashSet::new();
        for (_, record) in self.store.records().map_err(InstanceError::Adapter)? {
            if record.checkout.kind != CheckoutKind::Managed {
                continue;
            }
            if let Some(project) = self
                .store
                .project(&record.project)
                .map_err(InstanceError::Adapter)?
                && let Some(repo) = project.project.repo.as_ref()
                && repo.common_dir == common
                && let Ok(root) = strip_subpath(&record.checkout.path, &repo.subpath)
            {
                managed_roots.insert(root);
            }
        }
        let mut observed = HashSet::new();
        trees.sort_by(|a, b| a.path.cmp(&b.path));
        for p in projects {
            let pr = p.project.repo.as_ref().ok_or(InstanceError::NotARepo)?;
            if pr
                .subpath
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err(InstanceError::UnsafePath("invalid project subpath".into()));
            }
            let mut ordered = trees.clone();
            ordered.sort_by_key(|w| (w.path != pr.git_root, w.path.clone()));
            for wt in ordered.iter().filter(|w| !w.bare) {
                let path = wt.path.join(&pr.subpath);
                if !path.is_dir() {
                    continue;
                }
                let all = self.store.records().map_err(InstanceError::Adapter)?;
                let existing = all
                    .iter()
                    .find(|(_, r)| r.project == p.project.id && r.checkout.path == path)
                    .cloned();
                let is_new = existing.is_none();
                let kind = if wt.path == pr.git_root {
                    CheckoutKind::Primary
                } else {
                    existing
                        .as_ref()
                        .map(|(_, r)| r.checkout.kind)
                        .unwrap_or_else(|| {
                            if managed_roots.contains(&wt.path) {
                                CheckoutKind::Managed
                            } else {
                                CheckoutKind::Worktree
                            }
                        })
                };
                let desired = if kind == CheckoutKind::Primary {
                    None
                } else {
                    Some(
                        wt.branch
                            .as_deref()
                            .map(branch_slug)
                            .or_else(|| wt.head.as_deref().map(detached_slug))
                            .unwrap_or_else(|| branch_slug(&path.to_string_lossy())),
                    )
                };
                let (id, mut record) = if let Some((id, record)) = existing {
                    if !record.claimed() && kind != CheckoutKind::Primary && id.slug() != desired.as_deref() {
                        let (new, revived) =
                            self.allocate(&p, desired.as_deref().unwrap_or("checkout"), Some(&id), false)?;
                        if new != id {
                            self.observations.lock().expect("observations lock").remove(&id);
                            if revived.is_some() {
                                self.store.remove(&id).map_err(InstanceError::Adapter)?;
                            } else {
                                self.store.rename(&id, &new).map_err(InstanceError::Adapter)?;
                            }
                        }
                        (new, revived.unwrap_or(record))
                    } else {
                        (id, record)
                    }
                } else if kind == CheckoutKind::Primary {
                    (
                        InstanceId::primary(&p.project.id),
                        new_record(&p.project.id, path.clone(), kind, false, false),
                    )
                } else {
                    let (id, revived) =
                        self.allocate(&p, desired.as_deref().unwrap_or("checkout"), None, false)?;
                    (
                        id,
                        revived
                            .unwrap_or_else(|| new_record(&p.project.id, path.clone(), kind, false, false)),
                    )
                };
                record.checkout.path = path.clone();
                record.checkout.kind = kind;
                record.checkout.missing_since = None;
                self.store
                    .upsert(&id, record.clone())
                    .map_err(InstanceError::Adapter)?;
                if is_new && kind == CheckoutKind::Primary {
                    // A registered primary is assumed prepared; no installation runs on registration.
                    if let Ok(manifest) = self.store.manifest(&id)
                        && manifest.usable
                        && let (Ok(lock_hash), Ok(commands)) = (
                            deps::lock_hash(&path, &wt.path),
                            setup_commands(&manifest, &path, &wt.path),
                        )
                    {
                        record.checkout.setup = Some(SetupStamp {
                            lock_hash,
                            commands_hash: deps::commands_hash(&commands),
                            finished_at: Timestamp::now(),
                        });
                        self.store.upsert(&id, record).map_err(InstanceError::Adapter)?;
                    }
                }
                observed.insert(id.clone());
                self.observations.lock().expect("observations lock").insert(
                    id,
                    CheckoutObservation {
                        worktree: wt.clone(),
                        project_path: path,
                    },
                );
            }
        }
        for (id, mut record) in self.store.records().map_err(InstanceError::Adapter)? {
            let Some(p) = self
                .store
                .project(&record.project)
                .map_err(InstanceError::Adapter)?
            else {
                continue;
            };
            if p.project.repo.as_ref().is_some_and(|r| r.common_dir == common) && !observed.contains(&id) {
                if record.checkout.missing_since.is_none() {
                    record.checkout.missing_since = Some(Timestamp::now());
                    self.store.upsert(&id, record).map_err(InstanceError::Adapter)?;
                }
                self.observations.lock().expect("observations lock").remove(&id);
            }
        }
        self.purge_missing_unlocked(common)?;
        self.publish_setup_staleness(common)?;
        Ok(())
    }
    fn publish_setup_staleness(&self, common: &Path) -> Result<(), InstanceError> {
        for (id, record) in self.store.records().map_err(InstanceError::Adapter)? {
            let Some(project) = self
                .store
                .project(&record.project)
                .map_err(InstanceError::Adapter)?
            else {
                continue;
            };
            if !project
                .project
                .repo
                .as_ref()
                .is_some_and(|repo| repo.common_dir == common)
                || record.checkout.phase != CheckoutPhase::Ready
                || !record.checkout.path.is_dir()
            {
                continue;
            }
            match self.setup_stale(&id) {
                Ok(stale) => self
                    .store
                    .set_setup_stale(&id, stale)
                    .map_err(InstanceError::Adapter)?,
                Err(error) => tracing::warn!(%id, %error, "could not calculate setup staleness"),
            }
        }
        Ok(())
    }
    async fn prune_missing_managed(
        &self,
        common: &Path,
        repo: &RepoHandle,
        trees: &[Worktree],
    ) -> Result<bool, InstanceError> {
        let mut by_root: HashMap<PathBuf, Vec<(InstanceId, CheckoutKind, CheckoutPhase)>> = HashMap::new();
        for (id, record) in self.store.records().map_err(InstanceError::Adapter)? {
            let Some(project) = self
                .store
                .project(&record.project)
                .map_err(InstanceError::Adapter)?
            else {
                continue;
            };
            let Some(info) = project
                .project
                .repo
                .as_ref()
                .filter(|info| info.common_dir == common)
            else {
                continue;
            };
            let root = strip_subpath(&record.checkout.path, &info.subpath)?;
            by_root
                .entry(root)
                .or_default()
                .push((id, record.checkout.kind, record.checkout.phase));
        }
        let mut changed = false;
        for tree in trees
            .iter()
            .filter(|tree| tree.prunable.is_some() && tree.locked.is_none())
        {
            let Some(records) = by_root.get(&tree.path) else {
                continue;
            };
            if records
                .iter()
                .any(|(_, kind, phase)| *kind != CheckoutKind::Managed || *phase != CheckoutPhase::Ready)
            {
                continue;
            }
            if records.iter().any(|(id, _, _)| {
                !matches!(
                    self.store.status(id),
                    Ok(InstanceStatus::Stopped | InstanceStatus::NeedsConfig)
                )
            }) {
                continue;
            }
            if !matches!(std::fs::symlink_metadata(&tree.path), Err(ref e) if e.kind() == std::io::ErrorKind::NotFound)
            {
                continue;
            }
            // Unlike global `worktree prune`, remove targets only this missing entry;
            // do not use --force, and never act on user-owned or locked siblings.
            repo.remove_worktree(&tree.path, false).await?;
            for (id, _, _) in records {
                self.store.remove(id).map_err(InstanceError::Adapter)?;
            }
            changed = true;
        }
        Ok(changed)
    }
    fn recover_creating(&self, common: &Path, trees: &[Worktree]) -> Result<(), InstanceError> {
        for (id, mut record) in self.store.records().map_err(InstanceError::Adapter)? {
            if record.checkout.phase != CheckoutPhase::Creating {
                continue;
            }
            let Some(project) = self
                .store
                .project(&record.project)
                .map_err(InstanceError::Adapter)?
            else {
                continue;
            };
            let Some(repo) = project
                .project
                .repo
                .as_ref()
                .filter(|repo| repo.common_dir == common)
            else {
                continue;
            };
            let root = strip_subpath(&record.checkout.path, &repo.subpath)?;
            if record.checkout.path.is_dir() && trees.iter().any(|tree| !tree.bare && tree.path == root) {
                // Creating records predate proof that Git made the path. Keep the usable
                // checkout, but do not grant the right to delete it as managed data.
                record.checkout.phase = CheckoutPhase::Ready;
                record.checkout.kind = CheckoutKind::Worktree;
                record.checkout.missing_since = None;
                self.store.upsert(&id, record).map_err(InstanceError::Adapter)?;
                tracing::warn!(%id, path = %root.display(), "recovered checkout without managed ownership proof");
            } else {
                // The path can be absent, half-created, or a raced-in user directory.
                // Dropping only the intent is safe in all three cases.
                self.store.remove(&id).map_err(InstanceError::Adapter)?;
                if root.exists() {
                    tracing::warn!(%id, path = %root.display(), "preserved ambiguous interrupted checkout directory");
                }
            }
        }
        Ok(())
    }
    fn purge_missing_unlocked(&self, common: &Path) -> Result<(), InstanceError> {
        let now = Timestamp::now().as_second();
        let age = self
            .missing_retention_days
            .saturating_mul(86_400)
            .min(i64::MAX as u64) as i64;
        for (id, record) in self.store.records().map_err(InstanceError::Adapter)? {
            let Some(since) = record.checkout.missing_since else {
                continue;
            };
            if record.checkout.kind == CheckoutKind::Primary
                || record.pinned
                || now.saturating_sub(since.as_second()) < age
            {
                continue;
            }
            let Some(project) = self
                .store
                .project(&record.project)
                .map_err(InstanceError::Adapter)?
            else {
                continue;
            };
            if !project
                .project
                .repo
                .as_ref()
                .is_some_and(|repo| repo.common_dir == common)
            {
                continue;
            }
            if !matches!(
                self.store.status(&id).map_err(InstanceError::Adapter)?,
                InstanceStatus::Stopped | InstanceStatus::NeedsConfig
            ) {
                continue;
            }
            self.store.remove(&id).map_err(InstanceError::Adapter)?;
        }
        Ok(())
    }
    /// Get current observation after an explicit or scheduled refresh.
    pub fn observation(&self, id: &InstanceId) -> Option<CheckoutObservation> {
        self.observations
            .lock()
            .expect("observations lock")
            .get(id)
            .cloned()
    }
    /// Allocate a collision-free slug, reviving a matching missing instance if present.
    pub fn allocate_slug(
        &self,
        project: &ProjectId,
        desired: &str,
        managed: bool,
    ) -> Result<InstanceId, InstanceError> {
        let p = self.project(project)?;
        Ok(self.allocate(&p, desired, None, managed)?.0)
    }
    fn allocate(
        &self,
        p: &ProjectConfig,
        desired: &str,
        exclude: Option<&InstanceId>,
        managed: bool,
    ) -> Result<(InstanceId, Option<InstanceRecord>), InstanceError> {
        let desired = branch_slug(desired);
        let all = self.store.records().map_err(InstanceError::Adapter)?;
        for n in 1u32.. {
            let suffix = if n == 1 { String::new() } else { format!("-{n}") };
            let keep = BRANCH_SLUG_MAX.saturating_sub(suffix.len());
            let slug = format!(
                "{}{}",
                desired[..desired.len().min(keep)].trim_end_matches('-'),
                suffix
            );
            let id = InstanceId::derived(&p.project.id, &slug)
                .map_err(|e| InstanceError::Refused(e.to_string()))?;
            let existing = all
                .iter()
                .find(|(other, _)| *other == id && Some(other) != exclude);
            if existing.is_some_and(|(_, r)| r.checkout.missing_since.is_none()) {
                continue;
            }
            let domain = instance_domain(&p.primary_domain, Some(&slug));
            if domain.len() > 253 {
                continue;
            }
            let mut occupied = false;
            for (other, r) in &all {
                if Some(other) == exclude || *other == id || r.checkout.missing_since.is_some() {
                    continue;
                }
                if let Some(op) = self.store.project(&r.project).map_err(InstanceError::Adapter)?
                    && instance_domain(&op.primary_domain, other.slug()) == domain
                {
                    occupied = true;
                    break;
                }
            }
            if occupied || (managed && self.worktrees_dir.join(&p.repo_slug).join(&slug).exists()) {
                continue;
            }
            return Ok((id, existing.map(|(_, r)| r.clone())));
        }
        Err(InstanceError::Refused("slug space exhausted".into()))
    }
    /// Pin or unpin a discovered checkout.
    pub fn set_pinned(&self, id: &InstanceId, pinned: bool) -> Result<(), InstanceError> {
        let mut r = self.record(id)?;
        r.pinned = pinned;
        self.store.upsert(id, r).map_err(InstanceError::Adapter)
    }
    /// Explicitly relabel a stopped instance, preserving its record and sticky resources.
    pub fn rename_instance(&self, id: &InstanceId, slug: &str) -> Result<InstanceId, InstanceError> {
        let r = self.record(id)?;
        if id.is_primary() {
            return Err(InstanceError::Refused("cannot relabel primary".into()));
        }
        if !matches!(
            self.store.status(id).map_err(InstanceError::Adapter)?,
            InstanceStatus::Stopped | InstanceStatus::NeedsConfig
        ) {
            return Err(InstanceError::Refused("instance is not stopped".into()));
        }
        let p = self.project(&r.project)?;
        let desired = branch_slug(slug);
        if desired != slug {
            return Err(InstanceError::Refused("invalid slug".into()));
        }
        let (new, revived) = self.allocate(&p, slug, Some(id), false)?;
        if new.slug() != Some(slug) || revived.is_some() {
            return Err(InstanceError::Refused("slug or domain already in use".into()));
        }
        if &new != id {
            self.store.rename(id, &new).map_err(InstanceError::Adapter)?;
            let mut o = self.observations.lock().expect("observations lock");
            if let Some(x) = o.remove(id) {
                o.insert(new.clone(), x);
            }
        }
        Ok(new)
    }
    /// Resolve, create or reuse a branch and prepare it for the runtime.
    pub async fn run_branch(
        &self,
        req: RunBranchRequest,
        op: &OpHandle,
    ) -> Result<RunBranchResponse, InstanceError> {
        self.run_branch_inner(req, op, true).await
    }
    /// Resolve or create a checkout, leaving preparation to the caller's shared operation gate.
    pub async fn run_branch_unprepared(
        &self,
        req: RunBranchRequest,
        op: &OpHandle,
    ) -> Result<RunBranchResponse, InstanceError> {
        self.run_branch_inner(req, op, false).await
    }
    async fn run_branch_inner(
        &self,
        req: RunBranchRequest,
        op: &OpHandle,
        prepare: bool,
    ) -> Result<RunBranchResponse, InstanceError> {
        let cancel = op.cancel_token();
        if cancel.is_cancelled() {
            return Err(InstanceError::Cancelled);
        }
        if matches!(req.target, BranchTarget::Pr(_)) && req.pr_trust.is_none() {
            return Err(InstanceError::Refused("pr_trust_confirmation_required".into()));
        }
        let p = self.project(&req.project)?;
        let pr = p.project.repo.as_ref().ok_or(InstanceError::NotARepo)?.clone();
        let repo = self.repo(&p)?;
        let guard = self.mutation.lock().await;
        self.refresh_unlocked(&pr.common_dir).await?;
        let trees = repo.list_worktrees().await?;
        let name = match &req.target {
            BranchTarget::Branch(n) | BranchTarget::NewBranch { name: n, .. } => Some(n.as_str()),
            _ => None,
        };
        if let Some(name) = name {
            repo.validate_branch(name).await?;
            if let Some(tree) = trees.iter().find(|w| w.branch.as_deref() == Some(name)) {
                if matches!(req.target, BranchTarget::NewBranch { .. }) {
                    return Err(InstanceError::Refused("branch_exists".into()));
                }
                if !req.detach {
                    if tree.prunable.is_some() || !tree.path.is_dir() {
                        return Err(InstanceError::Refused("checkout_missing".into()));
                    }
                    let path = tree.path.join(&pr.subpath);
                    let (id, mut record) = self
                        .store
                        .records()
                        .map_err(InstanceError::Adapter)?
                        .into_iter()
                        .find(|(_, r)| r.project == req.project && r.checkout.path == path)
                        .ok_or_else(|| InstanceError::Refused("project_not_in_branch".into()))?;
                    if req.pin {
                        record.pinned = true;
                        self.store.upsert(&id, record).map_err(InstanceError::Adapter)?;
                    }
                    let outcome = if tree.path == pr.git_root {
                        Outcome::ReusedPrimary
                    } else {
                        Outcome::ReusedCheckout
                    };
                    let domain = instance_domain(&p.primary_domain, id.slug());
                    drop(guard);
                    if prepare {
                        self.ensure_prepared(&id, false, op).await?;
                    }
                    return Ok(RunBranchResponse {
                        instance: id,
                        domain,
                        outcome,
                        start: req.start,
                    });
                }
            }
        }
        let remotes = repo.remotes().await?;
        if req.fetch == FetchPolicy::Always && !matches!(req.target, BranchTarget::Pr(_)) {
            for remote in &remotes {
                op.step("Fetching");
                repo.fetch(remote, Duration::from_secs(60), &cancel).await?;
            }
        }
        let mut branches = repo.branches(100_000).await?;
        if let BranchTarget::Branch(name) = &req.target
            && !branches.iter().any(|b| b.name == *name)
            && req.fetch == FetchPolicy::Auto
        {
            for remote in &remotes {
                op.step("Fetching");
                repo.fetch(remote, Duration::from_secs(60), &cancel).await?;
            }
            branches = repo.branches(100_000).await?;
        }
        let (mode, desired) = match &req.target {
            BranchTarget::Branch(name) => {
                if req.detach {
                    let rev = if branches.iter().any(|b| b.name == *name && b.remote.is_none()) {
                        format!("refs/heads/{name}")
                    } else {
                        let preferred = repo.preferred_remote().await?;
                        let remote =
                            select_remote(&branches, name, req.remote.as_deref(), preferred.as_deref())?;
                        format!("refs/remotes/{remote}/{name}")
                    };
                    (AddMode::Detached(rev), branch_slug(name))
                } else if branches.iter().any(|b| b.name == *name && b.remote.is_none()) {
                    (AddMode::ExistingLocal(name.clone()), branch_slug(name))
                } else {
                    let preferred = repo.preferred_remote().await?;
                    let remote = select_remote(&branches, name, req.remote.as_deref(), preferred.as_deref())?;
                    (
                        AddMode::TrackRemote {
                            remote,
                            branch: name.clone(),
                        },
                        branch_slug(name),
                    )
                }
            }
            BranchTarget::NewBranch { name, base } => {
                if branches.iter().any(|b| b.name == *name) {
                    return Err(InstanceError::Refused("branch_exists".into()));
                }
                let base = if let Some(base) = base {
                    base.clone()
                } else if let Some(b) = repo.default_branch().await? {
                    b
                } else if branches.iter().any(|b| b.name == "main" && b.remote.is_none()) {
                    String::from("refs/heads/main")
                } else if branches.iter().any(|b| b.name == "master" && b.remote.is_none()) {
                    String::from("refs/heads/master")
                } else {
                    String::from("HEAD")
                };
                if req.fetch != FetchPolicy::Never {
                    let remote = base
                        .split_once('/')
                        .map(|(r, _)| r)
                        .filter(|r| remotes.iter().any(|v| v == r));
                    if let Some(remote) = remote {
                        op.step("Fetching");
                        if let Err(error) = repo.fetch(remote, Duration::from_secs(15), &cancel).await {
                            if cancel.is_cancelled() {
                                return Err(InstanceError::Cancelled);
                            }
                            tracing::warn!(%error, %remote, "could not refresh new-branch base");
                        }
                    }
                }
                (
                    AddMode::NewBranch {
                        name: name.clone(),
                        base,
                    },
                    branch_slug(name),
                )
            }
            BranchTarget::Detached(rev) => {
                let oid = repo.resolve_commit(rev).await?;
                (AddMode::Detached(oid.clone()), detached_slug(&oid))
            }
            BranchTarget::Pr(n) => {
                let remote = req
                    .remote
                    .clone()
                    .or_else(|| remotes.iter().find(|r| r.as_str() == "origin").cloned())
                    .or_else(|| remotes.first().cloned())
                    .ok_or_else(|| InstanceError::Refused("no remote for PR".into()))?;
                op.step("Fetching");
                let oid = repo.fetch_pr(&remote, *n, &cancel).await?;
                let name = format!("pr-{n}");
                if branches.iter().any(|b| b.name == name) {
                    return Err(InstanceError::Refused("branch_exists".into()));
                }
                (
                    AddMode::NewBranch {
                        name: name.clone(),
                        base: oid,
                    },
                    name,
                )
            }
        };
        if Path::new(&p.repo_slug).components().count() != 1
            || !matches!(
                Path::new(&p.repo_slug).components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            return Err(InstanceError::UnsafePath("invalid repository slug".into()));
        }
        if pr
            .subpath
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(InstanceError::UnsafePath("invalid project subpath".into()));
        }
        std::fs::create_dir_all(&self.worktrees_dir)?;
        let managed_root = std::fs::canonicalize(&self.worktrees_dir)?;
        let repo_dir = managed_root.join(&p.repo_slug);
        match std::fs::symlink_metadata(&repo_dir) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(InstanceError::UnsafePath(
                    "managed repository directory is not a real directory".into(),
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(&repo_dir)?,
            Err(e) => return Err(e.into()),
        }
        if !std::fs::canonicalize(&repo_dir)?.starts_with(&managed_root) {
            return Err(InstanceError::UnsafePath(
                "repository directory escapes managed root".into(),
            ));
        }
        let (id, revived) = self.allocate(&p, &desired, None, true)?;
        let slug = id.slug().expect("derived id");
        let root = repo_dir.join(slug);
        let project_path = root.join(&pr.subpath);
        let previous = revived.clone();
        let mut record = revived.unwrap_or_else(|| {
            new_record(
                &req.project,
                project_path.clone(),
                CheckoutKind::Managed,
                req.pin,
                matches!(mode, AddMode::NewBranch { .. } | AddMode::TrackRemote { .. }),
            )
        });
        record.checkout.path = project_path.clone();
        record.checkout.kind = CheckoutKind::Managed;
        record.checkout.phase = CheckoutPhase::Creating;
        record.checkout.missing_since = None;
        record.pinned |= req.pin;
        self.store
            .upsert(&id, record.clone())
            .map_err(InstanceError::Adapter)?;
        if let Err(error) = self.store.flush().await {
            if let Some(previous) = previous.clone() {
                self.store.upsert(&id, previous).map_err(InstanceError::Adapter)?;
            } else {
                self.store.remove(&id).map_err(InstanceError::Adapter)?;
            }
            return Err(InstanceError::Adapter(format!(
                "worktree intent flush failed: {error}"
            )));
        }
        op.step("Creating worktree");
        if let Err(e) = repo.add_worktree(&root, &mode, &cancel).await {
            // A failed git call does not establish ownership of an on-disk path.
            // Restore a revived record rather than treating a raced-in user worktree as managed.
            if let Ok(trees) = repo.list_worktrees().await
                && project_path.is_dir()
                && trees.iter().any(|tree| !tree.bare && tree.path == root)
            {
                record.checkout.phase = CheckoutPhase::Ready;
                record.checkout.kind = CheckoutKind::Worktree;
                self.store.upsert(&id, record).map_err(InstanceError::Adapter)?;
                tracing::warn!(%id, path = %root.display(), "preserved checkout after interrupted git creation without claiming ownership");
            } else if let Some(mut previous) = previous {
                if root.exists() && previous.checkout.path == project_path {
                    previous.checkout.kind = CheckoutKind::Worktree;
                }
                self.store.upsert(&id, previous).map_err(InstanceError::Adapter)?;
            } else {
                self.store.remove(&id).map_err(InstanceError::Adapter)?;
            }
            return Err(e.into());
        }
        record.checkout.phase = CheckoutPhase::Ready;
        self.store.upsert(&id, record).map_err(InstanceError::Adapter)?;
        self.refresh_unlocked(&pr.common_dir).await?;
        let actual = repo
            .list_worktrees()
            .await?
            .into_iter()
            .find(|w| w.path == root)
            .ok_or_else(|| InstanceError::Refused("created worktree not listed".into()))?;
        let expected = match &mode {
            AddMode::ExistingLocal(n)
            | AddMode::TrackRemote { branch: n, .. }
            | AddMode::NewBranch { name: n, .. } => Some(n.as_str()),
            AddMode::Detached(_) => None,
        };
        if expected != actual.branch.as_deref() || (expected.is_none() && !actual.detached) {
            return Err(InstanceError::Refused(
                "unexpected_detached or branch mismatch".into(),
            ));
        }
        if !project_path.is_dir() {
            return Err(InstanceError::Refused("project_not_in_branch".into()));
        }
        let domain = instance_domain(&p.primary_domain, id.slug());
        drop(guard);
        if prepare {
            self.ensure_prepared(&id, false, op).await?;
        }
        Ok(RunBranchResponse {
            instance: id,
            domain,
            outcome: Outcome::CreatedWorktree(mode),
            start: req.start,
        })
    }
    /// Compute whether the checkout's lockfiles or setup command list changed.
    pub fn setup_stale(&self, id: &InstanceId) -> Result<bool, InstanceError> {
        let record = self.record(id)?;
        let project = self.project(&record.project)?;
        let root = if let Some(repo) = project.project.repo.as_ref() {
            strip_subpath(&record.checkout.path, &repo.subpath)?
        } else {
            record.checkout.path.clone()
        };
        let manifest = self.store.manifest(id).map_err(InstanceError::Adapter)?;
        let hash = deps::lock_hash(&record.checkout.path, &root)?;
        let commands = setup_commands(&manifest, &record.checkout.path, &root)?;
        let command_hash = deps::commands_hash(&commands);
        let changed = record
            .checkout
            .setup
            .as_ref()
            .is_none_or(|s| s.lock_hash != hash || s.commands_hash != command_hash);
        if changed {
            return Ok(true);
        }
        if record.checkout.kind != CheckoutKind::Primary && manifest.setup.is_none() {
            return Ok(deps::default_setup(&record.checkout.path, &root)?
                .iter()
                .any(|d| !d.artifact.is_dir()));
        }
        Ok(false)
    }
    /// Ensure setup is current and publish steps through the supplied operation.
    pub async fn ensure_prepared(
        &self,
        id: &InstanceId,
        force: bool,
        op: &OpHandle,
    ) -> Result<Prepared, InstanceError> {
        self.prepare_inner(id, force, op.cancel_token(), Some(op)).await
    }
    /// Runtime hook: prepare using the supervisor's cancellation token.
    pub async fn ensure_ready(
        &self,
        id: &InstanceId,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Prepared, InstanceError> {
        self.prepare_inner(id, false, cancel, None).await
    }
    /// Runtime hook with the supervisor cancellation token and operation progress.
    pub async fn ensure_ready_with_progress(
        &self,
        id: &InstanceId,
        cancel: tokio_util::sync::CancellationToken,
        op: &OpHandle,
    ) -> Result<Prepared, InstanceError> {
        self.prepare_inner(id, false, cancel, Some(op)).await
    }
    async fn prepare_inner(
        &self,
        id: &InstanceId,
        force: bool,
        cancel: tokio_util::sync::CancellationToken,
        op: Option<&OpHandle>,
    ) -> Result<Prepared, InstanceError> {
        if cancel.is_cancelled() {
            return Err(InstanceError::Cancelled);
        }
        let lock = {
            self.prepare_locks
                .lock()
                .expect("prepare locks")
                .entry(id.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _prepare_guard = tokio::select! {
            _ = cancel.cancelled() => return Err(InstanceError::Cancelled),
            guard = lock.lock() => guard,
        };
        let mut record = self.record(id)?;
        if let Some(repo) = self.project(&record.project)?.project.repo {
            self.refresh_repo(&repo.common_dir).await?;
            record = self.record(id)?;
        }
        if record.checkout.phase == CheckoutPhase::Creating {
            return Err(InstanceError::Refused(
                "interrupted worktree creation needs recovery".into(),
            ));
        }
        let p = self.project(&record.project)?;
        let pr = p.project.repo.as_ref();
        let root = if let Some(pr) = pr {
            strip_subpath(&record.checkout.path, &pr.subpath)?
        } else {
            record.checkout.path.clone()
        };
        if !record.checkout.path.is_dir() {
            return Err(InstanceError::Refused("checkout_missing".into()));
        }
        if let Some(pr) = pr
            && root.join(".gitmodules").exists()
        {
            let repo = self.git.repo(pr.common_dir.clone());
            if repo.submodules_need_init(&root).await? {
                progress(op, "Initializing submodules");
                repo.submodule_init(&root, &cancel).await?;
            }
        }
        progress(op, "Reading manifest");
        let manifest = self.store.manifest(id).map_err(InstanceError::Adapter)?;
        if !manifest.usable {
            return Err(InstanceError::Refused("needs_config".into()));
        }
        let obs = self.observation(id);
        let branch = obs.as_ref().and_then(|o| o.worktree.branch.clone()).or_else(|| {
            obs.as_ref()
                .and_then(|o| o.worktree.head.as_deref().map(detached_slug))
        });
        let slug = id
            .slug()
            .map(str::to_owned)
            .or_else(|| branch.as_deref().map(branch_slug));
        let domain = instance_domain(&p.primary_domain, id.slug());
        let leased_port = self.store.port(id).map_err(InstanceError::Adapter)?;
        let needs_port = manifest
            .setup
            .as_ref()
            .is_some_and(|commands| commands.iter().any(|s| s.mentions("PORT")))
            || manifest.run.env.values().any(|s| s.mentions("PORT"))
            || (record.checkout.kind != CheckoutKind::Primary
                && manifest.worktree.env.values().any(|s| s.mentions("PORT")));
        if leased_port.is_none() && needs_port {
            return Err(InstanceError::Refused("port_unavailable before setup".into()));
        }
        let port = leased_port.unwrap_or(0);
        let ctx = VarContext {
            port,
            host: "127.0.0.1".parse().expect("loopback"),
            domain: domain.clone(),
            url: format!("https://{domain}"),
            project: p.project.id.clone(),
            instance: id.clone(),
            branch: branch.clone(),
            branch_slug: slug.clone(),
            checkout: record.checkout.path.clone(),
            primary: p.project.root.clone(),
        };
        let worktree_env = if record.checkout.kind == CheckoutKind::Primary {
            BTreeMap::new()
        } else {
            manifest
                .worktree
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.render_data(&ctx)))
                .collect()
        };
        let mut env: BTreeMap<String, String> = manifest
            .run
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.render_data(&ctx)))
            .collect();
        env.extend(worktree_env.clone());
        for (k, v) in [
            ("HOST", ctx.host.to_string()),
            ("DOMAIN", domain.clone()),
            ("URL", ctx.url.clone()),
            ("PROJECT", p.project.id.to_string()),
            ("INSTANCE", id.safe_name()),
            ("BRANCH", branch.unwrap_or_default()),
            ("BRANCH_SLUG", slug.clone().unwrap_or_default()),
            ("BRANCH_SNAKE", slug.unwrap_or_default().replace('-', "_")),
            ("CHECKOUT", record.checkout.path.to_string_lossy().into_owned()),
            ("PRIMARY", p.project.root.to_string_lossy().into_owned()),
        ] {
            env.insert(format!("LANTERN_{k}"), v);
        }
        if let Some(port) = leased_port {
            env.insert("LANTERN_PORT".into(), port.to_string());
            env.insert("PORT".into(), port.to_string());
        }
        env.insert("DOMAIN".into(), domain);
        env.insert("PROJECT_NAME".into(), p.project.id.to_string());
        env.insert("LANTERN_PHASE".into(), "setup".into());
        if record.checkout.kind == CheckoutKind::Primary && !force {
            return Ok(Prepared {
                manifest,
                worktree_env,
            });
        }
        let defaults = if manifest.setup.is_none() {
            deps::default_setup(&record.checkout.path, &root)?
        } else {
            Vec::new()
        };
        let commands: Vec<(PathBuf, String)> = if let Some(setup) = &manifest.setup {
            setup
                .iter()
                .map(|v| (record.checkout.path.clone(), v.render_shell()))
                .collect()
        } else {
            defaults
                .iter()
                .map(|d| (d.cwd.clone(), d.command.clone()))
                .collect()
        };
        let lock_hash = deps::lock_hash(&record.checkout.path, &root)?;
        let commands_hash = deps::commands_hash(&commands);
        let stale = record
            .checkout
            .setup
            .as_ref()
            .is_none_or(|s| s.lock_hash != lock_hash || s.commands_hash != commands_hash)
            || defaults.iter().any(|d| !d.artifact.is_dir());
        if !force && !stale {
            return Ok(Prepared {
                manifest,
                worktree_env,
            });
        }
        if record.checkout.kind != CheckoutKind::Primary {
            let primary_root = pr.map(|r| r.git_root.as_path()).unwrap_or(&p.project.root);
            progress(op, "Copying files");
            let (copied, copy_result, warnings) = files::materialize_detailed(
                primary_root,
                &p.project.root,
                &root,
                &record.checkout.path,
                &manifest.worktree.copy,
                false,
                &cancel,
            );
            for warning in warnings {
                progress(op, &warning);
            }
            record.checkout.materialized.extend(copied);
            record.checkout.materialized.sort();
            record.checkout.materialized.dedup();
            self.store
                .upsert(id, record.clone())
                .map_err(InstanceError::Adapter)?;
            copy_result?;
            progress(op, "Linking files");
            let (linked, link_result, warnings) = files::materialize_detailed(
                primary_root,
                &p.project.root,
                &root,
                &record.checkout.path,
                &manifest.worktree.link,
                true,
                &cancel,
            );
            for warning in warnings {
                progress(op, &warning);
            }
            record.checkout.materialized.extend(linked);
            record.checkout.materialized.sort();
            record.checkout.materialized.dedup();
            self.store
                .upsert(id, record.clone())
                .map_err(InstanceError::Adapter)?;
            link_result?;
        }
        if !commands.is_empty() {
            progress(op, "Waiting for a free setup slot");
            let permit = tokio::select! {_=cancel.cancelled()=>return Err(InstanceError::Cancelled),p=self.setup_slots.acquire()=>p.map_err(|e|InstanceError::Adapter(e.to_string()))?};
            for (i, (cwd, command)) in commands.iter().enumerate() {
                if cancel.is_cancelled() {
                    return Err(InstanceError::Cancelled);
                }
                progress(op, &format!("Setup {}/{}: {}", i + 1, commands.len(), command));
                let spec = OneShotSpec {
                    instance: id.clone(),
                    cwd: cwd.clone(),
                    shell_command: command.clone(),
                    env: env.clone(),
                    timeout: self.timeout,
                    cancel: cancel.clone(),
                    stream: Stream::Setup,
                };
                let result = self.runner.run(spec).await;
                if cancel.is_cancelled() {
                    return Err(InstanceError::Cancelled);
                }
                result.map_err(InstanceError::Adapter)?;
            }
            drop(permit);
        }
        if cancel.is_cancelled() {
            return Err(InstanceError::Cancelled);
        }
        record.checkout.setup = Some(SetupStamp {
            lock_hash: deps::lock_hash(&record.checkout.path, &root)?,
            commands_hash,
            finished_at: Timestamp::now(),
        });
        self.store.upsert(id, record).map_err(InstanceError::Adapter)?;
        let stale = self.setup_stale(id)?;
        self.store
            .set_setup_stale(id, stale)
            .map_err(InstanceError::Adapter)?;
        progress(op, "Ready");
        Ok(Prepared {
            manifest,
            worktree_env,
        })
    }
}

fn progress(op: Option<&OpHandle>, step: &str) {
    if let Some(op) = op {
        op.step(step);
    }
}

fn setup_commands(
    manifest: &ResolvedManifest,
    project: &Path,
    root: &Path,
) -> Result<Vec<(PathBuf, String)>, InstanceError> {
    if let Some(setup) = &manifest.setup {
        Ok(setup
            .iter()
            .map(|cmd| (project.to_path_buf(), cmd.render_shell()))
            .collect())
    } else {
        Ok(deps::default_setup(project, root)?
            .into_iter()
            .map(|d| (d.cwd, d.command))
            .collect())
    }
}

fn select_remote(
    branches: &[crate::git::BranchInfo],
    name: &str,
    requested: Option<&str>,
    preferred: Option<&str>,
) -> Result<String, InstanceError> {
    let mut remotes: Vec<String> = branches
        .iter()
        .filter(|b| b.name == name)
        .filter_map(|b| b.remote.clone())
        .collect();
    remotes.sort();
    remotes.dedup();
    if let Some(r) = requested {
        return if remotes.iter().any(|v| v == r) {
            Ok(r.into())
        } else {
            Err(InstanceError::Refused("branch_not_found".into()))
        };
    }
    if remotes.len() == 1 {
        return Ok(remotes.remove(0));
    }
    if let Some(preferred) = preferred
        && remotes.iter().any(|v| v == preferred)
    {
        return Ok(preferred.into());
    }
    if remotes.iter().any(|v| v == "origin") {
        return Ok("origin".into());
    }
    if remotes.is_empty() {
        Err(InstanceError::Refused("branch_not_found".into()))
    } else {
        Err(InstanceError::Refused(format!(
            "ambiguous_remote: {}",
            remotes.join(", ")
        )))
    }
}
pub(crate) fn strip_subpath(path: &Path, subpath: &Path) -> Result<PathBuf, InstanceError> {
    let mut p = path.to_path_buf();
    for _ in subpath.components() {
        if !p.pop() {
            return Err(InstanceError::UnsafePath(path.display().to_string()));
        }
    }
    if p.join(subpath) != path {
        return Err(InstanceError::UnsafePath(path.display().to_string()));
    }
    Ok(p)
}
fn new_record(
    project: &ProjectId,
    path: PathBuf,
    kind: CheckoutKind,
    pinned: bool,
    created_branch: bool,
) -> InstanceRecord {
    InstanceRecord {
        project: project.clone(),
        checkout: CheckoutRecord {
            path,
            kind,
            phase: CheckoutPhase::Ready,
            created_at: Timestamp::now(),
            created_branch,
            materialized: Vec::new(),
            setup: None,
            missing_since: None,
        },
        pinned,
        adopt: false,
        activated: false,
        first_started: None,
        last_started: None,
        last_runtime: None,
    }
}
