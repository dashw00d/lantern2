use lantern_core::{
    api::{OperationStatus, PatchProjectRequest, PrepareRequest, RegisterProjectRequest, ReloadRequest},
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
    let adapter = Arc::new(
        RegistryAdapter::new(registry.clone(), probe.clone(), settings.clone())
            .with_profile_dir(dirs.config.join("profiles")),
    );
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
    let (profile, created) = app
        .put_profile(
            "testing".into(),
            serde_json::json!({"env":{"APP_STAGE":"from-${PROJECT}-${PORT}"}, "port_range":[39660,39679]}),
        )
        .await
        .unwrap();
    assert!(created);
    assert_eq!(profile["name"], "testing");
    assert!(app.activate_profile("testing".into()).await.unwrap().is_empty());
    let on = app.start(id.clone()).await.unwrap();
    let outcome = tracker.wait(&on.id, Duration::from_secs(8)).await.unwrap();
    assert_eq!(outcome.status, OperationStatus::Succeeded, "{:?}", outcome.error);
    let view = app.instance(id.clone()).await.unwrap();
    assert_eq!(view.runtime.status, InstanceStatus::Running);
    assert!((39660..=39679).contains(&view.runtime.port.unwrap()));
    let script = fs::read_to_string(temp.0.join(format!(
        "run/lantern-test-assembly-inst-{}.service.sh",
        id.safe_name()
    )))
    .unwrap();
    assert!(
        script.contains("APP_STAGE")
            && script.contains(&format!("from-sample-{}", view.runtime.port.unwrap()))
    );
    assert_eq!(app.instances(InstanceFilter::default()).await.unwrap().len(), 1);
    assert_eq!(
        app.check_health(id.clone()).await.unwrap().state,
        lantern_core::model::HealthState::Healthy
    );
    let off = app.stop(id.clone()).await.unwrap();
    let outcome = tracker.wait(&off.id, Duration::from_secs(8)).await.unwrap();
    assert_eq!(outcome.status, OperationStatus::Succeeded, "{:?}", outcome.error);
    assert_eq!(
        app.instance(id.clone()).await.unwrap().runtime.status,
        InstanceStatus::Stopped
    );
    fs::write(
        temp.0.join("project/lantern.yaml"),
        "id: sample\ntype: proxy\nrun: {cmd: 'python3 -m http.server ${PORT}'}\nsetup: ['sleep 30']\n",
    )
    .unwrap();
    app.reload_project(
        project_id(&id),
        ReloadRequest {
            discard_overrides: false,
        },
    )
    .await
    .unwrap();
    let prepare = app
        .prepare(id.clone(), PrepareRequest { force: true })
        .await
        .unwrap();
    for _ in 0..100 {
        if fake.calls().iter().any(|c| c.contains("-setup-")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        fake.calls().iter().any(|c| c.contains("-setup-")),
        "setup did not start: {:?}",
        fake.calls()
    );
    let joined = app
        .prepare(id.clone(), PrepareRequest { force: true })
        .await
        .unwrap();
    assert_eq!(prepare.id, joined.id);
    assert_eq!(
        app.start(id.clone()).await.unwrap_err().code,
        "operation_in_progress"
    );
    let stop = app.stop(id.clone()).await.unwrap();
    let first = tracker.wait(&prepare.id, Duration::from_secs(8)).await.unwrap();
    let second = tracker.wait(&stop.id, Duration::from_secs(8)).await.unwrap();
    assert_eq!(first.status, OperationStatus::Cancelled);
    assert_eq!(second.status, OperationStatus::Succeeded, "{:?}", second.error);
    assert!(
        fake.calls()
            .iter()
            .any(|c| c.starts_with("stop ") && c.contains("-setup-"))
    );
    let renamed = lantern_core::ids::ProjectId::parse("renamed").unwrap();
    let renamed_detail = app
        .patch_project(
            project_id(&id),
            PatchProjectRequest {
                id: Some(renamed.clone()),
                name: None,
                enabled: None,
                tags: None,
                overrides: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed_detail.summary.project.id, renamed);
    assert!(
        renamed_detail
            .summary
            .project
            .aliases
            .contains(&"sample".to_owned())
    );
    assert_eq!(registry.lookup_project("sample"), Some(renamed.clone()));
    assert!(
        registry
            .state_snapshot()
            .port_leases
            .contains_key(&InstanceId::primary(&renamed))
    );
    fs::create_dir_all(&app.dirs.config).unwrap();
    fs::write(
        app.dirs.config.join("config.toml"),
        "# preserve this comment\n[runtime]\nstartup_timeout_secs = 60\n",
    )
    .unwrap();
    let patched = app
        .patch_settings(serde_json::json!({"runtime":{"startup_timeout_secs":25}}))
        .await
        .unwrap();
    assert_eq!(patched.settings["runtime"]["startup_timeout_secs"], 25);
    assert_eq!(patched.restart_required, vec!["runtime.startup_timeout_secs"]);
    assert!(
        fs::read_to_string(app.dirs.config.join("config.toml"))
            .unwrap()
            .contains("# preserve this comment")
    );
    assert!(app.reload_settings().await.is_err());
    shutdown.cancel();
    registry.flush().await.unwrap();
}

fn project_id(id: &InstanceId) -> lantern_core::ids::ProjectId {
    id.project()
}
