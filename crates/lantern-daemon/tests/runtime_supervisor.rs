use std::collections::BTreeMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lantern_core::Timestamp;
use lantern_core::api::OperationStatus;
use lantern_core::detect::Detection;
use lantern_core::ids::{InstanceId, ProjectId};
use lantern_core::manifest::{self, ResolveInput, TemplateDefinition, TemplateLookup, raw::RawManifest};
use lantern_core::model::{
    Checkout, CheckoutKind, Instance, InstanceStatus, IssueCode, Project, ProjectKind, ProjectSource,
    RuntimeState,
};
use lantern_core::settings::Settings;
use lantern_daemon::discovery::{
    PortBinding, Scanner, hide::HideSettings, matcher::CheckoutIndex, procinfo::ProcRoot,
};
use lantern_daemon::events::EventBus;
use lantern_daemon::operations::OperationTracker;
use lantern_daemon::runtime::seams::{
    InstancePreparer, PortProbe, PrepareFuture, RuntimeStore, ScannerProbe, StoreError,
};
use lantern_daemon::runtime::systemd::{
    Active, ExitCodeKind, FakeSystemd, Load, SystemctlBackend, UnitNames, UnitSnapshot, UserSystemd,
};
use lantern_daemon::runtime::{Runtime, RuntimeConfig, StartOpts, StopOpts};
use tokio_util::sync::CancellationToken;

static SEQ: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf, u16);
impl Temp {
    fn new() -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("lantern-supervisor-test-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p, 39100 + ((seq % 33) as u16) * 3)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct NoTemplates;
impl TemplateLookup for NoTemplates {
    fn get(&self, _: &str) -> Option<TemplateDefinition> {
        None
    }
}

#[derive(Default)]
struct Store {
    instances: Mutex<BTreeMap<InstanceId, Instance>>,
    projects: Mutex<BTreeMap<ProjectId, Project>>,
    leases: Mutex<BTreeMap<InstanceId, u16>>,
    activated: Mutex<BTreeMap<InstanceId, bool>>,
    changes: Mutex<Vec<(InstanceId, InstanceStatus)>>,
    conflict_once: std::sync::atomic::AtomicBool,
    conflict_port: std::sync::atomic::AtomicU16,
    fail_runtime_update: std::sync::atomic::AtomicBool,
}
impl Store {
    fn insert(&self, name: &str, root: &Path, cmd: &str, deps: &[&str]) -> InstanceId {
        let id = ProjectId::parse(name).unwrap();
        let iid = InstanceId::primary(&id);
        let input = format!(
            "id: {name}\ntype: proxy\nrun:\n  cmd: {cmd:?}\n{}",
            if deps.is_empty() {
                String::new()
            } else {
                format!("depends_on: [{}]\n", deps.join(", "))
            }
        );
        let parsed = manifest::parse_manifest(&input, "lantern.yaml");
        assert!(parsed.usable, "{:?}", parsed.diagnostics);
        let resolved = manifest::resolve(ResolveInput {
            dir_name: name,
            is_primary: true,
            primary: Some(&parsed),
            checkout: None,
            overrides: &RawManifest::default(),
            templates: &NoTemplates,
            detection: &Detection::default(),
            settings: &Settings::default(),
        });
        assert!(resolved.usable, "{:?}", resolved.diagnostics);
        let project = Project {
            id: id.clone(),
            aliases: vec![],
            name: name.into(),
            root: root.into(),
            repo: None,
            kind: ProjectKind::Project,
            enabled: true,
            tags: vec![],
            registered_at: Timestamp::now(),
            source: ProjectSource::Manual,
        };
        let instance = Instance {
            id: iid.clone(),
            project: id.clone(),
            checkout: Checkout {
                path: root.into(),
                git_root: None,
                kind: CheckoutKind::Primary,
                branch: None,
                head: None,
                dirty: None,
                locked: None,
                missing: false,
            },
            domain: format!("{name}.test"),
            manifest: Arc::new(resolved),
            runtime: RuntimeState::default(),
            pinned: false,
            adopt: false,
            setup_stale: false,
        };
        self.projects.lock().unwrap().insert(id, project);
        self.instances.lock().unwrap().insert(iid.clone(), instance);
        iid
    }
}
impl RuntimeStore for Store {
    fn instance(&self, id: &InstanceId) -> Result<Option<Instance>, StoreError> {
        Ok(self.instances.lock().unwrap().get(id).cloned())
    }
    fn instances(&self) -> Result<Vec<Instance>, StoreError> {
        Ok(self.instances.lock().unwrap().values().cloned().collect())
    }
    fn project(&self, id: &ProjectId) -> Result<Option<Project>, StoreError> {
        Ok(self.projects.lock().unwrap().get(id).cloned())
    }
    fn port_leases(&self) -> Result<BTreeMap<InstanceId, u16>, StoreError> {
        Ok(self.leases.lock().unwrap().clone())
    }
    fn compare_port_leases(
        &self,
        expected: &BTreeMap<InstanceId, u16>,
        replacement: BTreeMap<InstanceId, u16>,
    ) -> Result<bool, StoreError> {
        let mut leases = self.leases.lock().unwrap();
        if self.conflict_once.swap(false, Ordering::SeqCst) {
            leases.insert(
                InstanceId::parse("other").unwrap(),
                self.conflict_port.load(Ordering::SeqCst),
            );
            return Ok(false);
        }
        if &*leases != expected {
            return Ok(false);
        }
        *leases = replacement;
        Ok(true)
    }
    fn update_runtime(
        &self,
        id: &InstanceId,
        f: Box<dyn FnOnce(&mut RuntimeState) + Send>,
    ) -> Result<(), StoreError> {
        if self.fail_runtime_update.load(Ordering::SeqCst) {
            return Err(StoreError("injected publication failure".into()));
        }
        let mut map = self.instances.lock().unwrap();
        let i = map.get_mut(id).ok_or_else(|| StoreError("missing".into()))?;
        let before = i.runtime.status;
        f(&mut i.runtime);
        if i.runtime.status != before {
            self.changes.lock().unwrap().push((id.clone(), i.runtime.status));
        }
        Ok(())
    }
    fn activated(&self, id: &InstanceId) -> Result<bool, StoreError> {
        Ok(self.activated.lock().unwrap().get(id).copied().unwrap_or(false))
    }
    fn set_activated(&self, id: &InstanceId, v: bool) -> Result<(), StoreError> {
        self.activated.lock().unwrap().insert(id.clone(), v);
        Ok(())
    }
    fn record_started(&self, _: &InstanceId) -> Result<(), StoreError> {
        Ok(())
    }
    fn resolve_dependency(&self, name: &str) -> Result<Option<InstanceId>, StoreError> {
        Ok(ProjectId::parse(name)
            .ok()
            .map(|p| InstanceId::primary(&p))
            .filter(|i| self.instances.lock().unwrap().contains_key(i)))
    }
}
#[derive(Clone, Copy)]
enum ProbeMode {
    Ready,
    Waiting,
    Wrong,
    Error,
}
struct FakeProbe(Mutex<ProbeMode>);
impl PortProbe for FakeProbe {
    fn port_binding(&self, port: u16, unit: Option<&str>) -> io::Result<PortBinding> {
        if unit.is_some() && matches!(*self.0.lock().unwrap(), ProbeMode::Error) {
            return Err(io::Error::other("fake discovery failure"));
        }
        Ok(match (unit, *self.0.lock().unwrap()) {
            (Some(_), ProbeMode::Ready) => PortBinding::BoundByUnit {
                pid: 1234,
                addrs: vec![SocketAddr::from((Ipv4Addr::LOCALHOST, port))],
            },
            _ => PortBinding::Free,
        })
    }
    fn unit_listeners(&self, _: &str) -> io::Result<Vec<SocketAddr>> {
        Ok(if matches!(*self.0.lock().unwrap(), ProbeMode::Wrong) {
            vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 39198))]
        } else {
            vec![]
        })
    }
}
struct HungPrepare;
impl InstancePreparer for HungPrepare {
    fn ensure_ready<'a>(
        &'a self,
        _: &'a Instance,
        _: Option<u16>,
        cancel: CancellationToken,
        _: &'a lantern_daemon::operations::OpHandle,
    ) -> PrepareFuture<'a> {
        Box::pin(async move {
            cancel.cancelled().await;
            Err("cancelled".into())
        })
    }
}
fn config(temp: &Temp) -> RuntimeConfig {
    std::fs::create_dir_all(temp.0.join("cache")).unwrap();
    if !temp.0.join("cache/env.sh").exists() {
        std::fs::write(temp.0.join("cache/env.sh"), "export PATH='/usr/bin:/bin'\n").unwrap();
    }
    let mut settings = Settings::default();
    settings.ports.range_start = temp.1;
    settings.ports.range_end = temp.1 + 2;
    settings.runtime.startup_timeout_secs = 5;
    settings.runtime.stop_timeout_secs = 1;
    settings.runtime.backoff_base_secs = 0;
    settings.runtime.backoff_cap_secs = 0;
    settings.runtime.max_retries = 2;
    settings.health.interval_secs = 60;
    RuntimeConfig {
        settings,
        run_dir: temp.0.join("run"),
        shell_env_file: temp.0.join("cache/env.sh"),
        home: temp.0.clone(),
        names: UnitNames {
            instance_prefix: "lantern-test-supervisor-inst-".into(),
            operation_prefix: "lantern-test-supervisor-op-".into(),
            slice: "app-lantern-test.slice".into(),
        },
        poll_interval: Duration::from_millis(20),
        readiness_interval: Duration::from_millis(20),
        mismatch_after: Duration::from_millis(60),
    }
}
fn runtime(
    config: RuntimeConfig,
    store: Arc<Store>,
    probe: Arc<dyn PortProbe>,
    backend: Arc<dyn UserSystemd>,
) -> Runtime {
    Runtime::new(
        config,
        store,
        probe,
        backend,
        OperationTracker::new(EventBus::new()),
    )
}
async fn wait_state(
    rt: &Runtime,
    id: &InstanceId,
    predicate: impl Fn(&RuntimeState) -> bool,
) -> RuntimeState {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let state = rt.state(id).unwrap();
        if predicate(&state) {
            return state;
        }
        assert!(Instant::now() < deadline, "timed out in state {state:?}");
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}
async fn completed(op: &lantern_daemon::runtime::RuntimeOperation) -> OperationStatus {
    op.wait(Duration::from_secs(8)).await.unwrap().status
}
fn running_snapshot(unit: &str) -> UnitSnapshot {
    UnitSnapshot {
        unit: unit.into(),
        load: Load::Loaded,
        active: Active::Active,
        sub: "running".into(),
        result: "success".into(),
        main_pid: Some(1234),
        exit_code: None,
        exit_kind: ExitCodeKind::Unknown,
        started_at: Some(Timestamp::now()),
        invocation_id: None,
        cgroup: None,
    }
}
fn exit_snapshot(unit: &str, code: i32) -> UnitSnapshot {
    UnitSnapshot {
        active: Active::Failed,
        sub: "failed".into(),
        result: "exit-code".into(),
        main_pid: None,
        exit_code: Some(code),
        exit_kind: ExitCodeKind::Exited,
        ..running_snapshot(unit)
    }
}

