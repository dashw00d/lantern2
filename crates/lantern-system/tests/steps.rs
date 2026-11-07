use lantern_core::dirs::Dirs;
use lantern_system::{
    command::{CommandOutput, CommandRunner, CommandSpec, ProcessRunner},
    paths::LanternPaths,
    step::{OutcomeState, Step, StepState},
    steps::{
        dns_delegate::{DnsDelegateStep, delegate_contents},
        nss_trust::{Browser, NssTrustStep, discover},
        sysctl::{PortStrategy, PortsStep},
        user_units::{UserUnitsStep, unit_contents},
        v1_migrate::{V1MigrateStep, detect},
    },
};
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    paths: LanternPaths,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let dirs = Dirs {
            config: root.join("config/lantern"),
            state: root.join("state/lantern"),
            data: root.join("data/lantern"),
        };
        let paths = LanternPaths::new(
            dirs,
            root.join("home"),
            Some(root.join("run")),
            root.join("cache/lantern"),
            Some(root.join("system")),
        )
        .unwrap();
        fs::create_dir_all(paths.fake_root.as_ref().unwrap()).unwrap();
        Self { _temp: temp, paths }
    }
    fn write(&self, path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
}
#[derive(Default)]
struct State {
    commands: Vec<CommandSpec>,
    cap: bool,
    enabled: bool,
    nss: HashMap<PathBuf, Vec<u8>>,
}
struct RecordingRunner {
    state: RefCell<State>,
    live: PathBuf,
}
impl RecordingRunner {
    fn new(f: &Fixture) -> Self {
        Self {
            state: RefCell::new(State::default()),
            live: f.paths.sysctl_live.clone(),
        }
    }
}
impl CommandRunner for RecordingRunner {
    fn run(&self, spec: &CommandSpec) -> io::Result<CommandOutput> {
        assert!(spec.program.is_absolute());
        let mut state = self.state.borrow_mut();
        state.commands.push(spec.clone());
        let args: Vec<_> = spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let mut out = CommandOutput {
            success: true,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        match spec.program.to_str().unwrap() {
            "/usr/sbin/getcap" => {
                if state.cap {
                    out.stdout = b"caddy cap_net_bind_service=ep\n".to_vec();
                }
            }
            "/usr/sbin/setcap" => state.cap = true,
            "/usr/sbin/sysctl" => {
                let value = args.last().unwrap().split('=').next_back().unwrap();
                fs::create_dir_all(self.live.parent().unwrap())?;
                fs::write(&self.live, value)?;
            }
            "/usr/bin/systemctl" if args.iter().any(|a| a == "is-enabled") => out.success = state.enabled,
            "/usr/bin/systemctl" if args.iter().any(|a| a == "enable") => state.enabled = true,
            "/usr/bin/certutil" => {
                let db = PathBuf::from(args[1].strip_prefix("sql:").unwrap());
                if args.iter().any(|a| a == "-L") {
                    match state.nss.get(&db) {
                        Some(cert) => out.stdout = cert.clone(),
                        None => out.success = false,
                    }
                } else if args.iter().any(|a| a == "-N") {
                    fs::write(db.join("cert9.db"), b"")?;
                } else if args.iter().any(|a| a == "-D") {
                    state.nss.remove(&db);
                } else if args.iter().any(|a| a == "-A") {
                    let cert = fs::read(args.last().unwrap())?;
                    state.nss.insert(db, cert);
                }
            }
            _ => {}
        }
        Ok(out)
    }
}

#[test]
fn dns_is_exact_idempotent_and_validates_inputs() {
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    let dns = DnsDelegateStep {
        paths: &f.paths,
        runner: &runner,
        tld: "glow",
        listen: "127.0.0.1:5354".parse().unwrap(),
        from_package: false,
    };
    assert_eq!(dns.apply().state, OutcomeState::Changed);
    assert_eq!(
        fs::read_to_string(&f.paths.dns_delegate).unwrap(),
        "# Managed by Lantern (lantern init). Do not edit; changes are overwritten.\n# Sends every lookup below \".glow\" to Lantern's local DNS responder (run by `lantern daemon`).\n# \"~\" = route-only: this server is never used for any other name.\n[Delegate]\nDNS=127.0.0.1:5354\nDomains=~glow\n"
    );
    assert_eq!(dns.apply().state, OutcomeState::AlreadyDone);
    assert_eq!(
        runner
            .state
            .borrow()
            .commands
            .iter()
            .filter(|c| c.mutating)
            .count(),
        1
    );
    assert!(delegate_contents("glow\nDomains=~com", "127.0.0.1:5354".parse().unwrap()).is_err());
    assert!(delegate_contents("glow", "0.0.0.0:5354".parse().unwrap()).is_err());
    f.write(&f.paths.dns_delegate, b"custom");
    let packaged = DnsDelegateStep {
        from_package: true,
        ..dns
    };
    assert_eq!(packaged.apply().state, OutcomeState::Skipped);
    assert_eq!(fs::read(&f.paths.dns_delegate).unwrap(), b"custom");
}
#[test]
fn sysctl_and_setcap_are_selected_and_rechecked() {
    for strategy in [PortStrategy::Sysctl, PortStrategy::Setcap] {
        let f = Fixture::new();
        let runner = RecordingRunner::new(&f);
        f.write(&f.paths.sysctl_live, b"1024");
        f.write(&f.paths.bundled_caddy, b"fake");
        let step = PortsStep {
            paths: &f.paths,
            runner: &runner,
            http_port: 80,
            https_port: 443,
            caddy: &f.paths.bundled_caddy,
            strategy,
            from_package: false,
        };
        assert!(matches!(step.check(), StepState::Pending(_)));
        assert_eq!(step.apply().state, OutcomeState::Changed);
        assert_eq!(step.apply().state, OutcomeState::AlreadyDone);
        let commands = &runner.state.borrow().commands;
        assert_eq!(commands.iter().filter(|c| c.mutating).count(), 1);
        let actual = &commands.iter().find(|c| c.mutating).unwrap().program;
        assert_eq!(
            actual.file_name(),
            Some(OsStr::new(if strategy == PortStrategy::Sysctl {
                "sysctl"
            } else {
                "setcap"
            }))
        );
    }
}
#[test]
fn setcap_refuses_arbitrary_target() {
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    let bad = f.paths.home.join("fake-caddy");
    f.write(&bad, b"fake");
    let step = PortsStep {
        paths: &f.paths,
        runner: &runner,
        http_port: 80,
        https_port: 443,
        caddy: &bad,
        strategy: PortStrategy::Setcap,
        from_package: false,
    };
    assert_eq!(step.apply().state, OutcomeState::Failed);
    assert!(!runner.state.borrow().commands.iter().any(|c| c.mutating));
}
#[test]
fn nss_finds_snap_and_flatpak_and_rechecks_der() {
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    f.write(
        &f.paths.root_cert,
        b"-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n",
    );
    fs::create_dir_all(f.paths.home.join("snap/chromium/current")).unwrap();
    for path in [
        ".mozilla/firefox/normal/cert9.db",
        "snap/firefox/common/.mozilla/firefox/snap/cert9.db",
        ".var/app/org.mozilla.firefox/.mozilla/firefox/flatpak/cert9.db",
    ] {
        f.write(&f.paths.home.join(path), b"");
    }
    let dbs = discover(&f.paths);
    assert_eq!(dbs.len(), 5);
    assert_eq!(dbs.iter().filter(|d| d.browser == Browser::Firefox).count(), 3);
    let step = NssTrustStep {
        paths: &f.paths,
        runner: &runner,
        certutil: Some(Path::new("/usr/bin/certutil")),
    };
    assert_eq!(step.apply().state, OutcomeState::Changed);
    assert_eq!(step.apply().state, OutcomeState::AlreadyDone);
    let recorded = &runner.state.borrow().commands;
    assert_eq!(
        recorded
            .iter()
            .filter(|c| c.args.iter().any(|a| a == "-A"))
            .count(),
        5
    );
    assert!(recorded.iter().all(|c| c.env.is_empty()));
}
#[test]
fn user_service_enable_and_tray_are_idempotent() {
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    let exe = f.paths.home.join("build with spaces/lantern");
    let step = UserUnitsStep {
        paths: &f.paths,
        runner: &runner,
        executable: &exe,
        tray_autostart: true,
    };
    assert!(unit_contents(Path::new("/tmp/x\nInject=yes")).is_err());
    assert_eq!(step.apply().state, OutcomeState::Changed);
    assert_eq!(step.apply().state, OutcomeState::AlreadyDone);
    assert!(
        fs::read_to_string(&f.paths.user_unit)
            .unwrap()
            .contains("ExecStart=\"")
    );
    assert!(
        fs::read_to_string(&f.paths.autostart)
            .unwrap()
            .contains("gui --tray")
    );
}
#[test]
fn v1_cleanup_is_guarded_and_only_removes_listed_fake_files() {
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    let sudo = f.paths.system(Path::new("/etc/sudoers.d/lantern"));
    let site = f.paths.system(Path::new("/etc/caddy/sites.d/v1.caddy"));
    let other = f.paths.system(Path::new("/etc/caddy/sites.d/keep.txt"));
    f.write(&sudo, b"v1");
    f.write(&site, b"v1");
    f.write(&other, b"keep");
    assert_eq!(detect(&f.paths).len(), 2);
    let guarded = V1MigrateStep {
        paths: &f.paths,
        runner: &runner,
        allow_fake_cleanup: false,
    };
    assert_eq!(guarded.apply().state, OutcomeState::Skipped);
    assert!(sudo.exists());
    let enabled = V1MigrateStep {
        allow_fake_cleanup: true,
        ..guarded
    };
    assert_eq!(enabled.apply().state, OutcomeState::Changed);
    assert_eq!(enabled.apply().state, OutcomeState::AlreadyDone);
    assert!(!sudo.exists() && !site.exists() && other.exists());
}
#[test]
fn real_runner_blocks_mutation_in_fake_mode() {
    let runner = ProcessRunner { fake_root: true };
    let cmd = CommandSpec::new("/usr/bin/touch", ["/forbidden"], true);
    assert_eq!(
        runner.run(&cmd).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[derive(Debug)]
struct OfflineNetwork;
impl lantern_system::doctor::NetworkProbe for OfflineNetwork {
    fn check(&self, _: lantern_system::doctor::Probe) -> Result<String, String> {
        Err("offline fake".into())
    }
}
#[test]
fn doctor_is_read_only_and_reports_expected_checks() {
    use lantern_system::doctor::{DoctorCtx, run};
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    let settings = lantern_core::settings::Settings::default();
    let ctx = DoctorCtx {
        paths: &f.paths,
        settings: &settings,
        runner: &runner,
        network: &OfflineNetwork,
        caddy: &f.paths.bundled_caddy,
        certutil: Some(Path::new("/usr/bin/certutil")),
        search_path: vec![],
    };
    let checks = run(&ctx);
    for id in [
        "daemon.running",
        "session.systemd",
        "caddy.binary",
        "caddy.running",
        "ports.bind",
        "ports.free",
        "dns.resolved",
        "dns.delegate",
        "dns.responder",
        "dns.system",
        "tls.ca",
        "tls.system",
        "tls.certutil",
        "tls.nss.chrome",
        "tls.serving",
        "e2e.https",
        "browser.doh",
        "conflict.v1",
        "conflict.caddy",
        "path.shadow",
        "services.mailpit",
        "services.redis",
        "services.postgres",
        "php.fpm",
        "linger",
    ] {
        assert!(checks.iter().any(|c| c.id == id), "missing {id}");
    }
    assert!(checks.iter().all(|c| c.fix.is_some()));
    assert!(runner.state.borrow().commands.iter().all(|c| !c.mutating));
    assert!(!f.paths.user_unit.exists());
    assert!(!f.paths.dns_delegate.exists());
}
