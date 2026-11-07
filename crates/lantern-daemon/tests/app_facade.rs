use lantern_core::{
    api::{OperationStatus, RegisterProjectRequest},
    detect::Templates,
    dirs::Dirs,
    ids::InstanceId,
    model::InstanceStatus,
    settings::Settings,
};
use lantern_daemon::{
    api::facade::{DaemonFacade, InstanceFilter},
    app::{
        adapters::{CheckoutPreparer, RegistryAdapter, RuntimeOneShot},
        discovery::Discovery,
        facade::AppFacade,
    },
    discovery::{
        PortBinding, Scanner,
        hide::HideSettings,
        matcher::CheckoutIndex,
        procinfo::ProcRoot,
        source::{RawSocket, SocketSource, SocketSourceKind},
    },
    events::EventBus,
    git::Git,
    instances::InstanceService,
    operations::OperationTracker,
    registry::RegistryHandle,
    runtime::{
        Runtime, RuntimeConfig,
        seams::{PortProbe, ScannerProbe},
        systemd::{FakeSystemd, UnitNames},
    },
};
use std::{
    fs, io,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

struct Empty;
impl SocketSource for Empty {
    fn listening_tcp(&mut self) -> io::Result<Vec<RawSocket>> {
        Ok(vec![])
    }
}
struct Ready;
impl PortProbe for Ready {
    fn port_binding(&self, port: u16, unit: Option<&str>) -> io::Result<PortBinding> {
        Ok(if unit.is_some() {
            PortBinding::BoundByUnit {
                pid: 1234,
                addrs: vec![SocketAddr::from((Ipv4Addr::LOCALHOST, port))],
            }
        } else {
            PortBinding::Free
        })
    }
    fn unit_listeners(&self, _: &str) -> io::Result<Vec<SocketAddr>> {
        Ok(vec![])
    }
}
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn app_facade_starts_and_stops_real_registry_with_fake_systemd() {
    let temp = Temp(std::env::temp_dir().join(format!("lantern-assembly-facade-{}", std::process::id())));
    let root = temp.0.join("project");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("lantern.yaml"),
        "id: sample\ntype: proxy\nrun: {cmd: 'python3 -m http.server ${PORT}'}\n",
    )
    .unwrap();
    fs::create_dir_all(temp.0.join("cache")).unwrap();
    fs::write(temp.0.join("cache/env.sh"), "export PATH='/usr/bin:/bin'\n").unwrap();
    let dirs = Dirs {
        config: temp.0.join("config"),
        state: temp.0.join("state"),
        data: temp.0.join("data"),
    };
    let mut settings = Settings::default();
    settings.ports.range_start = 39660;
    settings.ports.range_end = 39669;
    settings.runtime.startup_timeout_secs = 5;
    settings.health.interval_secs = 60;
    let bus = EventBus::new();
    let tracker = OperationTracker::new(bus.clone());
    let (registry, _) =
        RegistryHandle::open(&dirs, settings.clone(), Templates::default(), temp.0.clone(), bus).unwrap();
    let probe: Arc<dyn PortProbe> = Arc::new(Ready);
    let adapter = Arc::new(RegistryAdapter::new(
        registry.clone(),
        probe.clone(),
        settings.clone(),
    ));
    let oneshot = Arc::new(RuntimeOneShot::default());
    let git = Git::detect().await.unwrap();
    let service = Arc::new(InstanceService::new(
        git.clone(),
        adapter.clone(),
        oneshot.clone(),
        dirs.data.join("worktrees"),
        2,
        Duration::from_secs(10),
    ));
    let names = UnitNames {
        instance_prefix: "lantern-test-assembly-inst-".into(),
        operation_prefix: "lantern-test-assembly-op-".into(),
        slice: "app-lantern-test.slice".into(),
    };
    let fake = Arc::new(FakeSystemd::default());
    fake.set_auto_start(true);
    let runtime = Arc::new(Runtime::with_preparer(
        RuntimeConfig {
            settings: settings.clone(),
            run_dir: temp.0.join("run"),
            shell_env_file: temp.0.join("cache/env.sh"),
            home: temp.0.clone(),
            names: names.clone(),
            poll_interval: Duration::from_millis(20),
            readiness_interval: Duration::from_millis(20),
            mismatch_after: Duration::from_millis(100),
        },
        adapter,
        probe,
        fake.clone(),
        tracker.clone(),
        Arc::new(CheckoutPreparer(service.clone())),
    ));
    oneshot.bind(&runtime).unwrap();
    let scanner = Scanner::with_source(
        ProcRoot::new(temp.0.join("proc")),
        CheckoutIndex::new([], &temp.0),
        HideSettings::default(),
        Box::new(Empty),
        SocketSourceKind::ProcNetTcp,
    );
    let shutdown = CancellationToken::new();
    let discovery = Discovery::spawn(
        Arc::new(ScannerProbe::new(scanner)),
        registry.clone(),
        runtime.clone(),
        names,
        shutdown.clone(),
    );
    let app = AppFacade::new(
        registry.clone(),
        runtime,
        service,
        git,
        discovery,
        tracker.clone(),
        settings,
        dirs,
        temp.0.clone(),
        "test".into(),
    );
    let project = app
        .register_project(RegisterProjectRequest {
            path: root,
            id: None,
            name: None,
            enabled: None,
            overrides: None,
            write_manifest: false,
        })
        .await
        .unwrap();
    let id: InstanceId = project.primary;
    let on = app.start(id.clone()).await.unwrap();
    let outcome = tracker.wait(&on.id, Duration::from_secs(8)).await.unwrap();
    assert_eq!(outcome.status, OperationStatus::Succeeded, "{:?}", outcome.error);
    let view = app.instance(id.clone()).await.unwrap();
    assert_eq!(view.runtime.status, InstanceStatus::Running);
    assert!((39660..=39669).contains(&view.runtime.port.unwrap()));
    assert_eq!(app.instances(InstanceFilter::default()).await.unwrap().len(), 1);
    let off = app.stop(id.clone()).await.unwrap();
    let outcome = tracker.wait(&off.id, Duration::from_secs(8)).await.unwrap();
    assert_eq!(outcome.status, OperationStatus::Succeeded, "{:?}", outcome.error);
    assert_eq!(
        app.instance(id).await.unwrap().runtime.status,
        InstanceStatus::Stopped
    );
    shutdown.cancel();
    registry.flush().await.unwrap();
}