#[tokio::test]
async fn runtime_start_stop_and_dependency_order() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let db = store.insert("db", &temp.0, "python3 -m http.server ${PORT}", &[]);
    let app = store.insert("app", &temp.0, "python3 -m http.server ${PORT}", &["db"]);
    let fake = Arc::new(FakeSystemd::default());
    fake.set_auto_start(true);
    let rt = runtime(
        config(&temp),
        store.clone(),
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready))),
        fake.clone(),
    );
    let op = rt.start(&app, StartOpts).await.unwrap();
    assert_eq!(completed(&op).await, OperationStatus::Succeeded);
    assert_eq!(rt.state(&db).unwrap().status, InstanceStatus::Running);
    assert_eq!(rt.state(&app).unwrap().status, InstanceStatus::Running);
    let calls = fake.calls();
    let a = calls
        .iter()
        .position(|s| s == "start lantern-test-supervisor-inst-db.service")
        .unwrap();
    let b = calls
        .iter()
        .position(|s| s == "start lantern-test-supervisor-inst-app.service")
        .unwrap();
    assert!(a < b, "{calls:?}");
    let app_port = rt.state(&app).unwrap().port;
    let stop = rt.stop(&app, StopOpts::default()).await.unwrap();
    assert_eq!(completed(&stop).await, OperationStatus::Succeeded);
    assert_eq!(rt.state(&app).unwrap().status, InstanceStatus::Stopped);
    assert_eq!(rt.state(&app).unwrap().port, app_port);
    assert_eq!(rt.state(&db).unwrap().status, InstanceStatus::Running);
    let all = rt.stop_all().await.unwrap();
    assert_eq!(completed(&all).await, OperationStatus::Succeeded);
}

#[tokio::test]
async fn runtime_reserves_configured_dns_port() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("dns-reserved", &temp.0, "server ${PORT}", &[]);
    let mut cfg = config(&temp);
    cfg.settings.dns.listen = format!("127.0.0.1:{}", temp.1);
    let rt = Runtime::with_scoped_journal(
        cfg,
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Waiting))),
        Arc::new(FakeSystemd::default()),
        OperationTracker::new(EventBus::new()),
        lantern_daemon::runtime::RuntimeAdapters {
            preparer: Arc::new(lantern_daemon::runtime::seams::NoopPreparer),
            router: Arc::new(lantern_daemon::runtime::seams::UnavailableRouter),
            services: Arc::new(lantern_daemon::runtime::seams::UnavailableServices),
        },
        true,
    );
    assert_eq!(rt.reserve_port(&id).await.unwrap(), temp.1 + 1);
    rt.shutdown_background();
}

#[tokio::test]
async fn runtime_stop_preempts_hung_start_and_identical_requests_join() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("app", &temp.0, "python3 -m http.server ${PORT}", &[]);
    let fake = Arc::new(FakeSystemd::default());
    let rt = Runtime::with_preparer(
        config(&temp),
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready))),
        fake.clone(),
        OperationTracker::new(EventBus::new()),
        Arc::new(HungPrepare),
    );
    let start = rt.start(&id, StartOpts).await.unwrap();
    wait_state(&rt, &id, |s| s.status == InstanceStatus::Preparing).await;
    let joined = rt.start(&id, StartOpts).await.unwrap();
    assert_eq!(start.id, joined.id);
    assert!(rt.restart(&id).await.is_err());
    let stop = rt.stop(&id, StopOpts::default()).await.unwrap();
    assert_eq!(completed(&stop).await, OperationStatus::Succeeded);
    assert_eq!(completed(&start).await, OperationStatus::Cancelled);
    assert_eq!(rt.state(&id).unwrap().status, InstanceStatus::Stopped);
    assert!(!fake.calls().iter().any(|s| s.starts_with("start ")));
}

#[tokio::test]
async fn runtime_port_mismatch_stops_the_unit() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("app", &temp.0, "python3 -m http.server ${PORT}", &[]);
    let fake = Arc::new(FakeSystemd::default());
    fake.set_auto_start(true);
    let rt = runtime(
        config(&temp),
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Wrong))),
        fake.clone(),
    );
    let op = rt.start(&id, StartOpts).await.unwrap();
    assert_eq!(completed(&op).await, OperationStatus::Failed);
    let s = rt.state(&id).unwrap();
    assert_eq!(s.status, InstanceStatus::NeedsConfig);
    assert_eq!(s.issue.unwrap().code, IssueCode::PortMismatch);
    assert!(fake.calls().iter().any(|s| s.starts_with("stop ")));
}

