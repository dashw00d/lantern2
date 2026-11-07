//! Safe, asynchronous git CLI access for repositories and linked worktrees.

use std::{
    collections::hash_map::DefaultHasher,
    ffi::{OsStr, OsString},
    hash::{Hash, Hasher},
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{process::Command, sync::Mutex};
use tokio_util::sync::CancellationToken;

const READ_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(120);

/// Failure of a git invocation or its output parser.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// Process could not be started.
    #[error("git spawn failed: {0}")]
    Spawn(#[from] std::io::Error),
    /// Installed git cannot produce NUL-delimited worktree output.
    #[error("git 2.36 or newer required: {0}")]
    GitTooOld(String),
    /// Ref is not safe or valid.
    #[error("invalid ref: {0}")]
    InvalidRef(String),
    /// Git rejected a branch name, preserving its diagnostic.
    #[error("invalid branch {name}: {stderr}")]
    InvalidBranch { name: String, stderr: String },
    /// Git returned nonzero, retaining its diagnostic and classified failure.
    #[error("git failed ({code:?}, {kind:?}): {stderr}")]
    Command {
        code: Option<i32>,
        stderr: String,
        kind: GitFailureKind,
    },
    /// Output is malformed.
    #[error("invalid git output: {0}")]
    Parse(String),
    /// Operation exceeded its deadline.
    #[error("git timed out")]
    Timeout,
    /// Operation was cancelled.
    #[error("git cancelled")]
    Cancelled,
}

/// Best-effort classification of a nonzero git command; stderr remains authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitFailureKind {
    /// Branch is already checked out in another worktree.
    BranchCheckedOut,
    /// Branch already exists.
    BranchExists,
    /// Destination path already exists.
    PathExists,
    /// Git reports a locked worktree.
    Locked,
    /// Ref does not resolve.
    RefNotFound,
    /// Unclassified error.
    Other,
}
fn classify_failure(stderr: &str) -> GitFailureKind {
    if stderr.contains("is already used by worktree") || stderr.contains("is already checked out at") {
        GitFailureKind::BranchCheckedOut
    } else if stderr.contains("branch named") && stderr.contains("already exists") {
        GitFailureKind::BranchExists
    } else if stderr.contains("already exists") {
        GitFailureKind::PathExists
    } else if stderr.contains("is locked") {
        GitFailureKind::Locked
    } else if stderr.contains("invalid reference")
        || stderr.contains("not a valid object name")
        || stderr.contains("Needed a single revision")
    {
        GitFailureKind::RefNotFound
    } else {
        GitFailureKind::Other
    }
}

/// Git executable and detected version.
#[derive(Debug, Clone)]
pub struct Git {
    bin: PathBuf,
    version: (u32, u32),
}

/// The state of a checkout's HEAD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadState {
    /// Checked-out local branch.
    Branch(String),
    /// Detached at this commit.
    Detached(String),
    /// No commits yet.
    Unborn,
}

/// The canonical repository and project path of a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// Top-level path of this worktree.
    pub git_root: PathBuf,
    /// Canonical shared git directory.
    pub common_dir: PathBuf,
    /// Project path relative to the worktree root.
    pub subpath: PathBuf,
    /// This is a linked worktree, not the main git directory.
    pub is_linked_worktree: bool,
    /// Current HEAD.
    pub head: HeadState,
}

/// An entry from `git worktree list --porcelain -z`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Worktree {
    /// Worktree root, preserving Unix path bytes.
    pub path: PathBuf,
    /// Commit object id, absent for bare repos.
    pub head: Option<String>,
    /// Local branch without `refs/heads/`.
    pub branch: Option<String>,
    /// HEAD is detached.
    pub detached: bool,
    /// Bare repository entry.
    pub bare: bool,
    /// Lock reason; empty when no reason was supplied.
    pub locked: Option<String>,
    /// Prunable reason; empty when unspecified.
    pub prunable: Option<String>,
}

