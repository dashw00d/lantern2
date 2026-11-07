//! Explicit removal checks; user-owned checkout directories are never removed.
use super::{
    InstanceError,
    service::{InstanceService, strip_subpath},
};
use crate::git::{ChangedPath, HeadState, RevCount};
use lantern_core::{
    Timestamp,
    ids::InstanceId,
    model::{CheckoutKind, InstanceStatus},
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// Reasons a worktree cannot be removed safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// User-owned checkout; only unpinning is permitted.
    NotManaged,
    /// A project in this shared checkout is active.
    Running,
    /// Git worktree is locked.
    Locked(String),
    /// Tracked or non-materialized changes.
    Dirty(usize),
    /// Commits are absent from all remote refs.
    Unpushed(u32),
    /// Detached commits would become unreachable.
    Orphaned(u32),
}
/// Detailed preflight result for user confirmation.
#[derive(Debug, Clone)]
pub struct RemovalCheck {
    /// All removal blockers.
    pub blockers: Vec<Blocker>,
    /// Changed paths not created by Lantern.
    pub dirty: Vec<ChangedPath>,
    /// Number of commits on no remote.
    pub unpushed: u32,
    /// Detached commits on no saved ref.
    pub orphaned: u32,
    /// Checkout includes submodule configuration.
    pub has_submodules: bool,
    /// Expanded resources which the user may clean up.
    pub worktree_env: BTreeMap<String, String>,
    /// Attached branch if any.
    pub branch: Option<String>,
}
/// Options for explicit removal.
#[derive(Debug, Clone, Copy, Default)]
pub struct RemoveOpts {
    /// Override dirty and unreachable-commit blockers only.
    pub force: bool,
    /// Delete a Lantern-created branch if it is safely merged.
    pub delete_branch: bool,
    /// Caller has requested runtime stop before removal.
    pub stop: bool,
}
/// Successful removal or user-worktree unpin result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveResult {
    /// True if a user worktree was only unpinned.
    pub forgotten: bool,
    /// True when a managed worktree directory was removed.
    pub removed: bool,
    /// Whether the optional safe branch deletion succeeded.
    pub branch_deleted: bool,
}
/// Why a checkout may be worth cleaning up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcReason {
    /// Remote-tracking upstream no longer exists.
    UpstreamGone,
    /// Branch is reachable from the default branch.
    Merged,
    /// Checkout has not been started recently.
    Idle,
    /// Checkout is missing.
    Missing,
}
/// Suggested non-forcing action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcAction {
    /// A clean, managed checkout may be removed after checking.
    Remove,
    /// User checkout may only be unpinned.
    Unpin,
    /// Missing entry can be forgotten after retention rules.
    ForgetMissing,
}
/// One advisory garbage collection candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcSuggestion {
    /// Candidate instance.
    pub instance: InstanceId,
    /// Reason it was suggested.
    pub reason: GcReason,
    /// Safe suggested action.
    pub action: GcAction,
}