#[derive(Default)]
struct CapturingSystemd {
    fake: FakeSystemd,
    started: Mutex<Vec<lantern_daemon::runtime::systemd::UnitStart>>,
}
impl UserSystemd for CapturingSystemd {
    fn start<'a>(
        &'a self,
        spec: &'a lantern_daemon::runtime::systemd::UnitStart,
    ) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, ()> {
        self.started.lock().unwrap().push(spec.clone());
        self.fake.start(spec)
    }
    fn stop<'a>(
        &'a self,
        unit: &'a str,
        timeout: Duration,
    ) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, ()> {
        self.fake.stop(unit, timeout)
    }
    fn reset_failed<'a>(&'a self, unit: &'a str) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, ()> {
        self.fake.reset_failed(unit)
    }
    fn poll<'a>(
        &'a self,
        units: &'a [String],
    ) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, Vec<UnitSnapshot>> {
        self.fake.poll(units)
    }
    fn list_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, Vec<String>> {
        self.fake.list_prefix(prefix)
    }
}

#[tokio::test]
async fn runtime_run_hooks_and_manifest_startup_timeout() {
    use lantern_core::template::TemplateString;
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("hooks", &temp.0, "server ${PORT}", &[]);
    {
        let mut all = store.instances.lock().unwrap();
        let run = &mut Arc::make_mut(&mut all.get_mut(&id).unwrap().manifest).run;
        run.pre_start = Some(TemplateString::parse("echo pre-${BRANCH}").0);
        run.post_stop = Some(TemplateString::parse("echo post-${BRANCH}").0);
        run.startup_timeout = Some(1);
    }
    let backend = Arc::new(CapturingSystemd::default());
    backend.fake.set_auto_start(true);
    let rt = Runtime::with_scoped_journal(
        config(&temp),
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Waiting))),
        backend.clone(),
        OperationTracker::new(EventBus::new()),
        lantern_daemon::runtime::RuntimeAdapters {
            preparer: Arc::new(lantern_daemon::runtime::seams::NoopPreparer),
            router: Arc::new(lantern_daemon::runtime::seams::UnavailableRouter),
            services: Arc::new(lantern_daemon::runtime::seams::UnavailableServices),
        },
        true,
    );
    let op = rt.start(&id, StartOpts).await.unwrap();
    let snapshot = op.wait(Duration::from_secs(3)).await.unwrap();
    assert_eq!(
        snapshot.status,
        OperationStatus::Failed,
        "manifest timeout was ignored: {snapshot:?}"
    );
    assert_eq!(
        rt.state(&id).unwrap().issue.unwrap().code,
        IssueCode::ReadinessTimeout
    );
    let unit = backend.started.lock().unwrap().first().unwrap().clone();
    let main = std::fs::read_to_string(&unit.script).unwrap();
    assert!(main.contains("echo pre-${LANTERN_BRANCH}"), "{main}");
    let post = std::fs::read_to_string(
        unit.post_stop
            .as_ref()
            .expect("post-stop script must be attached"),
    )
    .unwrap();
    assert!(post.contains("echo post-${LANTERN_BRANCH}"), "{post}");
    let oneshot = rt
        .prepare_oneshot(
            &id,
            lantern_daemon::runtime::OneShotLaunch {
                verb: "setup".into(),
                cwd: temp.0.clone(),
                shell_command: "echo setup".into(),
                env: BTreeMap::new(),
                timeout: Duration::from_secs(5),
            },
        )
        .await
        .unwrap();
    assert!(oneshot.unit.post_stop.is_none());
    assert!(
        !std::fs::read_to_string(oneshot.unit.script)
            .unwrap()
            .contains("echo pre-")
    );
    rt.shutdown_background();
}

#[tokio::test]
async fn runtime_crash_retries_and_stops_at_crash_loop() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("app", &temp.0, "python3 -m http.server ${PORT}", &[]);
    let fake = Arc::new(FakeSystemd::default());
    fake.set_auto_start(true);
    let rt = runtime(
        config(&temp),
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready))),
        fake.clone(),
    );
    assert_eq!(
        completed(&rt.start(&id, StartOpts).await.unwrap()).await,
        OperationStatus::Succeeded
    );
    let unit = "lantern-test-supervisor-inst-app.service";
    for restart in 0..=2 {
        fake.set_snapshot(exit_snapshot(unit, 1));
        if restart < 2 {
            wait_state(&rt, &id, |s| {
                s.status == InstanceStatus::Running && s.restarts == restart + 1
            })
            .await;
        }
    }
    let s = wait_state(&rt, &id, |s| {
        s.status == InstanceStatus::Crashed
            && s.issue.as_ref().is_some_and(|i| i.code == IssueCode::CrashLoop)
    })
    .await;
    assert_eq!(s.restarts, 2);
    assert_eq!(fake.calls().iter().filter(|c| c.starts_with("start ")).count(), 3);
}

#[tokio::test]
async fn runtime_deterministic_exits_are_not_retried() {
    for code in [78, 126, 127] {
        let temp = Temp::new();
        let store = Arc::new(Store::default());
        let id = store.insert("app", &temp.0, "python3 -m http.server ${PORT}", &[]);
        let fake = Arc::new(FakeSystemd::default());
        fake.set_auto_start(true);
        let rt = runtime(
            config(&temp),
            store,
            Arc::new(FakeProbe(Mutex::new(ProbeMode::Waiting))),
            fake.clone(),
        );
        let op = rt.start(&id, StartOpts).await.unwrap();
        wait_state(&rt, &id, |s| s.unit.is_some()).await;
        fake.set_snapshot(exit_snapshot("lantern-test-supervisor-inst-app.service", code));
        assert_eq!(completed(&op).await, OperationStatus::Failed);
        let s = wait_state(&rt, &id, |s| s.status == InstanceStatus::Crashed).await;
        assert_eq!(
            s.issue.unwrap().code,
            if code == 78 {
                IssueCode::ToolchainMissing
            } else {
                IssueCode::CommandNotFound
            }
        );
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(fake.calls().iter().filter(|s| s.starts_with("start ")).count(), 1);
    }
}

#[derive(Default)]
struct RecordingReal {
    units: Mutex<Vec<String>>,
    backend: SystemctlBackend,
}
impl UserSystemd for RecordingReal {
    fn start<'a>(
        &'a self,
        spec: &'a lantern_daemon::runtime::systemd::UnitStart,
    ) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, ()> {
        self.units.lock().unwrap().push(spec.unit.clone());
        self.backend.start(spec)
    }
    fn stop<'a>(
        &'a self,
        unit: &'a str,
        timeout: Duration,
    ) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, ()> {
        self.backend.stop(unit, timeout)
    }
    fn reset_failed<'a>(&'a self, unit: &'a str) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, ()> {
        self.backend.reset_failed(unit)
    }
    fn poll<'a>(
        &'a self,
        units: &'a [String],
    ) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, Vec<UnitSnapshot>> {
        self.backend.poll(units)
    }
    fn list_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, Vec<String>> {
        self.backend.list_prefix(prefix)
    }
}
impl Drop for RecordingReal {
    fn drop(&mut self) {
        for unit in self.units.lock().unwrap().iter() {
            if unit.starts_with("lantern-test-supervisor-") {
                for action in ["stop", "reset-failed"] {
                    let _ = std::process::Command::new("systemctl")
                        .args(["--user", action, "--", unit])
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
            }
        }
    }
}

struct RealGuard {
    unit: String,
}

#[tokio::test]
#[ignore = "real system: creates and reads only its own transient test unit"]
async fn runtime_real_reconcile_backfills_adopted_unit_logs() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let name = format!("backfill{}", std::process::id());
    let id = store.insert(
        &name,
        &temp.0,
        "printf 'history-before-restart\\n'; sleep 2; printf 'live-after-restart\\n'; sleep 60",
        &[],
    );
    let cfg = config(&temp);
    let unit = cfg.names.instance(&id);
    let _guard = RealGuard { unit: unit.clone() };
    let backend = Arc::new(RecordingReal::default());
    let make_runtime = || {
        Runtime::with_scoped_journal(
            cfg.clone(),
            store.clone(),
            Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready))),
            backend.clone(),
            OperationTracker::new(EventBus::new()),
            lantern_daemon::runtime::RuntimeAdapters {
                preparer: Arc::new(lantern_daemon::runtime::seams::NoopPreparer),
                router: Arc::new(lantern_daemon::runtime::seams::UnavailableRouter),
                services: Arc::new(lantern_daemon::runtime::seams::UnavailableServices),
            },
            true,
        )
    };
    let first = make_runtime();
    assert_eq!(
        completed(&first.start(&id, StartOpts).await.unwrap()).await,
        OperationStatus::Succeeded
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if first
                .log_history(&id, lantern_daemon::runtime::HistoryQuery { limit: 100 })
                .await
                .unwrap()
                .iter()
                .any(|l| l.line == "history-before-restart")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("test unit did not write journal line");
    first.shutdown_background();
    drop(first);
    let restarted = make_runtime();
    let report = restarted.reconcile().await;
    assert_eq!(report.adopted, vec![id.clone()]);
    let (backlog, mut live) = restarted.subscribe_logs(&id, 100);
    assert_eq!(
        backlog
            .iter()
            .filter(|l| l.line == "history-before-restart")
            .count(),
        1,
        "backlog: {backlog:?}"
    );
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if live.recv().await.unwrap().line == "live-after-restart" {
                break;
            }
        }
    })
    .await
    .expect("follower missed a line after reconciliation");
    restarted.shutdown_background();
}