/// A local or remote tracking branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInfo {
    /// Branch name without the remote prefix.
    pub name: String,
    /// Remote name for remote-tracking branches.
    pub remote: Option<String>,
    /// Commit object id.
    pub oid: String,
    /// Full upstream ref if configured.
    pub upstream: Option<String>,
    /// Number ahead of upstream.
    pub ahead: u32,
    /// Number behind upstream.
    pub behind: u32,
    /// Upstream has vanished.
    pub gone: bool,
    /// Commit timestamp.
    pub committed_at: i64,
    /// Checkout root if checked out.
    pub checked_out_at: Option<PathBuf>,
}

/// A changed path from porcelain status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedPath {
    /// Worktree-root-relative path.
    pub path: PathBuf,
    /// Git porcelain record type.
    pub kind: char,
}

/// Worktree status including parsed changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusSummary {
    /// Branch state.
    pub head: HeadState,
    /// Configured upstream.
    pub upstream: Option<String>,
    /// Counts ahead and behind.
    pub ahead_behind: Option<(u32, u32)>,
    /// Changes, including untracked files.
    pub changed: Vec<ChangedPath>,
}

/// Commit reachability calculation for removal checks.
#[derive(Debug, Clone, Copy)]
pub enum RevCount {
    /// Commits not present on any remote tracking ref.
    NotOnRemotes,
    /// Commits unreachable from branches, remotes, and tags.
    NotOnAnyRef,
}

/// Worktree creation mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddMode {
    /// Checkout an existing local branch.
    ExistingLocal(String),
    /// Create a tracking branch from the selected remote.
    TrackRemote {
        /// Remote name.
        remote: String,
        /// Local and remote branch name.
        branch: String,
    },
    /// Create a new non-tracking branch from a revision.
    NewBranch {
        /// New branch name.
        name: String,
        /// Base revision.
        base: String,
    },
    /// Checkout a commit detached.
    Detached(String),
}

