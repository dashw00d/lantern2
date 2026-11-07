use std::{fs, io, net::SocketAddr, path::PathBuf, sync::Arc};

use lantern_core::{
    Timestamp,
    detect::Templates,
    dirs::Dirs,
    ids::{InstanceId, ProjectId},
    manifest::raw::RawManifest,
    model::{CheckoutKind, ProjectSource},
    settings::Settings,
    state::{CheckoutPhase, CheckoutRecord, InstanceRecord, ProjectRecord},
};
use lantern_daemon::{
    app::adapters::RegistryAdapter,
    discovery::PortBinding,
    events::EventBus,
    instances::InstanceStore,
    registry::RegistryHandle,
    runtime::seams::{PortProbe, RuntimeStore},
};

#[derive(Debug)]
struct Free;
impl PortProbe for Free {
    fn port_binding(&self, _: u16, _: Option<&str>) -> io::Result<PortBinding> {
        Ok(PortBinding::Free)
    }
    fn unit_listeners(&self, _: &str) -> io::Result<Vec<SocketAddr>> {
        Ok(Vec::new())
    }
}
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn app_adapters_allocate_before_setup_and_compare_leases_atomically() {
    let tmp = Temp(std::env::temp_dir().join(format!("lantern-assembly-adapters-{}", std::process::id())));
    fs::create_dir_all(&tmp.0).unwrap();
    let dirs = Dirs {
        config: tmp.0.join("config"),
        state: tmp.0.join("state"),
        data: tmp.0.join("data"),
    };
    let root = tmp.0.join("project");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("lantern.yaml"),
        "id: sample\ntype: proxy\nrun: {cmd: 'python3 -m http.server ${PORT}'}\n",
    )
    .unwrap();
    let mut settings = Settings::default();
    settings.ports.range_start = 39650;
    settings.ports.range_end = 39659;
    let (registry, _) = RegistryHandle::open(
        &dirs,
        settings.clone(),
        Templates::default(),
        tmp.0.clone(),
        EventBus::new(),
    )
    .unwrap();
    let project = ProjectId::parse("sample").unwrap();
    let now = Timestamp::now();
    registry
        .upsert_project(ProjectRecord {
            id: project.clone(),
            aliases: vec![],
            root: root.clone(),
            repo: None,
            source: ProjectSource::Manifest,
            registered_at: now,
            last_seen_at: now,
            missing: false,
            overrides: RawManifest::default(),
        })
        .unwrap();
    let adapter = RegistryAdapter::new(registry.clone(), Arc::new(Free), settings);
    let id = InstanceId::parse("sample@branch").unwrap();
    InstanceStore::upsert(
        &adapter,
        &id,
        InstanceRecord {
            project,
            checkout: CheckoutRecord {
                path: root,
                kind: CheckoutKind::Worktree,
                phase: CheckoutPhase::Ready,
                created_at: now,
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
        },
    )
    .unwrap();
    let port = InstanceStore::port(&adapter, &id).unwrap().unwrap();
    assert!((39650..=39659).contains(&port));
    let expected = RuntimeStore::port_leases(&adapter).unwrap();
    let mut changed = expected.clone();
    changed.insert(id.clone(), 39659);
    assert!(RuntimeStore::compare_port_leases(&adapter, &expected, changed.clone()).unwrap());
    assert!(!RuntimeStore::compare_port_leases(&adapter, &expected, expected.clone()).unwrap());
    assert_eq!(RuntimeStore::port_leases(&adapter).unwrap(), changed);
    registry.flush().await.unwrap();
}