#[tokio::test]
#[ignore = "real system: executes hooks in its own transient test unit"]
async fn runtime_real_pre_start_and_post_stop_hooks_run() {
    use lantern_core::template::TemplateString;
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert(&format!("hook{}", std::process::id()), &temp.0, "sleep 60", &[]);
    {
        let mut all = store.instances.lock().unwrap();
        let run = &mut Arc::make_mut(&mut all.get_mut(&id).unwrap().manifest).run;
        run.pre_start = Some(TemplateString::parse("printf '%s' \"${PORT}\" > pre.txt").0);
        run.post_stop = Some(TemplateString::parse("printf '%s' \"${PORT}\" > post.txt").0);
    }
    let cfg = config(&temp);
    let _guard = RealGuard {
        unit: cfg.names.instance(&id),
    };
    let backend = Arc::new(RecordingReal::default());
    let rt = Runtime::with_scoped_journal(
        cfg,
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready))),
        backend,
        OperationTracker::new(EventBus::new()),
        lantern_daemon::runtime::RuntimeAdapters {
            preparer: Arc::new(lantern_daemon::runtime::seams::NoopPreparer),
            router: Arc::new(lantern_daemon::runtime::seams::UnavailableRouter),
            services: Arc::new(lantern_daemon::runtime::seams::UnavailableServices),
        },
        true,
    );
    assert_eq!(
        completed(&rt.start(&id, StartOpts).await.unwrap()).await,
        OperationStatus::Succeeded
    );
    let port = rt.state(&id).unwrap().port.unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !temp.0.join("pre.txt").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(temp.0.join("pre.txt")).unwrap(), port);
    assert_eq!(
        completed(&rt.stop(&id, StopOpts::default()).await.unwrap()).await,
        OperationStatus::Succeeded
    );
    assert_eq!(std::fs::read_to_string(temp.0.join("post.txt")).unwrap(), port);
    rt.shutdown_background();
}
impl Drop for RealGuard {
    fn drop(&mut self) {
        for action in ["stop", "reset-failed"] {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", action, "--", &self.unit])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

#[tokio::test]
#[ignore = "real system: creates a dedicated transient test unit in app-lantern-test.slice"]
async fn runtime_real_systemd_discovery_logs_and_stop() {
    use tokio::io::AsyncWriteExt;
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert(
        "e2e",
        &temp.0,
        "python3 -m http.server ${PORT} --bind 127.0.0.1",
        &[],
    );
    {
        let mut all = store.instances.lock().unwrap();
        let manifest = Arc::make_mut(&mut all.get_mut(&id).unwrap().manifest);
        manifest.health_endpoint = Some("/".into());
        manifest.deploy.status =
            Some(lantern_core::template::TemplateString::parse("printf 'supervisor-status\\n'").0);
        manifest.deploy.logs =
            Some(lantern_core::template::TemplateString::parse("printf 'supervisor-custom-log\\n'").0);
    }
    let mut cfg = config(&temp);
    cfg.settings.health.interval_secs = 1;
    cfg.settings.health.grace_secs = 0;
    let unit = cfg.names.instance(&id);
    let _guard = RealGuard { unit: unit.clone() };
    let scanner = Scanner::new(
        ProcRoot::default(),
        CheckoutIndex::new([], temp.0.clone()),
        HideSettings::default(),
    );
    let backend = Arc::new(RecordingReal::default());
    let rt = Runtime::with_scoped_journal(
        cfg,
        store,
        Arc::new(ScannerProbe::new(scanner)),
        backend.clone(),
        OperationTracker::new(EventBus::new()),
        lantern_daemon::runtime::RuntimeAdapters {
            preparer: Arc::new(lantern_daemon::runtime::seams::NoopPreparer),
            router: Arc::new(lantern_daemon::runtime::seams::UnavailableRouter),
            services: Arc::new(lantern_daemon::runtime::seams::UnavailableServices),
        },
        true,
    );
    let (_, mut lines) = rt.subscribe_logs(&id, 0);
    let op = rt.start(&id, StartOpts).await.unwrap();
    let result = op.wait(Duration::from_secs(25)).await.unwrap();
    assert_eq!(result.status, OperationStatus::Succeeded, "{result:?}");
    let state = rt.state(&id).unwrap();
    assert_eq!(state.status, InstanceStatus::Running);
    let port = state.port.unwrap();
    assert!((39100..=39199).contains(&port));
    wait_state(&rt, &id, |s| {
        s.health.state == lantern_core::model::HealthState::Healthy
    })
    .await;
    let mut socket = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    socket.write_all(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();
    let line = tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let l = lines.recv().await.unwrap();
            if l.line.contains("GET /") {
                break l;
            }
        }
    })
    .await
    .expect("journal follower did not deliver HTTP access log");
    assert_eq!(line.instance, id);
    let setup = rt
        .prepare_oneshot(
            &id,
            lantern_daemon::runtime::OneShotLaunch {
                verb: "setup".into(),
                cwd: temp.0.clone(),
                shell_command: "sleep 3; printf 'supervisor-setup-line\\n'; sleep 1".into(),
                env: BTreeMap::new(),
                timeout: Duration::from_secs(10),
            },
        )
        .await
        .unwrap();
    let setup_unit = setup.unit.unit.clone();
    let _setup_guard = RealGuard { unit: setup_unit };
    let setup_op = rt.run_oneshot(setup);
    assert_eq!(
        setup_op.wait(Duration::from_secs(20)).await.unwrap().status,
        OperationStatus::Succeeded
    );
    let setup_line = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let line = lines.recv().await.unwrap();
            if line.line.contains("supervisor-setup-line") {
                break line;
            }
        }
    })
    .await
    .expect("setup line missing from journal follower");
    assert_eq!(setup_line.stream, lantern_core::model::Stream::Setup);
    let status = rt.deploy_status(&id).await.unwrap();
    assert!(status.alive);
    assert!(status.output.contains("supervisor-status"));
    let deploy_log = rt.deploy_logs(&id).await.unwrap();
    assert!(deploy_log.contains("supervisor-custom-log"));
    let history = rt
        .log_history(&id, lantern_daemon::runtime::HistoryQuery { limit: 100 })
        .await
        .unwrap();
    assert!(
        history.iter().any(|entry| entry.line.contains("GET /")),
        "journal history omitted request: {history:?}"
    );
    let stop = rt.stop(&id, StopOpts::default()).await.unwrap();
    assert_eq!(completed(&stop).await, OperationStatus::Succeeded);
    let units = backend.units.lock().unwrap().clone();
    assert!(units.contains(&unit));
    let rows = SystemctlBackend.poll(&units).await.unwrap();
    assert!(
        rows.is_empty() || rows.iter().all(|r| r.load == Load::NotFound),
        "test units remain loaded: {rows:?}"
    );
}