/// A repository handle with serialized mutations.
#[derive(Debug, Clone)]
pub struct RepoHandle {
    git: Git,
    common_dir: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl Git {
    /// Detect git in PATH and require the porcelain `-z` version.
    pub async fn detect() -> Result<Self, GitError> {
        let mut git = Self {
            bin: PathBuf::from("git"),
            version: (0, 0),
        };
        let raw = git
            .run(None, &["--version".into()], READ_TIMEOUT, None, true)
            .await?;
        let text = String::from_utf8_lossy(&raw);
        let mut it = text.split_whitespace().nth(2).unwrap_or_default().split('.');
        git.version = (
            it.next().and_then(|s| s.parse().ok()).unwrap_or(0),
            it.next().and_then(|s| s.parse().ok()).unwrap_or(0),
        );
        if git.version < (2, 36) {
            return Err(GitError::GitTooOld(text.trim().to_string()));
        }
        Ok(git)
    }
    /// Detected major and minor version.
    pub fn version(&self) -> (u32, u32) {
        self.version
    }
    /// Bind a handle to a git common directory.
    pub fn repo(&self, common_dir: PathBuf) -> RepoHandle {
        RepoHandle {
            git: self.clone(),
            common_dir,
            lock: Arc::new(Mutex::new(())),
        }
    }
    /// Find the git root, common directory and subpath; return None outside a repository.
    pub async fn probe(&self, path: &Path) -> Result<Option<Probe>, GitError> {
        let args = vec![
            "rev-parse".into(),
            "--path-format=absolute".into(),
            "--show-toplevel".into(),
            "--git-common-dir".into(),
            "--absolute-git-dir".into(),
        ];
        let raw = match self.run(Some(path), &args, READ_TIMEOUT, None, true).await {
            Ok(v) => v,
            Err(GitError::Command { stderr, .. }) if stderr.contains("not a git repository") => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let lines: Vec<&[u8]> = raw.split(|b| *b == b'\n').collect();
        if lines.len() < 3 {
            return Err(GitError::Parse("rev-parse path output".into()));
        }
        let root = std::fs::canonicalize(PathBuf::from(OsString::from_vec(lines[0].to_vec())))?;
        let common = std::fs::canonicalize(PathBuf::from(OsString::from_vec(lines[1].to_vec())))?;
        let gitdir = std::fs::canonicalize(PathBuf::from(OsString::from_vec(lines[2].to_vec())))?;
        let real = std::fs::canonicalize(path)?;
        let subpath = real
            .strip_prefix(&root)
            .map_err(|_| GitError::Parse("project outside git root".into()))?
            .to_path_buf();
        let branch = self
            .run(
                Some(path),
                &[
                    "symbolic-ref".into(),
                    "--quiet".into(),
                    "--short".into(),
                    "HEAD".into(),
                ],
                READ_TIMEOUT,
                None,
                true,
            )
            .await;
        let head = match branch {
            Ok(v) => HeadState::Branch(trim(v)?),
            Err(GitError::Command { .. }) => match self
                .run(
                    Some(path),
                    &["rev-parse".into(), "--verify".into(), "HEAD".into()],
                    READ_TIMEOUT,
                    None,
                    true,
                )
                .await
            {
                Ok(v) => HeadState::Detached(trim(v)?),
                Err(GitError::Command { .. }) => HeadState::Unborn,
                Err(e) => return Err(e),
            },
            Err(e) => return Err(e),
        };
        Ok(Some(Probe {
            git_root: root,
            common_dir: common.clone(),
            subpath,
            is_linked_worktree: gitdir != common,
            head,
        }))
    }
    async fn run(
        &self,
        cwd: Option<&Path>,
        args: &[OsString],
        timeout: Duration,
        cancel: Option<&CancellationToken>,
        read: bool,
    ) -> Result<Vec<u8>, GitError> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(p) = cwd {
            cmd.current_dir(p);
        }
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_PREFIX",
            "GIT_OBJECT_DIRECTORY",
            "GIT_NAMESPACE",
        ] {
            cmd.env_remove(key);
        }
        cmd.env("LC_ALL", "C").env("GIT_TERMINAL_PROMPT", "0");
        if read {
            cmd.env("GIT_OPTIONAL_LOCKS", "0");
        }
        let child = cmd.spawn()?;
        let output = if let Some(token) = cancel {
            tokio::select! { _=token.cancelled()=>return Err(GitError::Cancelled), r=tokio::time::timeout(timeout,child.wait_with_output())=>r.map_err(|_|GitError::Timeout)?? }
        } else {
            tokio::time::timeout(timeout, child.wait_with_output())
                .await
                .map_err(|_| GitError::Timeout)??
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(GitError::Command {
                code: output.status.code(),
                kind: classify_failure(&stderr),
                stderr,
            });
        }
        Ok(output.stdout)
    }
}

fn trim(v: Vec<u8>) -> Result<String, GitError> {
    String::from_utf8(v)
        .map(|s| s.trim().to_string())
        .map_err(|e| GitError::Parse(e.to_string()))
}
fn text(v: &[u8]) -> Result<String, GitError> {
    String::from_utf8(v.to_vec()).map_err(|e| GitError::Parse(e.to_string()))
}
fn valid_arg(s: &str) -> Result<(), GitError> {
    if s.is_empty() || s.starts_with('-') || s.contains('\0') || s.contains('\n') {
        Err(GitError::InvalidRef(s.into()))
    } else {
        Ok(())
    }
}
fn path(v: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(v.to_vec()))
}

/// Parse git's NUL-delimited worktree porcelain output without losing path bytes.
pub fn parse_worktree_list(raw: &[u8]) -> Result<Vec<Worktree>, GitError> {
    let mut out = Vec::new();
    let mut item: Option<Worktree> = None;
    for rec in raw.split(|b| *b == 0) {
        if rec.is_empty() {
            if let Some(w) = item.take() {
                out.push(w);
            }
            continue;
        }
        if let Some(p) = rec.strip_prefix(b"worktree ") {
            if item.is_some() {
                return Err(GitError::Parse("missing worktree separator".into()));
            }
            item = Some(Worktree {
                path: path(p),
                ..Worktree::default()
            });
            continue;
        }
        let w = item
            .as_mut()
            .ok_or_else(|| GitError::Parse("worktree attribute without worktree".into()))?;
        if let Some(x) = rec.strip_prefix(b"HEAD ") {
            w.head = Some(text(x)?)
        } else if let Some(x) = rec.strip_prefix(b"branch refs/heads/") {
            w.branch = Some(text(x)?)
        } else if rec == b"detached" {
            w.detached = true
        } else if rec == b"bare" {
            w.bare = true
        } else if rec == b"locked" {
            w.locked = Some(String::new())
        } else if let Some(x) = rec.strip_prefix(b"locked ") {
            w.locked = Some(text(x)?)
        } else if rec == b"prunable" {
            w.prunable = Some(String::new())
        } else if let Some(x) = rec.strip_prefix(b"prunable ") {
            w.prunable = Some(text(x)?)
        }
    }
    if item.is_some() {
        return Err(GitError::Parse("unterminated worktree record".into()));
    }
    Ok(out)
}

