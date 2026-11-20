use lantern_core::{
    Timestamp, detect::Templates, dirs::Dirs, ids::ProjectId, manifest::raw::RawManifest,
    model::ProjectSource, settings::Settings, state::ProjectRecord,
};
use lantern_daemon::{
    events::EventBus,
    registry::{RegistryHandle, scan::scan_roots},
};
use std::{
    fs,
    os::unix::fs::symlink,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "lantern-registry-scan-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn dirs(&self) -> Dirs {
        Dirs {
            config: self.0.join("config"),
            state: self.0.join("state"),
            data: self.0.join("data"),
        }
    }
    fn mkdir(&self, path: &str) -> PathBuf {
        let p = self.0.join(path);
        fs::create_dir_all(&p).unwrap();
        p
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn registry_scan_manifest_first_suggestions_dismissal_and_manual() {
    let t = Temp::new();
    let workspace = t.mkdir("workspace");
    let app = t.mkdir("workspace/app");
    fs::write(app.join("lantern.yaml"), "id: same\ntype: static\n").unwrap();
    let nested = t.mkdir("workspace/app/nested");
    fs::write(nested.join("lantern.yaml"), "id: hidden-nested\n").unwrap();
    let duplicate = t.mkdir("workspace/duplicate");
    fs::write(duplicate.join("lantern.yaml"), "id: same\ntype: static\n").unwrap();
    let next = t.mkdir("workspace/next");
    fs::write(next.join("package.json"), r#"{"dependencies":{"next":"1"}}"#).unwrap();
    let linked = t.mkdir("workspace/linked");
    fs::write(linked.join(".git"), "gitdir: ../x").unwrap();
    fs::write(linked.join("lantern.yaml"), "id: linked\n").unwrap();
    let excluded = t.mkdir("workspace/node_modules/dep");
    fs::write(excluded.join("lantern.yaml"), "id: excluded\n").unwrap();
    let secret = t.mkdir("workspace/.hidden");
    fs::write(secret.join("lantern.yaml"), "id: hidden\n").unwrap();
    symlink(&workspace, workspace.join("loop")).unwrap();
    let reserved = t.mkdir("workspace/reserved");
    fs::write(reserved.join("lantern.yaml"), "id: lantern\n").unwrap();
    let manual = t.mkdir("manual");
    let now = Timestamp::now();
    let mut settings = Settings::default();
    settings.workspaces.roots = vec![workspace.display().to_string()];
    settings.workspaces.depth = 3;
    let (registry, _) = RegistryHandle::open(
        &t.dirs(),
        settings.clone(),
        Templates::default(),
        t.0.clone(),
        EventBus::new(),
    )
    .unwrap();
    registry
        .upsert_project(ProjectRecord {
            id: ProjectId::parse("manual").unwrap(),
            aliases: vec![],
            root: manual.clone(),
            repo: None,
            source: ProjectSource::Manual,
            registered_at: now,
            last_seen_at: now,
            missing: false,
            overrides: RawManifest::default(),
        })
        .unwrap();
    let report = registry.scan_workspace(&t.dirs()).unwrap();
    assert_eq!(report.registered.len(), 2);
    assert!(report.warnings.iter().any(|s| s.contains("duplicate_id")));
    assert_eq!(report.suggestions.len(), 1);
    assert_eq!(report.suggestions[0].path, next);
    assert!(
        report
            .skipped
            .iter()
            .any(|s| s.path == linked && s.reason == "linked_worktree")
    );
    assert!(
        report
            .skipped
            .iter()
            .any(|s| s.path == reserved && s.reason.contains("reserved"))
    );
    let paths: Vec<_> = registry.projects().into_iter().map(|p| p.project.root).collect();
    assert!(!paths.contains(&nested));
    assert!(!paths.contains(&excluded));
    assert!(!paths.contains(&secret));
    registry.dismiss_suggestion(&next);
    registry.flush().await.unwrap();
    assert!(registry.scan_workspace(&t.dirs()).unwrap().suggestions.is_empty());
    fs::remove_dir_all(&manual).unwrap();
    registry.scan_workspace(&t.dirs()).unwrap();
    let record = registry
        .project_record(&ProjectId::parse("manual").unwrap())
        .unwrap();
    assert!(record.missing);
    assert_eq!(record.source, ProjectSource::Manual);
    let view = registry
        .instances()
        .into_iter()
        .find(|i| i.project == record.id)
        .unwrap();
    assert!(view.checkout.missing);
    assert_eq!(
        view.runtime.status,
        lantern_core::model::InstanceStatus::NeedsConfig
    );
    fs::create_dir_all(&manual).unwrap();
    registry.scan_workspace(&t.dirs()).unwrap();
    assert!(!registry.project_record(&record.id).unwrap().missing);
    let view = registry
        .instances()
        .into_iter()
        .find(|i| i.project == record.id)
        .unwrap();
    assert!(!view.checkout.missing);
    assert_eq!(view.runtime.status, lantern_core::model::InstanceStatus::Stopped);
    drop(registry);
}

#[tokio::test]
async fn registry_scan_depth_hidden_and_relocation() {
    let t = Temp::new();
    let workspace = t.mkdir("workspace");
    let old = t.mkdir("workspace/old");
    fs::write(old.join("lantern.yaml"), "id: mover\n").unwrap();
    let hidden = t.mkdir("workspace/.shown");
    fs::write(hidden.join("lantern.yml"), "id: shown\n").unwrap();
    let deep = t.mkdir("workspace/a/b");
    fs::write(deep.join("lantern.yaml"), "id: deep\n").unwrap();
    let mut settings = Settings::default();
    settings.workspaces.roots = vec![workspace.display().to_string()];
    settings.workspaces.depth = 1;
    settings.workspaces.include_hidden.push(".shown".into());
    let result = scan_roots(&settings.workspaces, &t.0, &t.dirs().data.join("worktrees"));
    assert!(result.manifests.iter().any(|h| h.path == hidden));
    assert!(!result.manifests.iter().any(|h| h.path == deep));
    let (reg, _) = RegistryHandle::open(
        &t.dirs(),
        settings,
        Templates::default(),
        t.0.clone(),
        EventBus::new(),
    )
    .unwrap();
    reg.scan_workspace(&t.dirs()).unwrap();
    let new = workspace.join("new");
    fs::rename(&old, &new).unwrap();
    let report = reg.scan_workspace(&t.dirs()).unwrap();
    assert!(report.warnings.iter().any(|s| s.contains("relocated")));
    let id = ProjectId::parse("mover").unwrap();
    assert_eq!(reg.project_record(&id).unwrap().root, new);
    assert_eq!(
        reg.instances()
            .into_iter()
            .find(|v| v.project == id)
            .unwrap()
            .checkout
            .path,
        new
    );
}

#[tokio::test]
async fn registry_scan_listener_offer_is_confined_and_survives_scan() {
    let t = Temp::new();
    let app = t.mkdir("unscanned/app");
    fs::create_dir_all(app.join(".git")).unwrap();
    fs::write(app.join("package.json"), r#"{"dependencies":{"vite":"1"}}"#).unwrap();
    let nested = t.mkdir("unscanned/app/node_modules/tool");
    let mut settings = Settings::default();
    settings.workspaces.roots = vec![t.mkdir("empty-workspace").display().to_string()];
    let (reg, _) = RegistryHandle::open(
        &t.dirs(),
        settings,
        Templates::default(),
        t.0.clone(),
        EventBus::new(),
    )
    .unwrap();
    let suggestion = reg.offer_suggestion(&nested).unwrap().unwrap();
    assert_eq!(suggestion.path, app);
    assert_eq!(suggestion.framework, "vite");
    assert_eq!(reg.offer_suggestion(&nested).unwrap(), Some(suggestion.clone()));
    assert_eq!(reg.scan_workspace(&t.dirs()).unwrap().suggestions, [suggestion]);
    assert!(reg.offer_suggestion(&t.0).unwrap().is_none());
    reg.dismiss_suggestion(&app);
    assert!(reg.offer_suggestion(&nested).unwrap().is_none());
}

#[tokio::test]
async fn registry_scan_glob_ignore_monorepo_descent_and_repo_cleanup() {
    let t = Temp::new();
    let workspace = t.mkdir("workspace");
    let agents = t.mkdir("workspace/plexa-agents");
    fs::write(agents.join("package.json"), r#"{"dependencies":{"vite":"1"}}"#).unwrap();
    let mono = t.mkdir("workspace/mono");
    fs::create_dir_all(mono.join(".git")).unwrap();
    let web = t.mkdir("workspace/mono/web");
    fs::write(web.join("lantern.yaml"), "id: mono-web\ntype: static\n").unwrap();
    let api = t.mkdir("workspace/mono/api");
    fs::write(api.join("package.json"), r#"{"dependencies":{"astro":"5"}}"#).unwrap();

    let mut settings = Settings::default();
    settings.workspaces.roots = vec![workspace.display().to_string()];
    settings.workspaces.depth = 3;
    settings.workspaces.ignore = vec!["*-agents".into()];

    let (reg, _) = RegistryHandle::open(
        &t.dirs(),
        settings,
        Templates::default(),
        t.0.clone(),
        EventBus::new(),
    )
    .unwrap();
    let report = reg.scan_workspace(&t.dirs()).unwrap();

    // Glob ignore skipped plexa-agents.
    assert!(!report.suggestions.iter().any(|s| s.path == agents));
    // Monorepo subproject manifest was registered with parent git repo + subpath.
    let id = ProjectId::parse("mono-web").unwrap();
    let record = reg.project_record(&id).unwrap();
    let repo = record.repo.expect("monorepo git repo probed");
    assert_eq!(repo.common_dir, mono.join(".git").canonicalize().unwrap());
    assert_eq!(repo.subpath, PathBuf::from("web"));
    // Subproject without manifest was suggested.
    assert!(
        report
            .suggestions
            .iter()
            .any(|s| s.path == api && s.framework == "astro")
    );
    // Removing the project also prunes its unreferenced repo entry.
    reg.remove_project(&id).unwrap();
    assert!(reg.state_snapshot().repos.is_empty());
}