#[derive(Default)]
struct TestRouter {
    calls: Mutex<Vec<String>>,
    php_ok: bool,
}
impl lantern_daemon::runtime::seams::RuntimeRouter for TestRouter {
    fn publish<'a>(
        &'a self,
        instance: &'a Instance,
        target: lantern_daemon::runtime::seams::RouteTarget,
    ) -> lantern_daemon::runtime::seams::RouteFuture<'a> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(format!("publish {} {target:?}", instance.id));
            Ok(())
        })
    }
    fn unpublish<'a>(&'a self, id: &'a InstanceId) -> lantern_daemon::runtime::seams::RouteFuture<'a> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(format!("unpublish {id}"));
            Ok(())
        })
    }
    fn php_ready<'a>(&'a self) -> lantern_daemon::runtime::seams::RouteFuture<'a> {
        Box::pin(async move {
            if self.php_ok {
                Ok(())
            } else {
                Err("php-fpm unavailable".into())
            }
        })
    }
}

#[tokio::test]
async fn runtime_site_only_activation_and_reconcile_use_router() {
    use lantern_core::model::ServeMode;
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("site", &temp.0, "unused", &[]);
    {
        let mut all = store.instances.lock().unwrap();
        let i = all.get_mut(&id).unwrap();
        let m = Arc::make_mut(&mut i.manifest);
        m.serve = ServeMode::Static;
        m.run.cmd = None;
        m.root = Some(".".into());
    }
    let fake = Arc::new(FakeSystemd::default());
    let router = Arc::new(TestRouter::default());
    let make = || {
        Runtime::with_adapters(
            config(&temp),
            store.clone(),
            Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready))),
            fake.clone(),
            OperationTracker::new(EventBus::new()),
            Arc::new(lantern_daemon::runtime::seams::NoopPreparer),
            router.clone(),
        )
    };
    let rt = make();
    assert_eq!(
        completed(&rt.start(&id, StartOpts).await.unwrap()).await,
        OperationStatus::Succeeded
    );
    assert!(store.activated(&id).unwrap());
    assert!(fake.calls().iter().all(|s| !s.starts_with("start ")));
    let rt2 = make();
    let report = rt2.reconcile().await;
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(rt2.state(&id).unwrap().status, InstanceStatus::Running);
    assert_eq!(
        router
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.starts_with("publish site"))
            .count(),
        2
    );
    assert_eq!(
        completed(&rt2.stop(&id, StopOpts::default()).await.unwrap()).await,
        OperationStatus::Succeeded
    );
    assert!(!store.activated(&id).unwrap());
    assert!(router.calls.lock().unwrap().iter().any(|s| s == "unpublish site"));
}

#[tokio::test]
async fn runtime_php_site_requires_php_fpm_and_router() {
    use lantern_core::model::ServeMode;
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("php", &temp.0, "unused", &[]);
    {
        let mut all = store.instances.lock().unwrap();
        let m = Arc::make_mut(&mut all.get_mut(&id).unwrap().manifest);
        m.serve = ServeMode::Php;
        m.run.cmd = None;
        m.root = Some(".".into());
    }
    let fake = Arc::new(FakeSystemd::default());
    let router = Arc::new(TestRouter::default());
    let rt = Runtime::with_adapters(
        config(&temp),
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready))),
        fake,
        OperationTracker::new(EventBus::new()),
        Arc::new(lantern_daemon::runtime::seams::NoopPreparer),
        router,
    );
    let op = rt.start(&id, StartOpts).await.unwrap();
    assert_eq!(completed(&op).await, OperationStatus::Failed);
    assert_eq!(rt.state(&id).unwrap().status, InstanceStatus::NeedsConfig);
}

struct DeploySystemd {
    fake: FakeSystemd,
    running: Arc<std::sync::atomic::AtomicBool>,
    verbs: Mutex<Vec<String>>,
}
impl UserSystemd for DeploySystemd {
    fn start<'a>(
        &'a self,
        spec: &'a lantern_daemon::runtime::systemd::UnitStart,
    ) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, ()> {
        Box::pin(async move {
            self.fake.start(spec).await?;
            let text = std::fs::read_to_string(&spec.script)?;
            let verb = if text.contains("BEGINMARKER") {
                self.running.store(true, Ordering::SeqCst);
                "start"
            } else if text.contains("RESTARTMARKER") {
                self.running.store(true, Ordering::SeqCst);
                "restart"
            } else if text.contains("STOPMARKER") {
                self.running.store(false, Ordering::SeqCst);
                "stop"
            } else {
                "other"
            };
            self.verbs.lock().unwrap().push(verb.into());
            let status_failed = text.contains("STATUSFAIL");
            self.fake.set_snapshot(UnitSnapshot {
                sub: "exited".into(),
                exit_kind: ExitCodeKind::Exited,
                exit_code: Some(i32::from(status_failed)),
                ..running_snapshot(&spec.unit)
            });
            Ok(())
        })
    }
    fn stop<'a>(
        &'a self,
        unit: &'a str,
        timeout: Duration,
    ) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, ()> {
        self.fake.stop(unit, timeout)
    }
    fn reset_failed<'a>(&'a self, unit: &'a str) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, ()> {
        self.fake.reset_failed(unit)
    }
    fn poll<'a>(
        &'a self,
        units: &'a [String],
    ) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, Vec<UnitSnapshot>> {
        self.fake.poll(units)
    }
    fn list_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, Vec<String>> {
        self.fake.list_prefix(prefix)
    }
}
struct DeployProbe(Arc<std::sync::atomic::AtomicBool>);
impl PortProbe for DeployProbe {
    fn port_binding(&self, port: u16, _: Option<&str>) -> io::Result<PortBinding> {
        Ok(if self.0.load(Ordering::SeqCst) {
            PortBinding::BoundByUnit {
                pid: 100,
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
#[tokio::test]
async fn runtime_deploy_start_restart_and_stop_use_oneshots() {
    use lantern_core::template::TemplateString;
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("deploy", &temp.0, "unused", &[]);
    {
        let mut all = store.instances.lock().unwrap();
        let m = Arc::make_mut(&mut all.get_mut(&id).unwrap().manifest);
        m.deploy.start = Some(TemplateString::parse("echo BEGINMARKER ${PORT}").0);
        m.deploy.restart = Some(TemplateString::parse("echo RESTARTMARKER ${PORT}").0);
        m.deploy.stop = Some(TemplateString::parse("echo STOPMARKER").0);
    }
    let running = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let systemd = Arc::new(DeploySystemd {
        fake: FakeSystemd::default(),
        running: running.clone(),
        verbs: Mutex::new(vec![]),
    });
    let rt = runtime(
        config(&temp),
        store,
        Arc::new(DeployProbe(running.clone())),
        systemd.clone(),
    );
    let start = rt.start(&id, StartOpts).await.unwrap();
    let snap = start.wait(Duration::from_secs(8)).await.unwrap();
    assert_eq!(snap.status, OperationStatus::Succeeded, "{snap:?}");
    let first_port = rt.state(&id).unwrap().port;
    let restart = rt.restart(&id).await.unwrap();
    let snap = restart.wait(Duration::from_secs(8)).await.unwrap();
    assert_eq!(snap.status, OperationStatus::Succeeded, "{snap:?}");
    assert_eq!(rt.state(&id).unwrap().port, first_port);
    let stop = rt.stop(&id, StopOpts::default()).await.unwrap();
    let snap = stop.wait(Duration::from_secs(8)).await.unwrap();
    assert_eq!(snap.status, OperationStatus::Succeeded, "{snap:?}");
    assert_eq!(*systemd.verbs.lock().unwrap(), vec!["start", "restart", "stop"]);
    assert!(!running.load(Ordering::SeqCst));
}

#[tokio::test]
async fn runtime_deploy_status_failure_uses_crash_retry_path() {
    use lantern_core::template::TemplateString;
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("statusfail", &temp.0, "unused", &[]);
    {
        let mut all = store.instances.lock().unwrap();
        let m = Arc::make_mut(&mut all.get_mut(&id).unwrap().manifest);
        m.deploy.start = Some(TemplateString::parse("echo BEGINMARKER ${PORT}").0);
        m.deploy.status = Some(TemplateString::parse("false # STATUSFAIL").0);
    }
    let running = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let backend = Arc::new(DeploySystemd {
        fake: FakeSystemd::default(),
        running: running.clone(),
        verbs: Mutex::new(vec![]),
    });
    let mut cfg = config(&temp);
    cfg.settings.health.grace_secs = 0;
    cfg.settings.runtime.backoff_base_secs = 1;
    cfg.settings.runtime.backoff_cap_secs = 1;
    let rt = Runtime::with_scoped_journal(
        cfg,
        store,
        Arc::new(DeployProbe(running.clone())),
        backend.clone(),
        OperationTracker::new(EventBus::new()),
        lantern_daemon::runtime::RuntimeAdapters {
            preparer: Arc::new(lantern_daemon::runtime::seams::NoopPreparer),
            router: Arc::new(lantern_daemon::runtime::seams::UnavailableRouter),
            services: Arc::new(lantern_daemon::runtime::seams::UnavailableServices),
        },
        true,
    );
    assert_eq!(
        completed(&rt.start(&id, StartOpts).await.unwrap()).await,
        OperationStatus::Succeeded
    );
    for _ in 0..3 {
        rt.check_health_now(&id).await.unwrap();
    }
    let state = rt.state(&id).unwrap();
    assert_eq!(state.status, InstanceStatus::Crashed);
    assert!(state.next_retry_at.is_some());
    running.store(false, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if backend
                .verbs
                .lock()
                .unwrap()
                .iter()
                .filter(|verb| *verb == "start")
                .count()
                >= 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("deploy.start was not retried");
    rt.shutdown_background();
}

struct TestServices {
    calls: Mutex<Vec<String>>,
    backend: Arc<FakeSystemd>,
}
impl lantern_daemon::runtime::seams::ServiceControl for TestServices {
    fn has(&self, name: &str) -> bool {
        name == "redis"
    }
    fn ensure_running<'a>(
        &'a self,
        name: &'a str,
        _: Duration,
        _: CancellationToken,
    ) -> lantern_daemon::runtime::seams::ServiceFuture<'a> {
        Box::pin(async move {
            if self.backend.calls().iter().any(|s| s.starts_with("start ")) {
                return Err("instance started before service".into());
            }
            self.calls.lock().unwrap().push(name.into());
            Ok(())
        })
    }
}
#[tokio::test]
async fn runtime_shared_service_dependencies_run_before_spawn() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert(
        "app",
        &temp.0,
        "python3 -m http.server ${PORT}",
        &["service:redis"],
    );
    let backend = Arc::new(FakeSystemd::default());
    backend.set_auto_start(true);
    let services = Arc::new(TestServices {
        calls: Mutex::new(vec![]),
        backend: backend.clone(),
    });
    let adapters = lantern_daemon::runtime::RuntimeAdapters {
        preparer: Arc::new(lantern_daemon::runtime::seams::NoopPreparer),
        router: Arc::new(TestRouter::default()),
        services: services.clone(),
    };
    let rt = Runtime::with_all_adapters(
        config(&temp),
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready))),
        backend,
        OperationTracker::new(EventBus::new()),
        adapters,
    );
    let op = rt.start(&id, StartOpts).await.unwrap();
    assert_eq!(completed(&op).await, OperationStatus::Succeeded);
    assert_eq!(*services.calls.lock().unwrap(), vec!["redis"]);
}