/// Parse porcelain-v2 `-z` status, including rename source records.
pub fn parse_status(raw: &[u8]) -> Result<StatusSummary, GitError> {
    let mut head = HeadState::Unborn;
    let mut oid = None;
    let mut upstream = None;
    let mut ahead_behind = None;
    let mut changed = Vec::new();
    let mut fields = raw.split(|b| *b == 0);
    while let Some(r) = fields.next() {
        if let Some(v) = r.strip_prefix(b"# branch.oid ") {
            if v != b"(initial)" {
                oid = Some(text(v)?);
            }
        } else if let Some(v) = r.strip_prefix(b"# branch.head ") {
            if v != b"(detached)" {
                head = HeadState::Branch(text(v)?);
            }
        } else if let Some(v) = r.strip_prefix(b"# branch.upstream ") {
            upstream = Some(text(v)?);
        } else if let Some(v) = r.strip_prefix(b"# branch.ab ") {
            let s = text(v)?;
            let mut it = s.split_whitespace();
            let a = it
                .next()
                .and_then(|v| v.strip_prefix('+'))
                .and_then(|v| v.parse().ok());
            let b = it
                .next()
                .and_then(|v| v.strip_prefix('-'))
                .and_then(|v| v.parse().ok());
            if let (Some(a), Some(b)) = (a, b) {
                ahead_behind = Some((a, b));
            }
        } else if r.starts_with(b"? ") {
            changed.push(ChangedPath {
                path: path(&r[2..]),
                kind: '?',
            });
        } else if matches!(r.first(), Some(b'1' | b'2' | b'u')) {
            let kind = r[0] as char;
            let n = match kind {
                '1' => 8,
                '2' => 9,
                _ => 10,
            };
            let p = r
                .splitn(n + 1, |b| *b == b' ')
                .nth(n)
                .ok_or_else(|| GitError::Parse("status entry".into()))?;
            changed.push(ChangedPath { path: path(p), kind });
            if kind == '2' {
                fields
                    .next()
                    .ok_or_else(|| GitError::Parse("rename source".into()))?;
            }
        }
    }
    if matches!(head, HeadState::Unborn)
        && let Some(oid) = oid
    {
        head = HeadState::Detached(oid);
    }
    Ok(StatusSummary {
        head,
        upstream,
        ahead_behind,
        changed,
    })
}

