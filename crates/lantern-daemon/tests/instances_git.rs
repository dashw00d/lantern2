use lantern_core::{
    Timestamp,
    api::OperationKind,
    detect::{Detection, Templates},
    ids::{InstanceId, ProjectId},
    manifest::{self, ResolveInput, ResolvedManifest, raw::RawManifest},
    model::{InstanceStatus, Project, ProjectKind, ProjectRepo, ProjectSource, Stream},
    settings::Settings,
    state::InstanceRecord,
};
use lantern_daemon::{
    events::EventBus,
    git::{self, Git},
    instances::{
        BranchTarget, FetchPolicy, InstanceService, InstanceStore, OneShot, OneShotFuture, OneShotSpec,
        Outcome, ProjectConfig, RemoveOpts,
    },
    operations::{OpOutcome, OperationTracker},
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

static SEQ: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "lantern-instances-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn git_cmd(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Lantern Test")
        .env("GIT_AUTHOR_EMAIL", "lantern@example.invalid")
        .env("GIT_COMMITTER_NAME", "Lantern Test")
        .env("GIT_COMMITTER_EMAIL", "lantern@example.invalid")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn make_repo(temp: &Temp) -> (PathBuf, PathBuf) {
    let root = temp.0.join("repo");
    fs::create_dir(&root).unwrap();
    git_cmd(&root, &["init", "-b", "main"]);
    fs::create_dir_all(root.join("apps/web")).unwrap();
    fs::create_dir_all(root.join("apps/service")).unwrap();
    fs::write(root.join("apps/web/app.txt"), "base\n").unwrap();
    fs::write(root.join("apps/service/app.txt"), "base\n").unwrap();
    fs::write(root.join(".gitignore"), "branch.txt\n").unwrap();
    git_cmd(&root, &["add", "--all"]);
    git_cmd(&root, &["commit", "-m", "base"]);
    let remote = temp.0.join("origin.git");
    git_cmd(&temp.0, &["init", "--bare", remote.to_str().unwrap()]);
    git_cmd(&root, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git_cmd(&root, &["push", "-u", "origin", "main"]);
    (root, remote)
}
fn resolved(yaml: &str) -> Arc<ResolvedManifest> {
    let parsed = manifest::parse_manifest(yaml, "test.yaml");
    let result = manifest::resolve(ResolveInput {
        dir_name: "myapp",
        is_primary: true,
        primary: Some(&parsed),
        checkout: None,
        overrides: &RawManifest::default(),
        templates: &Templates::default(),
        detection: &Detection::default(),
        settings: &Settings::default(),
    });
    assert!(result.usable, "{:?}", result.diagnostics);
    Arc::new(result)
}
#[derive(Default)]
struct FakeStore {
    projects: Mutex<BTreeMap<ProjectId, ProjectConfig>>,
    records: Mutex<BTreeMap<InstanceId, InstanceRecord>>,
    manifest: Mutex<Option<Arc<ResolvedManifest>>>,
    statuses: Mutex<BTreeMap<InstanceId, InstanceStatus>>,
}
impl InstanceStore for FakeStore {
    fn project(&self, id: &ProjectId) -> Result<Option<ProjectConfig>, String> {
        Ok(self.projects.lock().unwrap().get(id).cloned())
    }
    fn projects_in_repo(&self, common: &Path) -> Result<Vec<ProjectConfig>, String> {
        Ok(self
            .projects
            .lock()
            .unwrap()
            .values()
            .filter(|p| p.project.repo.as_ref().is_some_and(|r| r.common_dir == common))
            .cloned()
            .collect())
    }
    fn records(&self) -> Result<Vec<(InstanceId, InstanceRecord)>, String> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }
    fn upsert(&self, id: &InstanceId, r: InstanceRecord) -> Result<(), String> {
        self.records.lock().unwrap().insert(id.clone(), r);
        Ok(())
    }
    fn remove(&self, id: &InstanceId) -> Result<(), String> {
        self.records.lock().unwrap().remove(id);
        Ok(())
    }
    fn rename(&self, old: &InstanceId, new: &InstanceId) -> Result<(), String> {
        let mut r = self.records.lock().unwrap();
        let v = r.remove(old).ok_or("missing")?;
        r.insert(new.clone(), v);
        Ok(())
    }
    fn manifest(&self, _: &InstanceId) -> Result<Arc<ResolvedManifest>, String> {
        Ok(self.manifest.lock().unwrap().clone().unwrap())
    }
    fn status(&self, id: &InstanceId) -> Result<InstanceStatus, String> {
        Ok(*self
            .statuses
            .lock()
            .unwrap()
            .get(id)
            .unwrap_or(&InstanceStatus::Stopped))
    }
    fn port(&self, _: &InstanceId) -> Result<Option<u16>, String> {
        Ok(Some(29876))
    }
}
#[derive(Default)]
struct FakeRunner {
    calls: Mutex<Vec<OneShotSpec>>,
}
impl OneShot for FakeRunner {
    fn run(&self, spec: OneShotSpec) -> OneShotFuture<'_> {
        Box::pin(async move {
            assert_eq!(spec.stream, Stream::Setup);
            self.calls.lock().unwrap().push(spec.clone());
            let output = tokio::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&spec.shell_command)
                .current_dir(&spec.cwd)
                .envs(&spec.env)
                .output()
                .await
                .map_err(|e| e.to_string())?;
            if output.status.success() {
                Ok(())
            } else {
                Err(String::from_utf8_lossy(&output.stderr).to_string())
            }
        })
    }
}
fn register(store: &FakeStore, root: &Path, id: &str, sub: &str) {
    let id = ProjectId::parse(id).unwrap();
    let now = Timestamp::now();
    let common = fs::canonicalize(root.join(".git")).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let project = Project {
        id: id.clone(),
        aliases: vec![],
        name: id.to_string(),
        root: root.join(sub),
        repo: Some(ProjectRepo {
            common_dir: common,
            git_root: root,
            subpath: PathBuf::from(sub),
        }),
        kind: ProjectKind::Project,
        enabled: true,
        tags: vec![],
        registered_at: now,
        source: ProjectSource::Manifest,
    };
    store.projects.lock().unwrap().insert(
        id.clone(),
        ProjectConfig {
            project,
            repo_slug: "mono".into(),
            primary_domain: format!("{id}.glow"),
        },
    );
}
fn req(target: BranchTarget) -> lantern_daemon::instances::RunBranchRequest {
    lantern_daemon::instances::RunBranchRequest {
        project: ProjectId::parse("myapp").unwrap(),
        target,
        start: false,
        pin: true,
        fetch: FetchPolicy::Never,
        detach: false,
        remote: None,
    }
}
fn operation() -> lantern_daemon::operations::OpHandle {
    OperationTracker::new(EventBus::new()).begin(OperationKind::Prepare, None)
}
async fn fixture(
    temp: &Temp,
    yaml: &str,
) -> (PathBuf, PathBuf, Arc<FakeStore>, Arc<FakeRunner>, InstanceService) {
    let (root, remote) = make_repo(temp);
    let store = Arc::new(FakeStore::default());
    *store.manifest.lock().unwrap() = Some(resolved(yaml));
    register(&store, &root, "myapp", "apps/web");
    let runner = Arc::new(FakeRunner::default());
    let service = InstanceService::new(
        Git::detect().await.unwrap(),
        store.clone(),
        runner.clone(),
        temp.0.join("managed"),
        2,
        Duration::from_secs(30),
    );
    (root, remote, store, runner, service)
}
const EMPTY: &str = "id: myapp\nrun: {cmd: 'echo ok'}\nsetup: []\n";