#[tokio::test]
async fn runtime_health_failure_and_recovery_updates_status() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("app", &temp.0, "python3 -m http.server ${PORT}", &[]);
    let backend = Arc::new(FakeSystemd::default());
    backend.set_auto_start(true);
    let probe = Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready)));
    let mut cfg = config(&temp);
    cfg.settings.health.interval_secs = 1;
    cfg.settings.health.fail_threshold = 1;
    cfg.settings.health.grace_secs = 0;
    let rt = runtime(cfg, store, probe.clone(), backend);
    assert_eq!(
        completed(&rt.start(&id, StartOpts).await.unwrap()).await,
        OperationStatus::Succeeded
    );
    *probe.0.lock().unwrap() = ProbeMode::Waiting;
    let failed = wait_state(&rt, &id, |s| s.status == InstanceStatus::Unhealthy).await;
    assert_eq!(failed.health.state, lantern_core::model::HealthState::Failing);
    *probe.0.lock().unwrap() = ProbeMode::Ready;
    let recovered = wait_state(&rt, &id, |s| {
        s.status == InstanceStatus::Running && s.health.state == lantern_core::model::HealthState::Healthy
    })
    .await;
    assert!(recovered.health.recent.len() >= 2);
}

#[tokio::test]
async fn runtime_health_check_returns_registry_publication_failure() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("health-publish", &temp.0, "server ${PORT}", &[]);
    let backend = Arc::new(FakeSystemd::default());
    backend.set_auto_start(true);
    let rt = Runtime::with_scoped_journal(
        config(&temp),
        store.clone(),
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready))),
        backend,
        OperationTracker::new(EventBus::new()),
        lantern_daemon::runtime::RuntimeAdapters {
            preparer: Arc::new(lantern_daemon::runtime::seams::NoopPreparer),
            router: Arc::new(lantern_daemon::runtime::seams::UnavailableRouter),
            services: Arc::new(lantern_daemon::runtime::seams::UnavailableServices),
        },
        true,
    );
    assert_eq!(
        completed(&rt.start(&id, StartOpts).await.unwrap()).await,
        OperationStatus::Succeeded
    );
    store.fail_runtime_update.store(true, Ordering::SeqCst);
    let error = rt.check_health_now(&id).await.unwrap_err().to_string();
    assert!(error.contains("injected publication failure"), "{error}");
    rt.shutdown_background();
}

#[tokio::test]
async fn runtime_explicit_cancellation_cleans_up_start() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("app", &temp.0, "python3 -m http.server ${PORT}", &[]);
    let backend = Arc::new(FakeSystemd::default());
    backend.set_auto_start(true);
    let rt = runtime(
        config(&temp),
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Waiting))),
        backend.clone(),
    );
    let start = rt.start(&id, StartOpts).await.unwrap();
    wait_state(&rt, &id, |s| s.unit.is_some()).await;
    assert!(start.cancel());
    assert_eq!(completed(&start).await, OperationStatus::Cancelled);
    assert_eq!(rt.state(&id).unwrap().status, InstanceStatus::Stopped);
    assert!(backend.calls().iter().any(|s| s.starts_with("stop ")));
}

#[tokio::test]
async fn runtime_reconcile_adopts_running_unit_and_republishes() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("app", &temp.0, "python3 -m http.server ${PORT}", &[]);
    {
        let mut instances = store.instances.lock().unwrap();
        let runtime = &mut instances.get_mut(&id).unwrap().runtime;
        runtime.port = Some(39180);
        runtime.status = InstanceStatus::Running;
    }
    let backend = Arc::new(FakeSystemd::default());
    backend.set_snapshot(running_snapshot("lantern-test-supervisor-inst-app.service"));
    let router = Arc::new(TestRouter::default());
    let rt = Runtime::with_adapters(
        config(&temp),
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready))),
        backend.clone(),
        OperationTracker::new(EventBus::new()),
        Arc::new(lantern_daemon::runtime::seams::NoopPreparer),
        router.clone(),
    );
    let report = rt.reconcile().await;
    assert_eq!(report.adopted, vec![id.clone()]);
    assert!(report.errors.is_empty(), "{report:?}");
    let state = rt.state(&id).unwrap();
    assert_eq!(state.status, InstanceStatus::Running);
    assert_eq!(
        state.upstream,
        Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 39180)))
    );
    assert!(
        router
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.starts_with("publish app Socket"))
    );
    assert!(!backend.calls().iter().any(|s| s.starts_with("start ")));
}