impl InstanceService {
    /// Inspect safety without mutating or removing any checkout.
    pub async fn removal_check(&self, id: &InstanceId) -> Result<RemovalCheck, InstanceError> {
        let record = self.record(id)?;
        let p = self.project(&record.project)?;
        let pr = p.project.repo.as_ref().ok_or(InstanceError::NotARepo)?;
        self.refresh_repo(&pr.common_dir).await?;
        self.removal_check_current(id).await
    }
    async fn removal_check_current(&self, id: &InstanceId) -> Result<RemovalCheck, InstanceError> {
        let record = self.record(id)?;
        let p = self.project(&record.project)?;
        let pr = p.project.repo.as_ref().ok_or(InstanceError::NotARepo)?;
        let root = strip_subpath(&record.checkout.path, &pr.subpath)?;
        let repo = self.repo(&p)?;
        let observation = self.observation(id);
        let mut blockers = Vec::new();
        if record.checkout.kind != CheckoutKind::Managed {
            blockers.push(Blocker::NotManaged);
        }
        let mut materialized = Vec::new();
        for (sibling, r) in self.store.records().map_err(InstanceError::Adapter)? {
            let Some(project) = self.store.project(&r.project).map_err(InstanceError::Adapter)? else {
                continue;
            };
            if project.project.repo.as_ref().is_some_and(|other| {
                other.common_dir == pr.common_dir
                    && strip_subpath(&r.checkout.path, &other.subpath).is_ok_and(|x| x == root)
            }) {
                if r.checkout.kind != CheckoutKind::Managed && !blockers.contains(&Blocker::NotManaged) {
                    blockers.push(Blocker::NotManaged);
                }
                materialized.extend(r.checkout.materialized);
                let status = self.store.status(&sibling).map_err(InstanceError::Adapter)?;
                if !matches!(status, InstanceStatus::Stopped | InstanceStatus::NeedsConfig)
                    && !blockers.contains(&Blocker::Running)
                {
                    blockers.push(Blocker::Running);
                }
            }
        }
        if let Some(reason) = observation.as_ref().and_then(|o| o.worktree.locked.clone()) {
            blockers.push(Blocker::Locked(reason));
        }
        let mut dirty = Vec::new();
        let mut unpushed = 0;
        let mut orphaned = 0;
        let mut branch = None;
        let has_submodules = root.join(".gitmodules").exists();
        if root.is_dir() {
            let status = repo.status(&root).await?;
            branch = match &status.head {
                HeadState::Branch(b) => Some(b.clone()),
                _ => None,
            };
            dirty = status
                .changed
                .into_iter()
                .filter(|c| c.kind != '?' || !materialized.contains(&c.path))
                .collect();
            if in_progress(&root)? {
                dirty.push(ChangedPath {
                    path: PathBuf::from(".git (operation in progress)"),
                    kind: '!',
                });
            }
            if !dirty.is_empty() {
                blockers.push(Blocker::Dirty(dirty.len()));
            }
            if matches!(status.head, HeadState::Detached(_)) {
                orphaned = repo.rev_count(&root, RevCount::NotOnAnyRef).await?;
                if orphaned > 0 {
                    blockers.push(Blocker::Orphaned(orphaned));
                }
            } else if !repo.remotes().await?.is_empty() {
                unpushed = repo.rev_count(&root, RevCount::NotOnRemotes).await?;
                if unpushed > 0 {
                    blockers.push(Blocker::Unpushed(unpushed));
                }
            }
        }
        Ok(RemovalCheck {
            blockers,
            dirty,
            unpushed,
            orphaned,
            has_submodules,
            worktree_env: self.removal_environment(id, &record, &p, observation.as_ref())?,
            branch,
        })
    }
    fn removal_environment(
        &self,
        id: &InstanceId,
        record: &lantern_core::state::InstanceRecord,
        p: &super::ProjectConfig,
        obs: Option<&super::CheckoutObservation>,
    ) -> Result<BTreeMap<String, String>, InstanceError> {
        if record.checkout.kind == CheckoutKind::Primary {
            return Ok(BTreeMap::new());
        }
        let manifest = self.store.manifest(id).map_err(InstanceError::Adapter)?;
        let domain = lantern_core::ids::instance_domain(&p.primary_domain, id.slug());
        let branch = obs
            .and_then(|o| o.worktree.branch.clone())
            .or_else(|| obs.and_then(|o| o.worktree.head.as_deref().map(lantern_core::ids::detached_slug)));
        let context = lantern_core::template::VarContext {
            port: self.store.port(id).map_err(InstanceError::Adapter)?.unwrap_or(0),
            host: "127.0.0.1".parse().expect("loopback"),
            url: format!("https://{domain}"),
            domain,
            project: p.project.id.clone(),
            instance: id.clone(),
            branch,
            branch_slug: id.slug().map(str::to_owned),
            checkout: record.checkout.path.clone(),
            primary: p.project.root.clone(),
        };
        Ok(manifest
            .worktree
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.render_data(&context)))
            .collect())
    }
    /// Remove a verified managed checkout or only unpin a user-made worktree.
    pub async fn remove_checkout(
        &self,
        id: &InstanceId,
        opts: RemoveOpts,
    ) -> Result<RemoveResult, InstanceError> {
        let guard = self.mutation.lock().await;
        let record = self.record(id)?;
        let p = self.project(&record.project)?;
        let pr = p.project.repo.as_ref().ok_or(InstanceError::NotARepo)?;
        self.refresh_unlocked(&pr.common_dir).await?;
        let record = self.record(id)?;
        match record.checkout.kind {
            CheckoutKind::Primary => return Err(InstanceError::Refused("cannot_remove_primary".into())),
            CheckoutKind::Worktree => {
                let mut r = record;
                r.pinned = false;
                self.store.upsert(id, r).map_err(InstanceError::Adapter)?;
                return Ok(RemoveResult {
                    forgotten: true,
                    removed: false,
                    branch_deleted: false,
                });
            }
            CheckoutKind::Managed => {}
        }
        let check = self.removal_check_current(id).await?;
        if check
            .blockers
            .iter()
            .any(|b| matches!(b, Blocker::Running | Blocker::Locked(_) | Blocker::NotManaged))
        {
            return Err(InstanceError::Refused(
                if opts.stop && check.blockers.contains(&Blocker::Running) {
                    "instance_running: runtime must stop all sibling instances first".into()
                } else {
                    format!("removal blocked: {:?}", check.blockers)
                },
            ));
        }
        if !opts.force && !check.blockers.is_empty() {
            return Err(InstanceError::Refused(format!(
                "removal blocked: {:?}",
                check.blockers
            )));
        }
        let root = strip_subpath(&record.checkout.path, &pr.subpath)?;
        verify_managed(&root, &self.worktrees_dir, &pr.common_dir)?;
        let repo = self.repo(&p)?;
        repo.remove_worktree(&root, opts.force || check.has_submodules)
            .await?;
        for (other, r) in self.store.records().map_err(InstanceError::Adapter)? {
            if let Some(project) = self.store.project(&r.project).map_err(InstanceError::Adapter)?
                && let Some(other_repo) = &project.project.repo
                && other_repo.common_dir == pr.common_dir
                && strip_subpath(&r.checkout.path, &other_repo.subpath).is_ok_and(|x| x == root)
            {
                self.store.remove(&other).map_err(InstanceError::Adapter)?;
            }
        }
        let mut branch_deleted = false;
        if opts.delete_branch
            && record.checkout.created_branch
            && let Some(branch) = check.branch
        {
            branch_deleted = repo.delete_branch(&branch).await.is_ok();
        }
        if let Some(parent) = root.parent() {
            let _ = fs::remove_dir(parent);
        }
        drop(guard);
        Ok(RemoveResult {
            forgotten: false,
            removed: true,
            branch_deleted,
        })
    }
    /// Suggest stale checkouts without fetching or removing anything.
    pub async fn gc_suggestions(
        &self,
        project: &lantern_core::ids::ProjectId,
        idle_days: u64,
    ) -> Result<Vec<GcSuggestion>, InstanceError> {
        let p = self.project(project)?;
        let repo = self.repo(&p)?;
        let branches = repo.branches(100_000).await?;
        let default = repo.default_branch().await?;
        let mut out = Vec::new();
        for (id, r) in self
            .store
            .records()
            .map_err(InstanceError::Adapter)?
            .into_iter()
            .filter(|(_, r)| r.project == *project && r.checkout.kind != CheckoutKind::Primary)
        {
            let reason = if r.checkout.missing_since.is_some() {
                Some(GcReason::Missing)
            } else {
                let obs = self.observation(&id);
                let branch = obs.as_ref().and_then(|o| o.worktree.branch.as_deref());
                if branch.is_some_and(|n| {
                    branches
                        .iter()
                        .any(|b| b.remote.is_none() && b.name == n && b.gone)
                }) {
                    Some(GcReason::UpstreamGone)
                } else if let (Some(b), Some(default)) = (branch, default.as_deref()) {
                    if repo.is_ancestor(&format!("refs/heads/{b}"), default).await? {
                        Some(GcReason::Merged)
                    } else if idle(&r, idle_days) {
                        Some(GcReason::Idle)
                    } else {
                        None
                    }
                } else if idle(&r, idle_days) {
                    Some(GcReason::Idle)
                } else {
                    None
                }
            };
            if let Some(reason) = reason {
                let action = if r.checkout.kind == CheckoutKind::Worktree {
                    GcAction::Unpin
                } else if reason == GcReason::Missing {
                    GcAction::ForgetMissing
                } else {
                    GcAction::Remove
                };
                out.push(GcSuggestion {
                    instance: id,
                    reason,
                    action,
                });
            }
        }
        Ok(out)
    }
}
fn idle(r: &lantern_core::state::InstanceRecord, days: u64) -> bool {
    let last = r.last_started.unwrap_or(r.checkout.created_at).as_second();
    Timestamp::now().as_second().saturating_sub(last)
        >= days.saturating_mul(86_400).min(i64::MAX as u64) as i64
}
fn in_progress(root: &Path) -> Result<bool, InstanceError> {
    let dot = root.join(".git");
    let admin = if dot.is_dir() {
        dot
    } else {
        let text = fs::read_to_string(dot)?;
        let target = text
            .trim()
            .strip_prefix("gitdir: ")
            .ok_or_else(|| InstanceError::UnsafePath("invalid .git file".into()))?;
        let p = Path::new(target);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        }
    };
    Ok([
        "rebase-apply",
        "rebase-merge",
        "MERGE_HEAD",
        "BISECT_LOG",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
    ]
    .iter()
    .any(|p| admin.join(p).exists()))
}
fn verify_managed(root: &Path, managed: &Path, common: &Path) -> Result<(), InstanceError> {
    let root = fs::canonicalize(root)?;
    let managed = fs::canonicalize(managed)?;
    if root == managed || !root.starts_with(&managed) {
        return Err(InstanceError::UnsafePath(
            "checkout outside managed directory".into(),
        ));
    }
    let dot = root.join(".git");
    if !fs::symlink_metadata(&dot)?.is_file() {
        return Err(InstanceError::UnsafePath(
            "managed checkout lacks .git file".into(),
        ));
    }
    let text = fs::read_to_string(dot)?;
    let target = text
        .trim()
        .strip_prefix("gitdir: ")
        .ok_or_else(|| InstanceError::UnsafePath("invalid .git file".into()))?;
    let p = Path::new(target);
    let gitdir = fs::canonicalize(if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    })?;
    let admins = fs::canonicalize(common.join("worktrees"))?;
    if !gitdir.starts_with(&admins) || gitdir == admins {
        return Err(InstanceError::UnsafePath(
            "gitdir outside repository worktrees".into(),
        ));
    }
    Ok(())
}
