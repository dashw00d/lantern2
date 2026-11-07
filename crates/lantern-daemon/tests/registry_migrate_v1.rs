use lantern_core::{
    detect::Templates,
    dirs::Dirs,
    ids::{InstanceId, ProjectId},
    model::ProjectSource,
    profiles::Profiles,
    settings::Settings,
};
use lantern_daemon::{
    events::EventBus,
    registry::{RegistryHandle, migrate_v1::ActionKind},
};
use serde_json::json;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "lantern-registry-v1-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn dirs(&self) -> Dirs {
        Dirs {
            config: self.0.join("v2-config"),
            state: self.0.join("v2-state"),
            data: self.0.join("v2-data"),
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
async fn registry_migrate_v1_dry_run_apply_repeat_and_force() {
    let t = Temp::new();
    let source = t.mkdir("v1");
    let manifest = t.mkdir("projects/app");
    let manual = t.mkdir("projects/manual");
    fs::create_dir_all(manual.join(".git")).unwrap();
    fs::write(
        manifest.join("lantern.yaml"),
        "id: app\nname: Original\ntype: proxy\nrun: {cmd: 'npm run dev', env: {KEEP: manifest}}\n",
    )
    .unwrap();
    fs::write(source.join("state.json"), serde_json::to_vec_pretty(&json!({
        "projects": {
            "Legacy-App": {"name":"Legacy-App", "id":"App", "path":manifest, "type":"proxy", "domain":"Legacy-App.glow", "run_cmd":"npm run dev", "run_env":{"KEEP":"edited"}, "pid":321, "port":41000, "status":"running"},
            "My Manual": {"name":"My Manual", "path":manual, "type":"static", "description":"kept"},
            "lantern": {"name":"lantern", "path":source, "id":"lantern"},
            "relative": {"name":"relative", "path":"."},
            "missing": {"name":"missing", "path":t.0.join("gone")}
        }, "port_assignments":{"Legacy-App":41000}, "service_states":{"redis":"running"}
    })).unwrap()).unwrap();
    let input_bytes = fs::read(source.join("state.json")).unwrap();
    fs::write(source.join("settings.json"), r#"{"tld":".glow","workspace_roots":["/fixture/work"],"port_range_start":41000,"port_range_end":42000,"active_profile":"custom"}"#).unwrap();
    let profiles = t.mkdir("v1/profiles");
    fs::write(profiles.join("custom.json"), r#"{"name":"custom","description":"Custom","services":["redis"],"auto_start_projects":["app"],"env":{"MODE":"test"},"port_range_start":29100,"port_range_end":29150}"#).unwrap();
    let templates = t.mkdir("v1/templates");
    fs::write(templates.join("custom.yml"), "run: {cmd: 'npm start'}\n").unwrap();
    fs::write(templates.join("broken.yml"), "run: [bad\n").unwrap();
    fs::create_dir_all(&t.dirs().config).unwrap();
    fs::write(
        t.dirs().config.join("config.toml"),
        "# keep me\n[domains]\ntld = 'local'\n",
    )
    .unwrap();
    let (registry, _) = RegistryHandle::open(
        &t.dirs(),
        Settings::default(),
        Templates::default(),
        t.0.clone(),
        EventBus::new(),
    )
    .unwrap();
    let dry = registry.migrate_v1(&source, &t.dirs(), true, false).unwrap();
    assert_eq!(
        dry.actions
            .iter()
            .filter(|a| a.kind == ActionKind::Added && ["Legacy-App", "My Manual"].contains(&a.what.as_str()))
            .count(),
        2
    );
    assert!(
        dry.actions
            .iter()
            .any(|a| a.what == "lantern" && a.kind == ActionKind::Skipped)
    );
    assert!(
        dry.actions
            .iter()
            .any(|a| a.what == "active_profile" && a.kind == ActionKind::Added)
    );
    assert!(registry.state_snapshot().projects.is_empty());
    assert!(!t.dirs().config.join("profiles/custom.yaml").exists());
    assert_eq!(
        fs::read_to_string(t.dirs().config.join("config.toml")).unwrap(),
        "# keep me\n[domains]\ntld = 'local'\n"
    );
    registry.migrate_v1(&source, &t.dirs(), false, false).unwrap();
    registry.flush().await.unwrap();
    let snapshot = registry.state_snapshot();
    assert_eq!(snapshot.projects.len(), 2);
    assert_eq!(snapshot.repos.len(), 1);
    assert_eq!(
        snapshot.projects[&ProjectId::parse("my-manual").unwrap()]
            .repo
            .as_ref()
            .unwrap()
            .common_dir,
        manual.join(".git")
    );
    assert_eq!(snapshot.active_profile.as_deref(), Some("custom"));
    assert!(snapshot.port_leases.is_empty());
    assert_eq!(
        registry.lookup_project("Legacy-App"),
        Some(ProjectId::parse("app").unwrap())
    );
    let primary = InstanceId::parse("app").unwrap();
    let (resolved, _) = registry.manifest(&primary).unwrap();
    assert_eq!(resolved.run.cmd.as_ref().unwrap().source(), "npm run dev");
    assert_eq!(resolved.run.env["KEEP"].source(), "edited");
    assert_eq!(
        registry
            .project_record(&ProjectId::parse("my-manual").unwrap())
            .unwrap()
            .source,
        ProjectSource::Manual
    );
    let cfg = fs::read_to_string(t.dirs().config.join("config.toml")).unwrap();
    assert!(cfg.contains("# keep me"));
    assert!(cfg.contains("'local'"));
    assert!(cfg.contains("41000"));
    let (profiles, _) = Profiles::load_dir(&t.dirs().config.join("profiles")).unwrap();
    let profile = profiles.get("custom").unwrap();
    assert_eq!(profile.auto_start, ["app"]);
    assert_eq!(profile.port_range, Some([29100, 29150]));
    assert!(t.dirs().config.join("templates/custom.yaml").exists());
    assert!(!t.dirs().config.join("templates/broken.yaml").exists());
    assert_eq!(fs::read(source.join("state.json")).unwrap(), input_bytes);
    let again = registry.migrate_v1(&source, &t.dirs(), false, false).unwrap();
    assert!(
        again
            .actions
            .iter()
            .any(|a| a.what == "Legacy-App" && a.kind == ActionKind::AlreadyPresent)
    );
    assert_eq!(registry.state_snapshot().projects.len(), 2);
    let forced = registry.migrate_v1(&source, &t.dirs(), true, true).unwrap();
    assert!(
        forced
            .actions
            .iter()
            .any(|a| a.what == "Legacy-App" && a.kind == ActionKind::Updated)
    );
}

#[tokio::test]
async fn registry_migrate_v1_overlapping_source_refused() {
    let t = Temp::new();
    let dirs = t.dirs();
    let source = t.mkdir("source");
    fs::write(source.join("state.json"), "{\"projects\":{}}").unwrap();
    let (reg, _) = RegistryHandle::open(
        &dirs,
        Settings::default(),
        Templates::default(),
        t.0.clone(),
        EventBus::new(),
    )
    .unwrap();
    let overlap = Dirs {
        config: source.clone(),
        state: dirs.state.clone(),
        data: dirs.data.clone(),
    };
    assert!(reg.migrate_v1(&source, &overlap, true, false).is_err());
}