#[tokio::test]
async fn instances_git_porcelain_and_probe() {
    let t = Temp::new();
    let (root, _) = make_repo(&t);
    let git = Git::detect().await.unwrap();
    let probe = git.probe(&root.join("apps/web")).await.unwrap().unwrap();
    assert_eq!(probe.subpath, Path::new("apps/web"));
    let repo = git.repo(probe.common_dir);
    let list = repo.list_worktrees().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].branch.as_deref(), Some("main"));
    assert!(repo.branches(50).await.unwrap().iter().any(|b| b.name == "main"));
    fs::write(root.join("apps/web/new file"), "x").unwrap();
    let status = repo.status(&root).await.unwrap();
    assert!(
        status
            .changed
            .iter()
            .any(|c| c.path == Path::new("apps/web/new file"))
    );
    let fixture=b"worktree /a path\0HEAD 1234567\0branch refs/heads/x$(id)\0locked reason here\0\0worktree /bare\0bare\0\0";
    let v = git::parse_worktree_list(fixture).unwrap();
    assert_eq!(v.len(), 2);
    assert_eq!(v[0].branch.as_deref(), Some("x$(id)"));
    assert_eq!(v[0].locked.as_deref(), Some("reason here"));
    assert!(v[1].bare);
}

#[tokio::test]
async fn instances_branch_create_reuse_remote_and_monorepo() {
    let t = Temp::new();
    let (root, remote, store, _, service) = fixture(&t, EMPTY).await;
    register(&store, &root, "backend", "apps/service");
    git_cmd(&root, &["branch", "local-one"]);
    let sha = git_cmd(&root, &["rev-parse", "HEAD"]);
    git_cmd(
        &t.0,
        &[
            "--git-dir",
            remote.to_str().unwrap(),
            "update-ref",
            "refs/heads/remote-one",
            &sha,
        ],
    );
    git_cmd(&root, &["fetch", "origin"]);
    let op = operation();
    let a = service
        .run_branch(req(BranchTarget::Branch("local-one".into())), &op)
        .await
        .unwrap();
    assert_eq!(a.instance.as_str(), "myapp@local-one");
    assert!(matches!(a.outcome, Outcome::CreatedWorktree(_)));
    let record = store
        .records()
        .unwrap()
        .into_iter()
        .find(|(i, _)| i == &a.instance)
        .unwrap()
        .1;
    assert!(record.checkout.path.ends_with("local-one/apps/web"));
    assert!(
        store
            .records()
            .unwrap()
            .iter()
            .any(|(i, r)| i.as_str() == "backend@local-one" && !r.pinned)
    );
    let reuse = service
        .run_branch(req(BranchTarget::Branch("local-one".into())), &op)
        .await
        .unwrap();
    assert_eq!(reuse.outcome, Outcome::ReusedCheckout);
    let primary = service
        .run_branch(req(BranchTarget::Branch("main".into())), &op)
        .await
        .unwrap();
    assert_eq!(primary.outcome, Outcome::ReusedPrimary);
    assert_eq!(primary.domain, "myapp.glow");
    let b = service
        .run_branch(req(BranchTarget::Branch("remote-one".into())), &op)
        .await
        .unwrap();
    assert!(matches!(
        b.outcome,
        Outcome::CreatedWorktree(git::AddMode::TrackRemote { .. })
    ));
    let c = service
        .run_branch(
            req(BranchTarget::NewBranch {
                name: "fresh".into(),
                base: Some("main".into()),
            }),
            &op,
        )
        .await
        .unwrap();
    assert!(matches!(
        c.outcome,
        Outcome::CreatedWorktree(git::AddMode::NewBranch { .. })
    ));
    op.finish(OpOutcome::Succeeded(None));
}