impl RepoHandle {
    /// Common git directory of this repository.
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }
    async fn run(
        &self,
        args: Vec<OsString>,
        timeout: Duration,
        cancel: Option<&CancellationToken>,
        read: bool,
    ) -> Result<Vec<u8>, GitError> {
        let mut dir = OsString::from("--git-dir=");
        dir.push(&self.common_dir);
        let mut a = vec![dir];
        a.extend(args);
        self.git.run(None, &a, timeout, cancel, read).await
    }
    /// Validate a local branch using git's own rules.
    pub async fn validate_branch(&self, name: &str) -> Result<(), GitError> {
        valid_arg(name)?;
        self.run(
            vec!["check-ref-format".into(), "--branch".into(), name.into()],
            READ_TIMEOUT,
            None,
            true,
        )
        .await
        .map(|_| ())
        .map_err(|e| match e {
            GitError::Command { stderr, .. } => GitError::InvalidBranch {
                name: name.into(),
                stderr,
            },
            other => other,
        })
    }
    /// List checkouts using the NUL-delimited format.
    pub async fn list_worktrees(&self) -> Result<Vec<Worktree>, GitError> {
        parse_worktree_list(
            &self
                .run(
                    vec![
                        "worktree".into(),
                        "list".into(),
                        "--porcelain".into(),
                        "-z".into(),
                    ],
                    READ_TIMEOUT,
                    None,
                    true,
                )
                .await?,
        )
    }
    /// List local and remote branch refs.
    pub async fn branches(&self, limit: usize) -> Result<Vec<BranchInfo>, GitError> {
        let raw=self.run(vec!["for-each-ref".into(),"--sort=-committerdate".into(),format!("--count={limit}").into(),"--format=%(refname)%00%(objectname)%00%(upstream)%00%(upstream:track,nobracket)%00%(committerdate:unix)%00%(worktreepath)".into(),"refs/heads".into(),"refs/remotes".into()],READ_TIMEOUT,None,true).await?;
        parse_branches(&raw)
    }
    /// List configured remote names.
    pub async fn remotes(&self) -> Result<Vec<String>, GitError> {
        Ok(
            trim(self.run(vec!["remote".into()], READ_TIMEOUT, None, true).await?)?
                .lines()
                .map(str::to_owned)
                .collect(),
        )
    }
    /// Return origin's symbolic default branch when configured.
    pub async fn default_branch(&self) -> Result<Option<String>, GitError> {
        match self
            .run(
                vec![
                    "symbolic-ref".into(),
                    "--quiet".into(),
                    "--short".into(),
                    "refs/remotes/origin/HEAD".into(),
                ],
                READ_TIMEOUT,
                None,
                true,
            )
            .await
        {
            Ok(v) => Ok(Some(trim(v)?)),
            Err(GitError::Command { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }
    /// Resolve a revision to a commit object id without option injection.
    pub async fn resolve_commit(&self, rev: &str) -> Result<String, GitError> {
        valid_arg(rev)?;
        trim(
            self.run(
                vec![
                    "rev-parse".into(),
                    "--verify".into(),
                    "--end-of-options".into(),
                    format!("{rev}^{{commit}}").into(),
                ],
                READ_TIMEOUT,
                None,
                true,
            )
            .await?,
        )
    }
    /// Query status from the checkout, avoiding optional git index locks.
    pub async fn status(&self, root: &Path) -> Result<StatusSummary, GitError> {
        parse_status(
            &self
                .git
                .run(
                    Some(root),
                    &[
                        "status".into(),
                        "--porcelain=v2".into(),
                        "--branch".into(),
                        "-z".into(),
                        "--untracked-files=all".into(),
                    ],
                    READ_TIMEOUT,
                    None,
                    true,
                )
                .await?,
        )
    }
    /// Count unreachable commits for removal safety.
    pub async fn rev_count(&self, root: &Path, spec: RevCount) -> Result<u32, GitError> {
        let mut a: Vec<OsString> = vec!["rev-list".into(), "--count".into(), "HEAD".into(), "--not".into()];
        if matches!(spec, RevCount::NotOnAnyRef) {
            a.push("--branches".into());
        }
        a.push("--remotes".into());
        if matches!(spec, RevCount::NotOnAnyRef) {
            a.push("--tags".into());
        }
        trim(self.git.run(Some(root), &a, READ_TIMEOUT, None, true).await?)?
            .parse()
            .map_err(|e: std::num::ParseIntError| GitError::Parse(e.to_string()))
    }
    /// Test whether the first revision is an ancestor of the second.
    pub async fn is_ancestor(&self, a: &str, b: &str) -> Result<bool, GitError> {
        let a = self.resolve_commit(a).await?;
        let b = self.resolve_commit(b).await?;
        match self
            .run(
                vec!["merge-base".into(), "--is-ancestor".into(), a.into(), b.into()],
                READ_TIMEOUT,
                None,
                true,
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(GitError::Command { code: Some(1), .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }
    /// Fetch and prune a configured remote.
    pub async fn fetch(
        &self,
        remote: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(), GitError> {
        valid_arg(remote)?;
        let _guard = self.lock.lock().await;
        self.run(
            vec!["fetch".into(), "--prune".into(), "--".into(), remote.into()],
            timeout,
            Some(cancel),
            false,
        )
        .await
        .map(|_| ())
    }
    /// Fetch a GitHub-style pull ref; works with a local fake remote in tests.
    pub async fn fetch_pr(
        &self,
        remote: &str,
        n: u32,
        cancel: &CancellationToken,
    ) -> Result<String, GitError> {
        valid_arg(remote)?;
        let _guard = self.lock.lock().await;
        self.run(
            vec![
                "fetch".into(),
                "--".into(),
                remote.into(),
                format!("pull/{n}/head").into(),
            ],
            Duration::from_secs(60),
            Some(cancel),
            false,
        )
        .await?;
        self.resolve_commit("FETCH_HEAD").await
    }
    /// Create one worktree without shell interpolation or git's implicit branch guessing.
    pub async fn add_worktree(
        &self,
        dest: &Path,
        mode: &AddMode,
        cancel: &CancellationToken,
    ) -> Result<(), GitError> {
        let mut a: Vec<OsString> = vec!["worktree".into(), "add".into()];
        let target = match mode {
            AddMode::ExistingLocal(name) => {
                self.validate_branch(name).await?;
                name.clone()
            }
            AddMode::TrackRemote { remote, branch } => {
                self.validate_branch(branch).await?;
                valid_arg(remote)?;
                a.extend(["--track".into(), "-b".into(), branch.into()]);
                let r = format!("refs/remotes/{remote}/{branch}");
                self.resolve_commit(&r).await?;
                r
            }
            AddMode::NewBranch { name, base } => {
                self.validate_branch(name).await?;
                let oid = self.resolve_commit(base).await?;
                a.extend(["--no-track".into(), "-b".into(), name.into()]);
                oid
            }
            AddMode::Detached(rev) => {
                let oid = self.resolve_commit(rev).await?;
                a.push("--detach".into());
                oid
            }
        };
        a.push("--".into());
        a.push(dest.as_os_str().to_owned());
        a.push(target.into());
        let _guard = self.lock.lock().await;
        self.run(a, WRITE_TIMEOUT, Some(cancel), false).await.map(|_| ())
    }
    /// Remove an explicitly selected worktree; the caller must enforce managed ownership.
    pub async fn remove_worktree(&self, dest: &Path, force: bool) -> Result<(), GitError> {
        let _guard = self.lock.lock().await;
        let mut a: Vec<OsString> = vec!["worktree".into(), "remove".into()];
        if force {
            a.push("--force".into());
        }
        a.push("--".into());
        a.push(dest.as_os_str().to_owned());
        self.run(a, WRITE_TIMEOUT, None, false).await.map(|_| ())
    }
    /// Remove stale git administrative entries; callers must protect user worktrees first.
    pub async fn prune(&self) -> Result<Vec<String>, GitError> {
        let _guard = self.lock.lock().await;
        Ok(trim(
            self.run(
                vec!["worktree".into(), "prune".into(), "-v".into()],
                WRITE_TIMEOUT,
                None,
                false,
            )
            .await?,
        )?
        .lines()
        .map(str::to_owned)
        .collect())
    }
    /// Repair links after a worktree move.
    pub async fn repair(&self, paths: &[PathBuf]) -> Result<(), GitError> {
        let _guard = self.lock.lock().await;
        let mut a: Vec<OsString> = vec!["worktree".into(), "repair".into(), "--".into()];
        a.extend(paths.iter().map(|p| p.as_os_str().to_owned()));
        self.run(a, WRITE_TIMEOUT, None, false).await.map(|_| ())
    }
    /// Determine whether any recorded submodule is uninitialized.
    pub async fn submodules_need_init(&self, root: &Path) -> Result<bool, GitError> {
        let raw = self
            .git
            .run(
                Some(root),
                &["submodule".into(), "status".into(), "--recursive".into()],
                READ_TIMEOUT,
                None,
                true,
            )
            .await?;
        Ok(raw.split(|b| *b == b'\n').any(|line| line.first() == Some(&b'-')))
    }
    /// Initialize submodules in a checkout.
    pub async fn submodule_init(&self, root: &Path, cancel: &CancellationToken) -> Result<(), GitError> {
        let _guard = self.lock.lock().await;
        self.git
            .run(
                Some(root),
                &[
                    "submodule".into(),
                    "update".into(),
                    "--init".into(),
                    "--recursive".into(),
                ],
                WRITE_TIMEOUT,
                Some(cancel),
                false,
            )
            .await
            .map(|_| ())
    }
    /// Delete a merged local branch without force.
    pub async fn delete_branch(&self, name: &str) -> Result<(), GitError> {
        self.validate_branch(name).await?;
        let _guard = self.lock.lock().await;
        self.run(
            vec!["branch".into(), "-d".into(), "--".into(), name.into()],
            WRITE_TIMEOUT,
            None,
            false,
        )
        .await
        .map(|_| ())
    }
    /// Stat-only fingerprint; `None` forces a refresh.
    pub fn fingerprint(&self, known: &[PathBuf]) -> Option<u64> {
        fingerprint(&self.common_dir, known)
    }
}

/// Parse NUL-field branch lines from `for-each-ref`.
pub fn parse_branches(raw: &[u8]) -> Result<Vec<BranchInfo>, GitError> {
    let mut out = Vec::new();
    for line in raw.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let f: Vec<&[u8]> = line.split(|b| *b == 0).collect();
        if f.len() != 6 {
            return Err(GitError::Parse("branch fields".into()));
        }
        let full = text(f[0])?;
        let (name, remote) = if let Some(s) = full.strip_prefix("refs/heads/") {
            (s.to_string(), None)
        } else if let Some(s) = full.strip_prefix("refs/remotes/") {
            let Some((r, n)) = s.split_once('/') else { continue };
            if n == "HEAD" {
                continue;
            }
            (n.to_string(), Some(r.to_string()))
        } else {
            continue;
        };
        let track = text(f[3])?;
        let mut ahead = 0;
        let mut behind = 0;
        for word in track.split(", ") {
            if let Some(n) = word.strip_prefix("ahead ") {
                ahead = n.parse().unwrap_or(0)
            }
            if let Some(n) = word.strip_prefix("behind ") {
                behind = n.parse().unwrap_or(0)
            }
        }
        out.push(BranchInfo {
            name,
            remote,
            oid: text(f[1])?,
            upstream: if f[2].is_empty() { None } else { Some(text(f[2])?) },
            ahead,
            behind,
            gone: track == "gone",
            committed_at: text(f[4])?.parse().unwrap_or(0),
            checked_out_at: if f[5].is_empty() { None } else { Some(path(f[5])) },
        });
    }
    Ok(out)
}

/// Hash git admin metadata and checkout existence, returning None when stats are unavailable.
pub fn fingerprint(common: &Path, known: &[PathBuf]) -> Option<u64> {
    fn add(p: &Path, h: &mut DefaultHasher) -> Option<()> {
        p.hash(h);
        match std::fs::metadata(p) {
            Ok(m) => {
                m.len().hash(h);
                m.modified().ok()?.hash(h);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false.hash(h),
            Err(_) => return None,
        };
        Some(())
    }
    let mut h = DefaultHasher::new();
    add(&common.join("HEAD"), &mut h)?;
    let worktrees = common.join("worktrees");
    add(&worktrees, &mut h)?;
    if worktrees.exists() {
        let mut dirs = std::fs::read_dir(&worktrees)
            .ok()?
            .map(|e| e.map(|x| x.path()))
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        dirs.sort();
        for p in dirs {
            add(&p, &mut h)?;
            add(&p.join("HEAD"), &mut h)?;
            add(&p.join("gitdir"), &mut h)?;
        }
    }
    for p in known {
        p.hash(&mut h);
        p.join(OsStr::new(".git")).exists().hash(&mut h);
    }
    Some(h.finish())
}
