use lantern_core::{
    api::{
        CreateInstanceRequest, OperationStatus, PatchProjectRequest, PrepareRequest, RegisterProjectRequest,
        ReloadRequest, RunTarget,
    },
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
        Runtime, RuntimeAdapters, RuntimeConfig,
        seams::{PortProbe, ScannerProbe, UnavailableRouter, UnavailableServices},
        systemd::{FakeSystemd, UnitNames},
    },
};
use std::{
    fs, io,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

struct Empty(Arc<AtomicUsize>);
impl SocketSource for Empty {
    fn listening_tcp(&mut self) -> io::Result<Vec<RawSocket>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
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
        "id: sample\ntype: proxy\nrun: {cmd: 'python3 -m http.server ${PORT}'}\ndocs: [README.md]\nendpoints: [{method: GET, path: /hello, description: Hello}]\n",
    )
    .unwrap();
    fs::write(root.join("README.md"), "Original documentation").unwrap();
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
    let (registry, _) = RegistryHandle::open(
        &dirs,
        settings.clone(),
        Templates::default(),
        temp.0.clone(),
        bus.clone(),
    )
    .unwrap();
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
        instance_prefix: format!("lantern-test-assembly-{}-fake-inst-", std::process::id()),
        operation_prefix: format!("lantern-test-assembly-{}-fake-op-", std::process::id()),
        slice: "app-lantern-test.slice".into(),
    };
    let fake = Arc::new(FakeSystemd::default());
    fake.set_auto_start(true);
    let runtime = Arc::new(Runtime::with_scoped_journal(
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
        RuntimeAdapters {
            preparer: Arc::new(CheckoutPreparer(service.clone())),
            router: Arc::new(UnavailableRouter),
            services: Arc::new(UnavailableServices),
        },
        true,
    ));
    oneshot.bind(&runtime).unwrap();
    let scan_count = Arc::new(AtomicUsize::new(0));
    let scanner = Scanner::with_source(
        ProcRoot::new(temp.0.join("proc")),
        CheckoutIndex::new([], &temp.0),
        HideSettings::default(),
        Box::new(Empty(scan_count.clone())),
        SocketSourceKind::ProcNetTcp,
    );
    let shutdown = CancellationToken::new();
    let discovery = Discovery::spawn(
        Arc::new(ScannerProbe::new(scanner)),
        registry.clone(),
        runtime.clone(),
        names.clone(),
        shutdown.clone(),
    );
    for _ in 0..50 {
        if discovery.cached().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(discovery.cached().is_some());
    let before = scan_count.load(Ordering::SeqCst);
    let mut requests = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let discovery = discovery.clone();
        requests.spawn(async move { discovery.scan().await.unwrap() });
    }
    while let Some(result) = requests.join_next().await {
        result.unwrap();
    }
    assert_eq!(
        scan_count.load(Ordering::SeqCst) - before,
        1,
        "concurrent scan requests must share one scan"
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
    )
    .with_intel(Arc::new(lantern_daemon::intel::Intel::new(
        temp.0.join("state/intel-cache.json"),
        lantern_daemon::mcp::upstream::UpstreamPolicy::default(),
        lantern_daemon::mcp::upstream::default_client(),
    )));
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
    assert_eq!(app.intel(id.clone(), None).await.unwrap_err().code, "unavailable");
    let snapshot = app.refresh_intel(id.clone()).await.unwrap();
    assert!(snapshot.docs.iter().any(|d| d.path == "README.md"));
    assert_eq!(
        app.doc(id.clone(), "README.md".into(), Some(0), Some(8))
            .await
            .unwrap()
            .content,
        "Original"
    );
    assert_eq!(
        app.doc(id.clone(), "../secret".into(), None, None)
            .await
            .unwrap_err()
            .code,
        "doc_not_found"
    );
    assert_eq!(
        app.endpoints(id.clone(), Some("hello".into()), Some(1))
            .await
            .unwrap()[0]
            .path,
        "/hello"
    );
    fs::write(temp.0.join("project/README.md"), b"binary\0data").unwrap();
    assert_eq!(
        app.doc(id.clone(), "README.md".into(), None, None)
            .await
            .unwrap_err()
            .code,
        "forbidden"
    );
    fs::write(temp.0.join("project/README.md"), "Original documentation").unwrap();
    // A newly discovered file does not appear on GET until an explicit refresh.
    fs::write(temp.0.join("project/CHANGELOG.md"), "changes").unwrap();
    assert!(
        !app.docs(id.clone())
            .await
            .unwrap()
            .iter()
            .any(|d| d.path == "CHANGELOG.md")
    );
    assert!(
        app.refresh_intel(id.clone())
            .await
            .unwrap()
            .docs
            .iter()
            .any(|d| d.path == "CHANGELOG.md")
    );
    let intel_stop = CancellationToken::new();
    let worker = lantern_daemon::app::intel_worker::spawn(Arc::new(app.clone()), &bus, intel_stop.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;
    fs::write(temp.0.join("project/lantern.yaml"), "id: sample\ntype: proxy\nrun: {cmd: 'python3 -m http.server ${PORT}'}\ndocs_auto: {enabled: false}\ndocs: [CHANGELOG.md]\nendpoints: [{method: GET, path: /hello, description: Hello}]\n").unwrap();
    let mut changed = false;
    for _ in 0..60 {
        if app
            .docs(id.clone())
            .await
            .is_ok_and(|docs| docs.len() == 1 && docs[0].path == "CHANGELOG.md")
        {
            changed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    intel_stop.cancel();
    worker.await.unwrap();
    assert!(changed, "manifest change did not refresh the cached docs");
    // The assembled gateway must use its own origin guard, not the REST guard.
    {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };
        use lantern_daemon::{
            api::{ApiConfig, ApiState},
            app::gateway::GatewayAdapter,
            net::{
                gateway::{self, GatewayState},
                routes::RoutePlan,
                sync::{ApplyFuture, NetRouter, RouteApplier},
            },
        };
        use tower::ServiceExt;
        struct Noop;
        impl RouteApplier for Noop {
            fn apply<'a>(&'a self, _: &'a RoutePlan) -> ApplyFuture<'a> {
                Box::pin(async { Ok(()) })
            }
        }
        let router = Arc::new(NetRouter::new(
            Arc::new(Noop),
            "glow".into(),
            false,
            temp.0.join("php.sock"),
            shutdown.clone(),
        ));
        router.replace_instances(lantern_daemon::app::network::registry_instances(&registry));
        let backend = Arc::new(GatewayAdapter::new(
            router,
            registry.clone(),
            Arc::new(app.clone()),
        ));
        let gateway = gateway::router(GatewayState {
            backend,
            daemon_port: 39601,
            https_port: 443,
            root_cert: temp.0.join("ca.crt"),
        });
        let state = ApiState::new(
            Arc::new(app.clone()),
            EventBus::new(),
            tracker.clone(),
            ApiConfig::new(39601, "glow", "test"),
        );
        let combined = lantern_daemon::api::router(state).merge(gateway);
        let request = Request::builder()
            .uri("/__lantern/status")
            .header("host", "127.0.0.1:39601")
            .header("x-forwarded-host", "sample.glow")
            .header("origin", "https://sample.glow")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            combined.clone().oneshot(request).await.unwrap().status(),
            StatusCode::OK
        );
        let forbidden = Request::builder()
            .uri("/__lantern/status")
            .header("host", "attacker.test")
            .header("x-forwarded-host", "sample.glow")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            combined.oneshot(forbidden).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    let collision = app
        .register_project(RegisterProjectRequest {
            path: temp.0.join("project"),
            id: None,
            name: None,
            enabled: None,
            overrides: None,
            write_manifest: false,
        })
        .await
        .unwrap_err();
    assert_eq!(collision.code, "already_exists");
    let invalid_root = temp.0.join("invalid");
    fs::create_dir_all(&invalid_root).unwrap();
    let invalid = app
        .register_project(RegisterProjectRequest {
            path: invalid_root.clone(),
            id: None,
            name: None,
            enabled: None,
            overrides: Some(serde_json::json!({"enabled":[]})),
            write_manifest: true,
        })
        .await
        .unwrap_err();
    assert_eq!(invalid.code, "validation_failed");
    assert!(!invalid_root.join("lantern.yaml").exists());

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
    let script = fs::read_to_string(temp.0.join(format!("run/{}.sh", names.instance(&id)))).unwrap();
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
    let again = app.start(id.clone()).await.unwrap();
    assert_eq!(
        tracker
            .wait(&again.id, Duration::from_secs(8))
            .await
            .unwrap()
            .status,
        OperationStatus::Succeeded
    );
    let all = app
        .stop_all(lantern_core::api::StopAllRequest {
            include_services: false,
        })
        .await
        .unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(
        tracker
            .wait(&all[0].id, Duration::from_secs(8))
            .await
            .unwrap()
            .status,
        OperationStatus::Succeeded
    );
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
    let repo = temp.0.join("git-project");
    fs::create_dir_all(&repo).unwrap();
    run_git(&repo, &["init", "-b", "main"]);
    fs::write(
        repo.join("lantern.yaml"),
        "id: gitapp\ntype: proxy\nrun: {cmd: 'python3 -m http.server ${PORT}'}\n",
    )
    .unwrap();
    run_git(&repo, &["add", "lantern.yaml"]);
    run_git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "fixture",
        ],
    );
    let project = app
        .register_project(RegisterProjectRequest {
            path: repo,
            id: None,
            name: None,
            enabled: None,
            overrides: None,
            write_manifest: false,
        })
        .await
        .unwrap();
    let branch = app
        .run_branch(
            project.project.id,
            CreateInstanceRequest {
                target: RunTarget::NewBranch {
                    name: "feature".into(),
                    base: None,
                },
                fetch: false,
                start: false,
                pin: Some(true),
            },
        )
        .await
        .unwrap();
    let result = tracker.wait(&branch.id, Duration::from_secs(20)).await.unwrap();
    assert_eq!(result.status, OperationStatus::Succeeded, "{:?}", result.error);
    let id = InstanceId::parse(result.result.unwrap()["instance"].as_str().unwrap()).unwrap();
    assert_eq!(id.as_str(), "gitapp@feature");
    assert!(
        app.instance(id.clone())
            .await
            .unwrap()
            .checkout
            .path
            .starts_with(app.dirs.data.join("worktrees"))
    );
    assert!(registry.state_snapshot().port_leases.contains_key(&id));
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

fn run_git(dir: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}