#[tokio::test]
async fn instances_setup_is_safe_stamped_and_copies_links() {
    let t = Temp::new();
    let yaml = "id: myapp\nrun: {cmd: 'echo ok'}\nsetup:\n  - 'printf \"%s\" \"${BRANCH}\" > branch.txt'\nworktree:\n  copy: [.env]\n  link: [assets]\n  env: {DB_NAME: 'app_${BRANCH_SNAKE}'}\n";
    let (root, _, store, runner, service) = fixture(&t, yaml).await;
    fs::write(root.join("apps/web/.env"), "secret\n").unwrap();
    fs::create_dir(root.join("apps/web/assets")).unwrap();
    fs::write(root.join("apps/web/assets/data"), "value").unwrap();
    git_cmd(&root, &["branch", "--", "x$(id)"]);
    let op = operation();
    let result = service
        .run_branch(req(BranchTarget::Branch("x$(id)".into())), &op)
        .await
        .unwrap();
    let rec = store
        .records()
        .unwrap()
        .into_iter()
        .find(|(i, _)| i == &result.instance)
        .unwrap()
        .1;
    assert_eq!(
        fs::read_to_string(rec.checkout.path.join("branch.txt")).unwrap(),
        "x$(id)"
    );
    assert_eq!(
        fs::read_to_string(rec.checkout.path.join(".env")).unwrap(),
        "secret\n"
    );
    assert!(
        fs::symlink_metadata(rec.checkout.path.join("assets"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(rec.checkout.setup.is_some());
    assert_eq!(runner.calls.lock().unwrap().len(), 1);
    let prepared = service
        .ensure_prepared(&result.instance, false, &op)
        .await
        .unwrap();
    assert_eq!(prepared.worktree_env["DB_NAME"], "app_x_id");
    assert_eq!(runner.calls.lock().unwrap().len(), 1);
    assert_eq!(runner.calls.lock().unwrap()[0].env["LANTERN_PORT"], "29876");
    op.finish(OpOutcome::Succeeded(None));
}

#[tokio::test]
async fn instances_removal_guards_and_user_worktree() {
    let t = Temp::new();
    let (root, _, store, _, service) = fixture(&t, EMPTY).await;
    git_cmd(&root, &["branch", "--", "manual"]);
    let manual = t.0.join("manual");
    git_cmd(
        &root,
        &["worktree", "add", "--", manual.to_str().unwrap(), "manual"],
    );
    service.refresh_repo(&root.join(".git")).await.unwrap();
    let manual_id = InstanceId::parse("myapp@manual").unwrap();
    let r = service
        .remove_checkout(
            &manual_id,
            RemoveOpts {
                force: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(r.forgotten);
    assert!(manual.is_dir());
    git_cmd(&root, &["branch", "managed"]);
    let op = operation();
    let created = service
        .run_branch(req(BranchTarget::Branch("managed".into())), &op)
        .await
        .unwrap();
    let rec = store
        .records()
        .unwrap()
        .into_iter()
        .find(|(i, _)| i == &created.instance)
        .unwrap()
        .1;
    fs::write(rec.checkout.path.join("app.txt"), "dirty").unwrap();
    assert!(
        service
            .remove_checkout(&created.instance, RemoveOpts::default())
            .await
            .is_err()
    );
    git_cmd(&rec.checkout.path, &["add", "app.txt"]);
    git_cmd(&rec.checkout.path, &["commit", "-m", "local change"]);
    let check = service.removal_check(&created.instance).await.unwrap();
    assert_eq!(check.unpushed, 1);
    assert!(
        service
            .remove_checkout(&created.instance, RemoveOpts::default())
            .await
            .is_err()
    );
    let done = service
        .remove_checkout(
            &created.instance,
            RemoveOpts {
                force: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(done.removed);
    assert!(!rec.checkout.path.exists());
    assert!(manual.exists());
    op.finish(OpOutcome::Succeeded(None));
}

#[tokio::test]
async fn instances_pr_detached_and_option_ref_safety() {
    let t = Temp::new();
    let (root, remote, _, _, service) = fixture(&t, EMPTY).await;
    let oid = git_cmd(&root, &["rev-parse", "HEAD"]);
    git_cmd(
        &t.0,
        &[
            "--git-dir",
            remote.to_str().unwrap(),
            "update-ref",
            "refs/pull/42/head",
            &oid,
        ],
    );
    let op = operation();
    let pr = service.run_branch(req(BranchTarget::Pr(42)), &op).await.unwrap();
    assert_eq!(pr.instance.as_str(), "myapp@pr-42");
    let detached = service
        .run_branch(req(BranchTarget::Detached(oid.clone())), &op)
        .await
        .unwrap();
    assert_eq!(detached.instance.slug(), Some(&oid[..7]));
    assert!(matches!(
        detached.outcome,
        Outcome::CreatedWorktree(git::AddMode::Detached(_))
    ));
    assert!(
        service
            .run_branch(req(BranchTarget::Branch("--help".into())), &op)
            .await
            .is_err()
    );
    assert!(
        service
            .run_branch(req(BranchTarget::Detached("--help".into())), &op)
            .await
            .is_err()
    );
    op.finish(OpOutcome::Succeeded(None));
}

#[tokio::test]
async fn instances_discovery_collisions_freeze_and_revival() {
    let t = Temp::new();
    let (root, _, store, _, service) = fixture(&t, EMPTY).await;
    git_cmd(&root, &["branch", "feature/a"]);
    git_cmd(&root, &["branch", "feature-a"]);
    git_cmd(&root, &["branch", "scratch"]);
    let a = t.0.join("a");
    let b = t.0.join("b");
    git_cmd(
        &root,
        &["worktree", "add", "--", a.to_str().unwrap(), "feature/a"],
    );
    git_cmd(
        &root,
        &["worktree", "add", "--", b.to_str().unwrap(), "feature-a"],
    );
    service.refresh_repo(&root.join(".git")).await.unwrap();
    let first = InstanceId::parse("myapp@feature-a").unwrap();
    let second = InstanceId::parse("myapp@feature-a-2").unwrap();
    assert!(
        store
            .records()
            .unwrap()
            .iter()
            .any(|(i, r)| i == &first && r.checkout.path == a.join("apps/web"))
    );
    assert!(
        store
            .records()
            .unwrap()
            .iter()
            .any(|(i, r)| i == &second && r.checkout.path == b.join("apps/web"))
    );
    git_cmd(&a, &["switch", "scratch"]);
    service.refresh_repo(&root.join(".git")).await.unwrap();
    let scratch = InstanceId::parse("myapp@scratch").unwrap();
    assert!(service.observation(&scratch).is_some());
    service.set_pinned(&scratch, true).unwrap();
    git_cmd(&a, &["switch", "feature/a"]);
    service.refresh_repo(&root.join(".git")).await.unwrap();
    assert_eq!(
        service.observation(&scratch).unwrap().worktree.branch.as_deref(),
        Some("feature/a")
    );
    git_cmd(&root, &["worktree", "remove", "--", a.to_str().unwrap()]);
    service.refresh_repo(&root.join(".git")).await.unwrap();
    let saved = store
        .records()
        .unwrap()
        .into_iter()
        .find(|(i, _)| i == &scratch)
        .unwrap()
        .1;
    assert!(saved.checkout.missing_since.is_some());
    let restored = t.0.join("z");
    git_cmd(
        &root,
        &["worktree", "add", "--", restored.to_str().unwrap(), "scratch"],
    );
    service.refresh_repo(&root.join(".git")).await.unwrap();
    let saved = store
        .records()
        .unwrap()
        .into_iter()
        .find(|(i, _)| i == &scratch)
        .unwrap()
        .1;
    assert!(saved.pinned);
    assert_eq!(saved.checkout.path, restored.join("apps/web"));
    assert!(saved.checkout.missing_since.is_none());
}

#[tokio::test]
async fn instances_setup_stale_and_copy_guards() {
    let t = Temp::new();
    let yaml =
        "id: myapp\nrun: {cmd: 'echo ok'}\nsetup: ['printf ok > branch.txt']\nworktree: {copy: [.env]}\n";
    let (root, _, store, runner, service) = fixture(&t, yaml).await;
    fs::write(root.join("apps/web/.env"), "original").unwrap();
    git_cmd(&root, &["branch", "stale"]);
    let op = operation();
    let result = service
        .run_branch(req(BranchTarget::Branch("stale".into())), &op)
        .await
        .unwrap();
    let rec = store
        .records()
        .unwrap()
        .into_iter()
        .find(|(i, _)| i == &result.instance)
        .unwrap()
        .1;
    fs::write(rec.checkout.path.join(".env"), "edited").unwrap();
    fs::write(rec.checkout.path.join("package-lock.json"), "{}\n").unwrap();
    service
        .ensure_prepared(&result.instance, false, &op)
        .await
        .unwrap();
    assert_eq!(runner.calls.lock().unwrap().len(), 2);
    assert_eq!(
        fs::read_to_string(rec.checkout.path.join(".env")).unwrap(),
        "edited"
    );
    let token = op.cancel_token();
    token.cancel();
    assert!(
        service
            .ensure_prepared(&result.instance, true, &op)
            .await
            .is_err()
    );
    assert_eq!(runner.calls.lock().unwrap().len(), 2);
    op.finish(OpOutcome::Cancelled);
}

#[tokio::test]
async fn instances_git_guard_copied_directory_and_running_sibling() {
    let t = Temp::new();
    let yaml = "id: myapp\nrun: {cmd: 'echo ok'}\nsetup: []\nworktree: {copy: [fixtures]}\n";
    let (root, _, store, _, service) = fixture(&t, yaml).await;
    register(&store, &root, "backend", "apps/service");
    fs::create_dir(root.join("apps/web/fixtures")).unwrap();
    fs::write(root.join("apps/web/fixtures/copied"), "from primary").unwrap();
    git_cmd(&root, &["branch", "testing"]);
    let op = operation();
    let run = service
        .run_branch(req(BranchTarget::Branch("testing".into())), &op)
        .await
        .unwrap();
    let rec = store
        .records()
        .unwrap()
        .into_iter()
        .find(|(i, _)| i == &run.instance)
        .unwrap()
        .1;
    let check = service.removal_check(&run.instance).await.unwrap();
    assert!(check.dirty.is_empty(), "{:?}", check.dirty);
    fs::write(rec.checkout.path.join("fixtures/user-added"), "keep").unwrap();
    let check = service.removal_check(&run.instance).await.unwrap();
    assert!(
        check
            .dirty
            .iter()
            .any(|c| c.path.ends_with("fixtures/user-added"))
    );
    let sibling = InstanceId::parse("backend@testing").unwrap();
    store
        .statuses
        .lock()
        .unwrap()
        .insert(sibling, InstanceStatus::Running);
    assert!(
        service
            .remove_checkout(
                &run.instance,
                RemoveOpts {
                    force: true,
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    assert!(rec.checkout.path.exists());
    op.finish(OpOutcome::Succeeded(None));
}

#[tokio::test]
async fn instances_setup_failure_never_stamps_and_retry_succeeds() {
    let t = Temp::new();
    let (root, _, store, runner, service) = fixture(
        &t,
        "id: myapp\nrun: {cmd: 'echo ok'}\nsetup: ['false', 'printf ok > branch.txt']\n",
    )
    .await;
    git_cmd(&root, &["branch", "broken"]);
    let op = operation();
    assert!(
        service
            .run_branch(req(BranchTarget::Branch("broken".into())), &op)
            .await
            .is_err()
    );
    let id = InstanceId::parse("myapp@broken").unwrap();
    let rec = store
        .records()
        .unwrap()
        .into_iter()
        .find(|(i, _)| i == &id)
        .unwrap()
        .1;
    assert!(rec.checkout.setup.is_none());
    assert_eq!(runner.calls.lock().unwrap().len(), 1);
    *store.manifest.lock().unwrap() = Some(resolved(
        "id: myapp\nrun: {cmd: 'echo ok'}\nsetup: ['printf ok > branch.txt']\n",
    ));
    service.ensure_prepared(&id, false, &op).await.unwrap();
    assert!(
        store
            .records()
            .unwrap()
            .into_iter()
            .find(|(i, _)| i == &id)
            .unwrap()
            .1
            .checkout
            .setup
            .is_some()
    );
    assert_eq!(runner.calls.lock().unwrap().len(), 2);
    op.finish(OpOutcome::Succeeded(None));
}

#[tokio::test]
async fn instances_git_defaults_and_stat_refresh() {
    let t = Temp::new();
    let (root, _, _, _, service) = fixture(&t, EMPTY).await;
    fs::write(root.join("composer.lock"), "{}\n").unwrap();
    fs::write(root.join("uv.lock"), "lock\n").unwrap();
    fs::write(root.join("pnpm-lock.yaml"), "lock\n").unwrap();
    let defaults = lantern_daemon::instances::deps::default_setup(&root.join("apps/web"), &root).unwrap();
    assert_eq!(
        defaults.iter().map(|d| d.command.as_str()).collect::<Vec<_>>(),
        vec![
            "composer install --no-interaction --prefer-dist",
            "uv sync --frozen",
            "pnpm install --frozen-lockfile --prefer-offline"
        ]
    );
    let hash = lantern_daemon::instances::deps::lock_hash(&root.join("apps/web"), &root).unwrap();
    fs::write(root.join("uv.lock"), "changed\n").unwrap();
    assert_ne!(
        hash,
        lantern_daemon::instances::deps::lock_hash(&root.join("apps/web"), &root).unwrap()
    );
    let common = root.join(".git");
    assert!(service.tick(&common, false).await.unwrap());
    // The first discovery adds the primary to the fingerprint's known checkout list.
    let _ = service.tick(&common, false).await.unwrap();
    assert!(!service.tick(&common, false).await.unwrap());
    git_cmd(&root, &["branch", "fingerprint"]);
    let added = t.0.join("newtree");
    git_cmd(
        &root,
        &["worktree", "add", "--", added.to_str().unwrap(), "fingerprint"],
    );
    assert!(service.tick(&common, false).await.unwrap());
    assert!(
        service
            .observation(&InstanceId::parse("myapp@fingerprint").unwrap())
            .is_some()
    );
    assert_eq!(InstanceService::refresh_interval(true), Duration::from_secs(5));
    assert_eq!(InstanceService::refresh_interval(false), Duration::from_secs(30));
}

#[tokio::test]
async fn instances_git_user_worktree_inside_managed_directory_stays_user_owned() {
    let t = Temp::new();
    let (root, _, store, _, service) = fixture(&t, EMPTY).await;
    git_cmd(&root, &["branch", "intruder"]);
    let path = t.0.join("managed/mono/intruder");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    git_cmd(
        &root,
        &["worktree", "add", "--", path.to_str().unwrap(), "intruder"],
    );
    service.refresh_repo(&root.join(".git")).await.unwrap();
    let id = InstanceId::parse("myapp@intruder").unwrap();
    let record = store
        .records()
        .unwrap()
        .into_iter()
        .find(|(i, _)| i == &id)
        .unwrap()
        .1;
    assert_eq!(record.checkout.kind, lantern_core::model::CheckoutKind::Worktree);
    let result = service
        .remove_checkout(
            &id,
            RemoveOpts {
                force: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(result.forgotten);
    assert!(path.is_dir());
}

#[tokio::test]
async fn instances_gc_apply_only_clean_managed_checkouts() {
    let t = Temp::new();
    let (root, _, store, _, service) = fixture(&t, EMPTY).await;
    git_cmd(&root, &["branch", "stale-managed"]);
    git_cmd(&root, &["branch", "stale-user"]);
    let user = t.0.join("user-wt");
    git_cmd(
        &root,
        &["worktree", "add", "--", user.to_str().unwrap(), "stale-user"],
    );
    let op = operation();
    let created = service
        .run_branch(req(BranchTarget::Branch("stale-managed".into())), &op)
        .await
        .unwrap();
    let record = store
        .records()
        .unwrap()
        .into_iter()
        .find(|(i, _)| i == &created.instance)
        .unwrap()
        .1;
    fs::write(record.checkout.path.join("app.txt"), "dirty").unwrap();
    let project = ProjectId::parse("myapp").unwrap();
    let first = service.gc_apply(&project, 0).await.unwrap();
    assert!(first.removed.is_empty());
    assert!(record.checkout.path.exists());
    git_cmd(&record.checkout.path, &["restore", "--", "app.txt"]);
    let second = service.gc_apply(&project, 0).await.unwrap();
    assert!(second.removed.contains(&created.instance));
    assert!(user.exists());
    op.finish(OpOutcome::Succeeded(None));
}

#[tokio::test]
async fn instances_primary_stamp_does_not_install_automatically() {
    let t = Temp::new();
    let (root, _, _, runner, service) = fixture(
        &t,
        "id: myapp\nrun: {cmd: 'echo ok'}\nsetup: ['printf ok > branch.txt']\n",
    )
    .await;
    service.refresh_repo(&root.join(".git")).await.unwrap();
    let id = InstanceId::primary(&ProjectId::parse("myapp").unwrap());
    assert!(!service.setup_stale(&id).unwrap());
    fs::write(root.join("apps/web/.node-version"), "22\n").unwrap();
    assert!(service.setup_stale(&id).unwrap());
    let op = operation();
    service.ensure_prepared(&id, false, &op).await.unwrap();
    assert_eq!(runner.calls.lock().unwrap().len(), 0);
    assert!(service.setup_stale(&id).unwrap());
    op.finish(OpOutcome::Succeeded(None));
}

#[tokio::test]
async fn instances_git_ambiguous_remote_uses_configured_preference() {
    let t = Temp::new();
    let (root, origin, _, _, service) = fixture(&t, EMPTY).await;
    let other = t.0.join("other.git");
    git_cmd(&t.0, &["init", "--bare", other.to_str().unwrap()]);
    git_cmd(&root, &["remote", "add", "other", other.to_str().unwrap()]);
    git_cmd(&root, &["push", "other", "main"]);
    let oid = git_cmd(&root, &["rev-parse", "HEAD"]);
    for remote in [&origin, &other] {
        git_cmd(
            &t.0,
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "update-ref",
                "refs/heads/choice",
                &oid,
            ],
        );
    }
    git_cmd(&root, &["fetch", "origin"]);
    git_cmd(&root, &["fetch", "other"]);
    git_cmd(&root, &["config", "checkout.defaultRemote", "other"]);
    let op = operation();
    let result = service
        .run_branch(req(BranchTarget::Branch("choice".into())), &op)
        .await
        .unwrap();
    assert!(
        matches!(result.outcome, Outcome::CreatedWorktree(git::AddMode::TrackRemote { remote, .. }) if remote == "other")
    );
    op.finish(OpOutcome::Succeeded(None));
}