#[tokio::test]
async fn runtime_reconcile_unready_unit_uses_elapsed_startup_time() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("old", &temp.0, "server ${PORT}", &[]);
    store.instances.lock().unwrap().get_mut(&id).unwrap().runtime.port = Some(39180);
    {
        let mut all = store.instances.lock().unwrap();
        Arc::make_mut(&mut all.get_mut(&id).unwrap().manifest)
            .run
            .startup_timeout = Some(1);
    }
    let backend = Arc::new(FakeSystemd::default());
    let mut row = running_snapshot("lantern-test-supervisor-inst-old.service");
    row.started_at = Some(Timestamp::from_second(Timestamp::now().as_second() - 2).unwrap());
    backend.set_snapshot(row);
    let rt = Runtime::with_scoped_journal(
        config(&temp),
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Waiting))),
        backend.clone(),
        OperationTracker::new(EventBus::new()),
        lantern_daemon::runtime::RuntimeAdapters {
            preparer: Arc::new(lantern_daemon::runtime::seams::NoopPreparer),
            router: Arc::new(lantern_daemon::runtime::seams::UnavailableRouter),
            services: Arc::new(lantern_daemon::runtime::seams::UnavailableServices),
        },
        true,
    );
    assert_eq!(rt.reconcile().await.adopted, vec![id.clone()]);
    let state = tokio::time::timeout(
        Duration::from_millis(800),
        wait_state(&rt, &id, |s| s.status == InstanceStatus::Crashed),
    )
    .await
    .expect("expired startup should time out immediately");
    assert_eq!(state.issue.unwrap().code, IssueCode::ReadinessTimeout);
    assert!(
        backend
            .calls()
            .iter()
            .any(|s| s == "stop lantern-test-supervisor-inst-old.service")
    );
    rt.shutdown_background();
}

#[tokio::test]
async fn runtime_history_limits_after_filtering_instance() {
    use std::os::unix::fs::PermissionsExt;
    if std::env::var_os("LANTERN_HISTORY_FIXTURE_CHILD").is_some() {
        let temp = Temp::new();
        let store = Arc::new(Store::default());
        let id = store.insert("history", &temp.0, "unused", &[]);
        let rt = runtime(
            config(&temp),
            store,
            Arc::new(FakeProbe(Mutex::new(ProbeMode::Waiting))),
            Arc::new(FakeSystemd::default()),
        );
        let lines = rt
            .log_history(&id, lantern_daemon::runtime::HistoryQuery { limit: 2 })
            .await
            .unwrap();
        assert_eq!(
            lines.iter().map(|l| l.line.as_str()).collect::<Vec<_>>(),
            ["old", "new"]
        );
        rt.shutdown_background();
        return;
    }
    let temp = Temp::new();
    let fake = temp.0.join("journalctl");
    std::fs::write(&fake, r##"#!/usr/bin/python3
import json, sys
rows = [('history', 'old'), ('history', 'new'), ('other', 'noise1'), ('other', 'noise2'), ('other', 'noise3')]
rows = [json.dumps({'_SYSTEMD_USER_UNIT': 'lantern-test-supervisor-inst-' + name + '.service', 'MESSAGE': line, '__REALTIME_TIMESTAMP': str(i+1), '__CURSOR': str(i+1)}) for i, (name, line) in enumerate(rows)]
args = sys.argv[1:]
if '--reverse' in args: rows.reverse()
limit = None
for i, arg in enumerate(args):
    if arg == '--lines': limit = args[i+1]
    if arg.startswith('--lines='): limit = arg.split('=', 1)[1]
if limit and limit != 'all': rows = rows[:int(limit)] if '--reverse' in args else rows[-int(limit):]
print('\n'.join(rows))
"##).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runtime_history_limits_after_filtering_instance",
            "--nocapture",
        ])
        .env("LANTERN_HISTORY_FIXTURE_CHILD", "1")
        .env("PATH", format!("{}:/usr/bin:/bin", temp.0.display()))
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn runtime_captures_and_refreshes_shell_env_when_sources_change() {
    use std::os::unix::fs::PermissionsExt;
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("app", &temp.0, "python3 -m http.server ${PORT}", &[]);
    let fake = Arc::new(FakeSystemd::default());
    fake.set_auto_start(true);
    let mut cfg = config(&temp);
    let fake_shell = temp.0.join("test-shell");
    let counter = temp.0.join("captures");
    std::fs::write(&fake_shell,format!("#!/bin/sh\nprintf x >> '{}'\nprintf '<<LANTERN_ENV_BEGIN>>PATH=/usr/bin:/bin\\0TEST_CAPTURE=value\\0<<LANTERN_ENV_END>>'\n",counter.display())).unwrap();
    std::fs::set_permissions(&fake_shell, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::remove_file(&cfg.shell_env_file).unwrap();
    cfg.settings.runtime.shell = fake_shell.to_string_lossy().into_owned();
    let rt = runtime(
        cfg.clone(),
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready))),
        fake,
    );
    assert_eq!(
        completed(&rt.start(&id, StartOpts).await.unwrap()).await,
        OperationStatus::Succeeded
    );
    assert!(rt.env_report().captured_at.is_some());
    assert!(
        std::fs::read_to_string(&cfg.shell_env_file)
            .unwrap()
            .contains("TEST_CAPTURE='value'")
    );
    assert_eq!(
        completed(&rt.stop(&id, StopOpts::default()).await.unwrap()).await,
        OperationStatus::Succeeded
    );
    std::fs::write(temp.0.join(".bashrc"), "# changed").unwrap();
    assert_eq!(
        completed(&rt.start(&id, StartOpts).await.unwrap()).await,
        OperationStatus::Succeeded
    );
    assert_eq!(std::fs::read_to_string(counter).unwrap(), "xx");
}

struct PreparedEnvironment;
impl InstancePreparer for PreparedEnvironment {
    fn ensure_ready<'a>(
        &'a self,
        instance: &'a Instance,
        port: Option<u16>,
        _: CancellationToken,
        progress: &'a lantern_daemon::operations::OpHandle,
    ) -> PrepareFuture<'a> {
        Box::pin(async move {
            assert!(port.is_some());
            progress.step("test preparation");
            let mut manifest = (*instance.manifest).clone();
            manifest.run.env.insert(
                "PREPARED_VALUE".into(),
                lantern_core::template::TemplateString::parse("manifest-updated").0,
            );
            manifest.run.env.insert(
                "RUN_VALUE".into(),
                lantern_core::template::TemplateString::parse("updated").0,
            );
            Ok(lantern_daemon::runtime::seams::PreparedInstance {
                manifest: Arc::new(manifest),
                worktree_env: BTreeMap::from([
                    ("RUN_VALUE".into(), "from-worktree".into()),
                    ("BRANCH_RESOURCE".into(), "branch-value".into()),
                ]),
            })
        })
    }
}
#[tokio::test]
async fn runtime_uses_prepared_manifest_and_branch_environment() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let primary = store.insert("app", &temp.0, "python3 -m http.server ${PORT}", &[]);
    let branch = InstanceId::derived(&primary.project(), "feature").unwrap();
    {
        let mut all = store.instances.lock().unwrap();
        let mut i = all.get(&primary).unwrap().clone();
        i.id = branch.clone();
        i.checkout.branch = Some("feature".into());
        i.checkout.kind = CheckoutKind::Worktree;
        all.insert(branch.clone(), i);
    }
    let backend = Arc::new(FakeSystemd::default());
    backend.set_auto_start(true);
    let rt = Runtime::with_preparer(
        config(&temp),
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready))),
        backend,
        OperationTracker::new(EventBus::new()),
        Arc::new(PreparedEnvironment),
    );
    assert_eq!(
        completed(&rt.start(&branch, StartOpts).await.unwrap()).await,
        OperationStatus::Succeeded
    );
    let script = std::fs::read_to_string(
        temp.0
            .join("run/lantern-test-supervisor-inst-app_feature.service.sh"),
    )
    .unwrap();
    assert!(script.contains("export RUN_VALUE='from-worktree'"));
    assert!(script.contains("export PREPARED_VALUE='manifest-updated'"));
    assert!(script.contains("export BRANCH_RESOURCE='branch-value'"));
}

