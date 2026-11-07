use lantern_core::{
    Timestamp,
    api::Event,
    detect::Templates,
    dirs::Dirs,
    ids::{InstanceId, ProjectId},
    manifest::raw::RawManifest,
    model::{CheckoutKind, ExternalState, HealthState, InstanceStatus, ProjectSource, Scheme},
    settings::Settings,
    state::{CheckoutPhase, CheckoutRecord, InstanceRecord, ProjectRecord},
};
use lantern_daemon::{
    events::EventBus,
    registry::{Error, ExternalChange, RegistryHandle},
    runtime::ports::{PortAvailability, PortRange},
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "lantern-registry-model-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn dirs(&self) -> Dirs {
        Dirs {
            config: self.0.join("config"),
            state: self.0.join("state"),
            data: self.0.join("data"),
        }
    }
    fn project(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join("workspace").join(name);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("lantern.yaml"), text).unwrap();
        path
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn project(id: &str, path: &Path) -> ProjectRecord {
    let now = Timestamp::now();
    ProjectRecord {
        id: ProjectId::parse(id).unwrap(),
        aliases: vec!["Legacy Name".into()],
        root: path.to_owned(),
        repo: None,
        source: ProjectSource::Manifest,
        registered_at: now,
        last_seen_at: now,
        missing: false,
        overrides: RawManifest::default(),
    }
}
fn checkout(project: &ProjectId, path: &Path) -> InstanceRecord {
    InstanceRecord {
        project: project.clone(),
        checkout: CheckoutRecord {
            path: path.to_owned(),
            kind: CheckoutKind::Worktree,
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
    }
}
fn external(port: u16) -> ExternalState {
    ExternalState {
        port,
        upstream: format!("127.0.0.1:{port}").parse().unwrap(),
        scheme: Scheme::Http,
        pid: Some(123),
        label: "vite".into(),
        extra_ports: vec![],
        since: Timestamp::now(),
    }
}
fn registry(temp: &Temp) -> (RegistryHandle, EventBus) {
    let bus = EventBus::new();
    let (registry, _) = RegistryHandle::open(
        &temp.dirs(),
        Settings::default(),
        Templates::default(),
        temp.0.clone(),
        bus.clone(),
    )
    .unwrap();
    (registry, bus)
}

#[tokio::test]
async fn registry_model_views_aliases_revisions_and_checkouts() {
    let temp = Temp::new();
    let root = temp.project(
        "app",
        "id: app\ntype: proxy\nrun: {cmd: 'npm run dev -- --port ${PORT}'}\n",
    );
    let (reg, bus) = registry(&temp);
    let mut events = bus.subscribe();
    let mut index = reg.subscribe_checkout_index();
    let record = project("app", &root);
    let id = InstanceId::primary(&record.id);
    reg.upsert_project(record.clone()).unwrap();
    assert_eq!(reg.lookup_project("Legacy Name"), Some(record.id.clone()));
    assert!(matches!(events.try_recv().unwrap(), Event::ProjectUpdated { .. }));
    assert!(index.has_changed().unwrap());
    index.borrow_and_update();
    assert!(index.borrow().lookup(&root).is_some());
    let first = reg.instances().pop().unwrap();
    assert_eq!(first.domain, "app.glow");
    assert_eq!(first.manifest.id, record.id);
    let revision = first.manifest.manifest_rev;
    let branch = temp.project("branch", "id: other\ntype: static\n");
    let branch_id = InstanceId::derived(&record.id, "feat").unwrap();
    reg.upsert_instance(branch_id.clone(), checkout(&record.id, &branch))
        .unwrap();
    assert!(index.has_changed().unwrap());
    let b = reg.instances().into_iter().find(|v| v.id == branch_id).unwrap();
    assert!(!b.visible);
    assert_eq!(b.domain, "feat.app.glow");
    assert_eq!(b.manifest.id, record.id);
    reg.record_started(&branch_id, Timestamp::now()).unwrap();
    assert!(reg.instance_record(&branch_id).unwrap().claimed());
    reg.set_pinned(&branch_id, false).unwrap();
    assert!(
        !reg.instances()
            .into_iter()
            .find(|v| v.id == branch_id)
            .unwrap()
            .visible
    );
    fs::write(root.join("lantern.yaml"), "id: renamed\ntype: static\n").unwrap();
    reg.reload_project(&record.id).unwrap();
    let (resolved, rev) = reg.manifest(&id).unwrap();
    assert_ne!(revision, rev);
    assert_eq!(resolved.id, record.id);
    assert!(resolved.diagnostics.iter().any(|d| d.code == "id_changed"));
    assert_eq!(reg.projects()[0].instance_count, 2);
    assert!(ProjectId::parse("lantern").is_err());
    let collision = ProjectRecord {
        id: ProjectId::parse("other").unwrap(),
        ..record.clone()
    };
    assert!(matches!(reg.upsert_project(collision), Err(Error::Conflict(_))));
    reg.flush().await.unwrap();
}

#[tokio::test]
async fn registry_model_external_rules_and_runtime_events() {
    let temp = Temp::new();
    let root = temp.project("app", "id: app\ntype: static\n");
    let (reg, bus) = registry(&temp);
    let record = project("app", &root);
    let id = InstanceId::primary(&record.id);
    reg.upsert_project(record).unwrap();
    for status in [
        InstanceStatus::Stopped,
        InstanceStatus::Crashed,
        InstanceStatus::NeedsConfig,
        InstanceStatus::External,
    ] {
        reg.update_runtime(&id, |r| r.status = status).unwrap();
        reg.apply_external(ExternalChange::Appeared {
            instance: id.clone(),
            external: external(3001),
        })
        .unwrap();
        let runtime = reg.instances().remove(0).runtime;
        assert_eq!(runtime.status, InstanceStatus::External);
        assert_eq!(runtime.port, Some(3001));
        assert_eq!(runtime.message.as_deref(), Some("started outside Lantern"));
        reg.apply_external(ExternalChange::Updated {
            instance: id.clone(),
            external: external(3002),
        })
        .unwrap();
        assert_eq!(reg.instances()[0].runtime.port, Some(3002));
        reg.apply_external(ExternalChange::Gone { instance: id.clone() })
            .unwrap();
        let runtime = &reg.instances()[0].runtime;
        assert_eq!(runtime.status, InstanceStatus::Stopped);
        assert!(runtime.external.is_none());
        assert!(runtime.port.is_none());
    }
    for status in [
        InstanceStatus::Preparing,
        InstanceStatus::Starting,
        InstanceStatus::Running,
        InstanceStatus::Unhealthy,
        InstanceStatus::Stopping,
    ] {
        reg.update_runtime(&id, |r| r.status = status).unwrap();
        let before = reg.instances()[0].runtime.clone();
        reg.apply_external(ExternalChange::Appeared {
            instance: id.clone(),
            external: external(3001),
        })
        .unwrap();
        reg.apply_external(ExternalChange::Gone { instance: id.clone() })
            .unwrap();
        assert_eq!(reg.instances()[0].runtime, before);
    }
    let mut events = bus.subscribe();
    reg.update_runtime(&id, |r| r.health.checked_at = Some(Timestamp::now()))
        .unwrap();
    assert!(events.try_recv().is_err());
    reg.update_runtime(&id, |r| r.health.state = HealthState::Healthy)
        .unwrap();
    assert!(matches!(
        events.try_recv().unwrap(),
        Event::InstanceUpdated { .. }
    ));
    reg.set_adopt(&id, true).unwrap();
    assert!(reg.subscribe_checkout_index().borrow().any_adopted);
    reg.set_activated(&id, true).unwrap();
    assert!(reg.instance_record(&id).unwrap().activated);
}

#[tokio::test]
async fn registry_model_port_leases_are_persisted_transactionally() {
    let temp = Temp::new();
    let root = temp.project("app", "id: app\ntype: static\n");
    let (reg, _) = registry(&temp);
    let record = project("app", &root);
    let id = InstanceId::primary(&record.id);
    reg.upsert_project(record).unwrap();
    let result = reg
        .update_port_leases(|leases| {
            leases.allocate(
                &id,
                PortRange {
                    start: 29100,
                    end: 29101,
                },
                &[],
                |_| PortAvailability::<()>::Free,
            )
        })
        .unwrap();
    assert_eq!(result.port, 29100);
    reg.flush().await.unwrap();
    assert_eq!(reg.state_snapshot().port_leases[&id].port, 29100);
    let old = reg.state_snapshot().port_leases;
    let failure: Result<(), &str> = reg.update_port_leases(|l| {
        l.release(&id);
        Err("no")
    });
    assert_eq!(failure, Err("no"));
    assert_eq!(reg.state_snapshot().port_leases, old);
    reg.remove_project(&id.project()).unwrap();
    assert!(reg.state_snapshot().port_leases.is_empty());
}