#[tokio::test]
async fn runtime_retries_atomic_lease_conflicts_without_losing_other_writes() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("app", &temp.0, "python3 -m http.server ${PORT}", &[]);
    store.conflict_once.store(true, Ordering::SeqCst);
    store.conflict_port.store(temp.1, Ordering::SeqCst);
    let backend = Arc::new(FakeSystemd::default());
    backend.set_auto_start(true);
    let rt = runtime(
        config(&temp),
        store.clone(),
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready))),
        backend,
    );
    assert_eq!(
        completed(&rt.start(&id, StartOpts).await.unwrap()).await,
        OperationStatus::Succeeded
    );
    let leases = store.port_leases().unwrap();
    assert_eq!(leases.get(&InstanceId::parse("other").unwrap()), Some(&temp.1));
    assert_ne!(leases.get(&id), Some(&temp.1));
}

#[tokio::test]
async fn runtime_reserves_port_before_prepare_and_rechecks_it() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("app", &temp.0, "python3 -m http.server ${PORT}", &[]);
    let fake = Arc::new(FakeSystemd::default());
    fake.set_auto_start(true);
    let rt = runtime(
        config(&temp),
        store.clone(),
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready))),
        fake,
    );
    let port = rt.reserve_port(&id).await.unwrap();
    assert_eq!(store.port_leases().unwrap().get(&id), Some(&port));
    assert_eq!(
        completed(&rt.start(&id, StartOpts).await.unwrap()).await,
        OperationStatus::Succeeded
    );
    let active_port = rt.state(&id).unwrap().port.unwrap();
    assert!((temp.1..=temp.1 + 2).contains(&active_port));
    assert_eq!(store.port_leases().unwrap().get(&id), Some(&active_port));
}

#[tokio::test]
async fn runtime_dotenv_is_data_and_manifest_env_wins() {
    let temp = Temp::new();
    std::fs::write(
        temp.0.join(".env"),
        "FROM_FILE='literal $(id)'\nSHARED=from_file\n",
    )
    .unwrap();
    let store = Arc::new(Store::default());
    let id = store.insert("app", &temp.0, "python3 -m http.server ${PORT}", &[]);
    {
        let mut all = store.instances.lock().unwrap();
        let m = Arc::make_mut(&mut all.get_mut(&id).unwrap().manifest);
        m.deploy.env_file = Some(".env".into());
        m.run.env.insert(
            "SHARED".into(),
            lantern_core::template::TemplateString::parse("from_manifest").0,
        );
    }
    let fake = Arc::new(FakeSystemd::default());
    fake.set_auto_start(true);
    let rt = runtime(
        config(&temp),
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready))),
        fake,
    );
    assert_eq!(
        completed(&rt.start(&id, StartOpts).await.unwrap()).await,
        OperationStatus::Succeeded
    );
    let script =
        std::fs::read_to_string(temp.0.join("run/lantern-test-supervisor-inst-app.service.sh")).unwrap();
    assert!(script.contains("export FROM_FILE='literal $(id)'"));
    assert!(
        script.find("export SHARED='from_file'").unwrap()
            < script.find("export SHARED='from_manifest'").unwrap()
    );
}

#[tokio::test]
async fn runtime_rejects_env_file_symlink_escaping_checkout() {
    use std::os::unix::fs::symlink;
    let temp = Temp::new();
    let checkout = temp.0.join("checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    std::fs::write(temp.0.join("outside.env"), "SECRET=value\n").unwrap();
    symlink(temp.0.join("outside.env"), checkout.join(".env")).unwrap();
    let store = Arc::new(Store::default());
    let id = store.insert("app", &checkout, "python3 -m http.server ${PORT}", &[]);
    {
        let mut all = store.instances.lock().unwrap();
        Arc::make_mut(&mut all.get_mut(&id).unwrap().manifest)
            .deploy
            .env_file = Some(".env".into());
    }
    let fake = Arc::new(FakeSystemd::default());
    let rt = runtime(
        config(&temp),
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready))),
        fake.clone(),
    );
    assert_eq!(
        completed(&rt.start(&id, StartOpts).await.unwrap()).await,
        OperationStatus::Failed
    );
    assert!(!fake.calls().iter().any(|s| s.starts_with("start ")));
}

#[tokio::test]
async fn runtime_setup_oneshot_uses_launcher_and_is_cancellable() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("app", &temp.0, "python3 -m http.server ${PORT}", &[]);
    let backend = Arc::new(FakeSystemd::default());
    backend.set_auto_start(true);
    let rt = runtime(
        config(&temp),
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Ready))),
        backend.clone(),
    );
    let port = rt.reserve_port(&id).await.unwrap();
    let spec = rt
        .prepare_oneshot(
            &id,
            lantern_daemon::runtime::OneShotLaunch {
                verb: "setup".into(),
                cwd: temp.0.clone(),
                shell_command: lantern_core::template::TemplateString::parse("echo ${PORT}")
                    .0
                    .render_shell(),
                env: BTreeMap::from([("VALUE".into(), "$(literal)".into())]),
                timeout: Duration::from_secs(5),
            },
        )
        .await
        .unwrap();
    assert!(
        spec.unit
            .unit
            .starts_with("lantern-test-supervisor-op-app-setup-")
    );
    let script = std::fs::read_to_string(&spec.unit.script).unwrap();
    assert!(script.contains(&format!("export LANTERN_PORT='{port}'")));
    assert!(script.contains("export VALUE='$(literal)'"));
    assert!(script.contains("${LANTERN_PORT}"));
    let op = rt.run_oneshot(spec);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !backend.calls().iter().any(|s| s.starts_with("start ")) {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(op.cancel());
    assert_eq!(completed(&op).await, OperationStatus::Cancelled);
    assert!(backend.calls().iter().any(|s| s.starts_with("stop ")));
}

#[tokio::test]
async fn runtime_probe_failure_stops_the_started_unit() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("app", &temp.0, "python3 -m http.server ${PORT}", &[]);
    let fake = Arc::new(FakeSystemd::default());
    fake.set_auto_start(true);
    let rt = runtime(
        config(&temp),
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Error))),
        fake.clone(),
    );
    let op = rt.start(&id, StartOpts).await.unwrap();
    assert_eq!(completed(&op).await, OperationStatus::Failed);
    assert_eq!(rt.state(&id).unwrap().status, InstanceStatus::Crashed);
    assert!(fake.calls().iter().any(|s| s.starts_with("stop ")));
}

struct RefuseStop(FakeSystemd);
impl UserSystemd for RefuseStop {
    fn start<'a>(
        &'a self,
        spec: &'a lantern_daemon::runtime::systemd::UnitStart,
    ) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, ()> {
        self.0.start(spec)
    }
    fn stop<'a>(
        &'a self,
        _: &'a str,
        _: Duration,
    ) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, ()> {
        Box::pin(async {
            Err(lantern_daemon::runtime::systemd::UnitError::Timeout(
                "test stop".into(),
            ))
        })
    }
    fn reset_failed<'a>(&'a self, unit: &'a str) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, ()> {
        self.0.reset_failed(unit)
    }
    fn poll<'a>(
        &'a self,
        units: &'a [String],
    ) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, Vec<UnitSnapshot>> {
        self.0.poll(units)
    }
    fn list_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> lantern_daemon::runtime::systemd::SystemdFuture<'a, Vec<String>> {
        self.0.list_prefix(prefix)
    }
}
#[tokio::test]
async fn runtime_failed_cleanup_is_not_reported_as_stopped() {
    let temp = Temp::new();
    let store = Arc::new(Store::default());
    let id = store.insert("app", &temp.0, "python3 -m http.server ${PORT}", &[]);
    let fake = Arc::new(RefuseStop(FakeSystemd::default()));
    fake.0.set_auto_start(true);
    let rt = runtime(
        config(&temp),
        store,
        Arc::new(FakeProbe(Mutex::new(ProbeMode::Wrong))),
        fake,
    );
    let op = rt.start(&id, StartOpts).await.unwrap();
    assert_eq!(completed(&op).await, OperationStatus::Failed);
    assert_eq!(rt.state(&id).unwrap().status, InstanceStatus::Stopping);
}
