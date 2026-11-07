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
    active: bool,
    nss: HashMap<PathBuf, Vec<u8>>,
}
struct RecordingRunner {
    state: RefCell<State>,
    live: PathBuf,
    system_cert: PathBuf,
    system_cert_link: PathBuf,
}
impl RecordingRunner {
    fn new(f: &Fixture) -> Self {
        Self {
            state: RefCell::new(State::default()),
            live: f.paths.sysctl_live.clone(),
            system_cert: f.paths.system_cert.clone(),
            system_cert_link: f.paths.system_cert_link.clone(),
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
            "/usr/sbin/update-ca-certificates" => {
                fs::create_dir_all(self.system_cert_link.parent().unwrap())?;
                if fs::symlink_metadata(&self.system_cert_link).is_ok() {
                    fs::remove_file(&self.system_cert_link)?;
                }
                if self.system_cert.exists() {
                    std::os::unix::fs::symlink(&self.system_cert, &self.system_cert_link)?;
                }
            }
            "/usr/sbin/setcap" => state.cap = true,
            "/usr/sbin/sysctl" => {
                let value = args.last().unwrap().split('=').next_back().unwrap();
                fs::create_dir_all(self.live.parent().unwrap())?;
                fs::write(&self.live, value)?;
            }
            "/usr/bin/systemctl" if args.iter().any(|a| a == "is-enabled") => out.success = state.enabled,
            "/usr/bin/systemctl"
                if args.iter().any(|a| a == "is-active") && args.iter().any(|a| a == "lantern.service") =>
            {
                out.success = state.active
            }
            "/usr/bin/systemctl" if args.iter().any(|a| a == "enable") => {
                state.enabled = true;
                state.active = true;
            }
            "/usr/bin/certutil" => {
                let db = PathBuf::from(args[1].strip_prefix("sql:").unwrap());
                if args.iter().any(|a| a == "-L") {
                    match state.nss.get(&db) {
                        Some(cert) if args.iter().any(|a| a == "-n") => out.stdout = cert.clone(),
                        Some(_) => out.stdout = b"Certificate Nickname                                         Trust Attributes\nLantern Local CA                                             C,,\n".to_vec(),
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
        assert!(commands.iter().filter(|c| c.mutating).all(|c| c.fake_root));
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
    lantern_system::ca::ensure_root(&f.paths, "glow").unwrap();
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
        tld: "glow",
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
    runner.state.borrow_mut().active = false;
    assert!(matches!(step.check(), StepState::Pending(_)));
    assert_eq!(step.apply().state, OutcomeState::Changed);
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
    let mut fake = CommandSpec::new("/definitely-not-a-real-lantern-command", ["unused"], true);
    fake.fake_root = true;
    assert_eq!(
        ProcessRunner { fake_root: false }.run(&fake).unwrap_err().kind(),
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

#[derive(Debug)]
struct FakeStep {
    id: &'static str,
    root: bool,
    done: std::cell::Cell<bool>,
}
impl Step for FakeStep {
    fn id(&self) -> &'static str {
        self.id
    }
    fn description(&self) -> &'static str {
        "fake"
    }
    fn needs_root(&self) -> bool {
        self.root
    }
    fn fix_command(&self) -> &'static str {
        "fake"
    }
    fn check(&self) -> StepState {
        if self.done.get() {
            StepState::Done("done".into())
        } else {
            StepState::Pending("pending".into())
        }
    }
    fn apply(&self) -> lantern_system::step::StepOutcome {
        let prior = self.done.replace(true);
        lantern_system::step::StepOutcome::new(
            self.id,
            if prior {
                OutcomeState::AlreadyDone
            } else {
                OutcomeState::Changed
            },
            "fake",
        )
    }
}
#[test]
fn init_composes_steps_with_uid_and_settings_validation() {
    use lantern_system::init::{
        InitCtx, SystemInitCtx, pkexec_invocation, run_init, run_system_steps, run_user_steps,
    };
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    f.write(&f.paths.sysctl_live, b"1024");
    lantern_system::ca::ensure_root(&f.paths, "glow").unwrap();
    let exe = f.paths.home.join("build/lantern");
    f.write(&exe, b"binary");
    let ca = FakeStep {
        id: "ca",
        root: false,
        done: std::cell::Cell::new(false),
    };
    let trust = FakeStep {
        id: "system_trust",
        root: true,
        done: std::cell::Cell::new(false),
    };
    let user = InitCtx {
        paths: &f.paths,
        runner: &runner,
        tld: "glow",
        executable: &exe,
        certutil: Some(Path::new("/usr/bin/certutil")),
        tray_autostart: true,
        ca_step: Some(&ca),
    };
    assert!(
        run_user_steps(&user)
            .iter()
            .all(|o| o.state == OutcomeState::Changed)
    );
    assert!(
        run_user_steps(&user)
            .iter()
            .all(|o| o.state == OutcomeState::AlreadyDone)
    );
    let settings = lantern_core::settings::Settings::default();
    let system = SystemInitCtx {
        paths: &f.paths,
        settings: &settings,
        runner: &runner,
        uid: 1000,
        port_strategy: PortStrategy::Sysctl,
        from_package: false,
        migrate_v1: true,
        allow_fake_cleanup: false,
        system_trust_step: Some(&trust),
    };
    let before_plan = runner
        .state
        .borrow()
        .commands
        .iter()
        .filter(|c| c.mutating)
        .count();
    let plan = run_init(&user, &system).unwrap();
    assert!(plan.pkexec.is_some());
    let setcap_plan = run_init(
        &user,
        &SystemInitCtx {
            port_strategy: PortStrategy::Setcap,
            ..system
        },
    )
    .unwrap();
    let args = &setcap_plan.pkexec.as_ref().unwrap().args;
    assert!(args.iter().any(|arg| arg == "--ports=setcap"));
    assert!(args.iter().any(|arg| arg == "--json"));
    assert_eq!(
        runner
            .state
            .borrow()
            .commands
            .iter()
            .filter(|c| c.mutating)
            .count(),
        before_plan
    );
    assert!(
        run_system_steps(&system)
            .iter()
            .all(|o| o.state == OutcomeState::Changed || o.state == OutcomeState::AlreadyDone)
    );
    assert!(
        run_system_steps(&system)
            .iter()
            .all(|o| o.state == OutcomeState::AlreadyDone)
    );
    assert!(run_init(&user, &system).unwrap().pkexec.is_none());
    let before = runner.state.borrow().commands.len();
    let invalid = SystemInitCtx { uid: 0, ..system };
    assert_eq!(run_system_steps(&invalid)[0].state, OutcomeState::Failed);
    assert_eq!(runner.state.borrow().commands.len(), before);
    let cmd = pkexec_invocation(&exe, 1000).unwrap();
    assert_eq!(cmd.program, Path::new("/usr/bin/pkexec"));
    assert_eq!(cmd.args[0], fs::canonicalize(&exe).unwrap().as_os_str());
    assert_eq!(cmd.args[1..], ["init", "--system-steps", "--uid", "1000"]);
    assert!(pkexec_invocation(&exe, 0).is_err());
    assert!(
        !runner
            .state
            .borrow()
            .commands
            .iter()
            .any(|c| c.program == Path::new("/usr/bin/pkexec"))
    );
}

struct FailReload<'a>(&'a RecordingRunner);
impl CommandRunner for FailReload<'_> {
    fn run(&self, spec: &CommandSpec) -> io::Result<CommandOutput> {
        if spec.args.iter().any(|a| a == "reload") {
            self.0.state.borrow_mut().commands.push(spec.clone());
            return Ok(CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"reload failed".to_vec(),
            });
        }
        self.0.run(spec)
    }
}
#[test]
fn failed_dns_reload_restores_previous_file_and_remains_pending() {
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    let failing = FailReload(&runner);
    f.write(&f.paths.dns_delegate, b"prior config\n");
    let dns = DnsDelegateStep {
        paths: &f.paths,
        runner: &failing,
        tld: "glow",
        listen: "127.0.0.1:5354".parse().unwrap(),
        from_package: false,
    };
    assert_eq!(dns.apply().state, OutcomeState::Failed);
    assert_eq!(fs::read(&f.paths.dns_delegate).unwrap(), b"prior config\n");
    assert!(matches!(dns.check(), StepState::Pending(_)));
}
#[test]
fn root_file_symlinks_are_rejected_and_v1_cleanup_does_not_follow_parents() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    let outside = f.paths.home.join("external");
    fs::create_dir_all(&outside).unwrap();
    let target = outside.join("keep.caddy");
    f.write(&target, b"keep");
    let sites = f.paths.system(Path::new("/etc/caddy/sites.d"));
    fs::create_dir_all(sites.parent().unwrap()).unwrap();
    symlink(&outside, &sites).unwrap();
    let step = V1MigrateStep {
        paths: &f.paths,
        runner: &runner,
        allow_fake_cleanup: true,
    };
    assert_eq!(step.apply().state, OutcomeState::Failed);
    assert_eq!(fs::read(&target).unwrap(), b"keep");
    fs::create_dir_all(f.paths.dns_delegate.parent().unwrap()).unwrap();
    symlink(&target, &f.paths.dns_delegate).unwrap();
    let dns = DnsDelegateStep {
        paths: &f.paths,
        runner: &runner,
        tld: "glow",
        listen: "127.0.0.1:5354".parse().unwrap(),
        from_package: false,
    };
    assert_eq!(dns.apply().state, OutcomeState::Failed);
    assert_eq!(fs::read(&target).unwrap(), b"keep");
}
#[test]
fn nss_matching_certificate_without_ssl_ca_trust_is_not_done() {
    struct Untrusted<'a>(&'a RecordingRunner);
    impl CommandRunner for Untrusted<'_> {
        fn run(&self, command: &CommandSpec) -> io::Result<CommandOutput> {
            let mut out = self.0.run(command)?;
            if command.program == Path::new("/usr/bin/certutil")
                && command.args.iter().any(|a| a == "-L")
                && !command.args.iter().any(|a| a == "-n")
                && out.success
            {
                out.stdout = b"Lantern Local CA   ,,\nLantern Local CA Copied    C,,\n".to_vec();
            }
            Ok(out)
        }
    }
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    lantern_system::ca::ensure_root(&f.paths, "glow").unwrap();
    let step = NssTrustStep {
        paths: &f.paths,
        runner: &runner,
        tld: "glow",
        certutil: Some(Path::new("/usr/bin/certutil")),
    };
    assert_eq!(step.apply().state, OutcomeState::Changed);
    let untrusted = Untrusted(&runner);
    let other = NssTrustStep {
        runner: &untrusted,
        ..step
    };
    assert!(matches!(other.check(), StepState::Pending(_)));
}

#[test]
fn generated_ca_has_critical_constrained_der_and_private_key() {
    use lantern_system::ca::{CaState, ensure_root, inspect_der, inspect_root};
    use lantern_system::steps::nss_trust::certificate_der;
    use std::os::unix::fs::PermissionsExt;
    use x509_parser::{extensions::GeneralName, prelude::*};
    let f = Fixture::new();
    let created = ensure_root(&f.paths, "glow").unwrap();
    let info = match created {
        CaState::Created(info) => info,
        _ => panic!("expected creation"),
    };
    assert_eq!(info.permitted_subtree, ".glow");
    assert!(info.constraint_critical);
    assert_eq!(
        fs::metadata(&f.paths.root_key).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&f.paths.ca_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let der = certificate_der(&fs::read(&f.paths.root_cert).unwrap()).unwrap();
    let (_, cert) = parse_x509_certificate(&der).unwrap();
    let nc = cert.name_constraints().unwrap().unwrap();
    assert!(nc.critical);
    assert!(matches!(
        nc.value.permitted_subtrees.as_ref().unwrap()[0].base,
        GeneralName::DNSName(".glow")
    ));
    assert!(cert.verify_signature(None).is_ok());
    assert_eq!(inspect_root(&f.paths, "glow", 30).unwrap(), info);
    assert!(matches!(
        ensure_root(&f.paths, "glow").unwrap(),
        CaState::Existing(_)
    ));
    assert!(inspect_der(&der, "other").is_err());
    let mut tampered = der.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 1;
    assert!(inspect_der(&tampered, "glow").is_err());
    let original = fs::read(&f.paths.root_cert).unwrap();
    assert!(ensure_root(&f.paths, "other").is_err());
    assert_eq!(fs::read(&f.paths.root_cert).unwrap(), original);
}

#[test]
fn system_trust_verifies_input_reencodes_and_is_idempotent() {
    use lantern_system::{ca::ensure_root, steps::system_trust::SystemTrustStep};
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    ensure_root(&f.paths, "glow").unwrap();
    let trust = SystemTrustStep {
        paths: &f.paths,
        runner: &runner,
        tld: "glow",
    };
    assert!(matches!(trust.check(), StepState::Pending(_)));
    assert_eq!(trust.apply().state, OutcomeState::Changed);
    assert_eq!(trust.apply().state, OutcomeState::AlreadyDone);
    assert_eq!(
        fs::metadata(&f.paths.system_cert).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert!(f.paths.system_cert_link.exists());
    let actions = runner.state.borrow();
    assert_eq!(
        actions
            .commands
            .iter()
            .filter(|c| c.program == Path::new("/usr/sbin/update-ca-certificates"))
            .count(),
        1
    );
    assert!(
        actions
            .commands
            .iter()
            .filter(|c| c.mutating)
            .all(|c| c.fake_root)
    );
    drop(actions);
    let wrong = SystemTrustStep {
        tld: "other",
        ..trust
    };
    assert!(matches!(wrong.check(), StepState::Failed(_)));
    assert_eq!(wrong.apply().state, OutcomeState::Failed);
}
#[test]
fn system_trust_rejects_symlink_or_oversized_root_without_commands() {
    use lantern_system::steps::system_trust::SystemTrustStep;
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    let external = f.paths.home.join("cert");
    f.write(&external, b"untrusted");
    fs::create_dir_all(&f.paths.ca_dir).unwrap();
    std::os::unix::fs::symlink(&external, &f.paths.root_cert).unwrap();
    let trust = SystemTrustStep {
        paths: &f.paths,
        runner: &runner,
        tld: "glow",
    };
    assert_eq!(trust.apply().state, OutcomeState::Failed);
    fs::remove_file(&f.paths.root_cert).unwrap();
    f.write(&f.paths.root_cert, &vec![b'x'; 16385]);
    assert_eq!(trust.apply().state, OutcomeState::Failed);
    assert!(runner.state.borrow().commands.is_empty());
}
#[test]
fn init_defaults_to_real_ca_and_fake_system_trust() {
    use lantern_system::init::{InitCtx, SystemInitCtx, run_init, run_system_steps};
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    f.write(&f.paths.sysctl_live, b"1024");
    let exe = f.paths.home.join("build/lantern");
    f.write(&exe, b"binary");
    let settings = lantern_core::settings::Settings::default();
    let user = InitCtx {
        paths: &f.paths,
        runner: &runner,
        tld: "glow",
        executable: &exe,
        certutil: Some(Path::new("/usr/bin/certutil")),
        tray_autostart: true,
        ca_step: None,
    };
    let system = SystemInitCtx {
        paths: &f.paths,
        settings: &settings,
        runner: &runner,
        uid: 1000,
        port_strategy: PortStrategy::Sysctl,
        from_package: false,
        migrate_v1: false,
        allow_fake_cleanup: false,
        system_trust_step: None,
    };
    let plan = run_init(&user, &system).unwrap();
    assert!(
        plan.user_outcomes
            .iter()
            .all(|o| o.state == OutcomeState::Changed)
    );
    assert!(plan.pkexec.is_some());
    assert!(
        run_system_steps(&system)
            .iter()
            .all(|o| o.state == OutcomeState::Changed)
    );
    let again = run_init(&user, &system).unwrap();
    assert!(
        again
            .user_outcomes
            .iter()
            .all(|o| o.state == OutcomeState::AlreadyDone)
    );
    assert!(
        again
            .system_steps
            .iter()
            .all(|s| matches!(s.state, StepState::Done(_)))
    );
    assert!(again.pkexec.is_none());
}

#[test]
fn ca_rotation_stops_caddy_archives_old_roots_and_restarts() {
    use lantern_system::ca::{CaStep, ensure_root, inspect_root};
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    ensure_root(&f.paths, "glow").unwrap();
    let old = fs::read(&f.paths.root_cert).unwrap();
    f.write(&f.paths.intermediate_cert, b"old-intermediate");
    f.write(&f.paths.certificates.join("host/leaf.crt"), b"old-leaf");
    let step = CaStep {
        paths: &f.paths,
        tld: "glow2",
        runner: Some(&runner),
    };
    assert!(matches!(step.check(), StepState::Pending(_)));
    assert_eq!(step.apply().state, OutcomeState::Changed);
    assert_eq!(step.apply().state, OutcomeState::AlreadyDone);
    assert_eq!(
        inspect_root(&f.paths, "glow2", 90).unwrap().permitted_subtree,
        ".glow2"
    );
    let archive = fs::read_dir(&f.paths.ca_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with(".old-"))
        .unwrap();
    assert_eq!(fs::read(archive.join("root.crt")).unwrap(), old);
    assert_eq!(
        fs::read(archive.join("intermediate.crt")).unwrap(),
        b"old-intermediate"
    );
    assert_eq!(
        fs::read(archive.join("certificates/host/leaf.crt")).unwrap(),
        b"old-leaf"
    );
    assert!(!f.paths.intermediate_cert.exists() && !f.paths.certificates.exists());
    let calls = &runner.state.borrow().commands;
    let ops: Vec<_> = calls
        .iter()
        .filter(|c| c.mutating)
        .map(|c| {
            c.args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0][1], "stop");
    assert_eq!(ops[1][1], "start");
    assert!(calls.iter().filter(|c| c.mutating).all(|c| c.fake_root));
}
#[test]
fn ca_rotation_rolls_back_if_caddy_cannot_restart() {
    use lantern_system::ca::{CaStep, ensure_root, inspect_root};
    struct FailFirstStart<'a> {
        base: &'a RecordingRunner,
        failed: std::cell::Cell<bool>,
    }
    impl CommandRunner for FailFirstStart<'_> {
        fn run(&self, spec: &CommandSpec) -> io::Result<CommandOutput> {
            if spec.args.iter().any(|a| a == "start") && !self.failed.replace(true) {
                self.base.state.borrow_mut().commands.push(spec.clone());
                return Ok(CommandOutput {
                    success: false,
                    stdout: vec![],
                    stderr: b"simulated start failure".to_vec(),
                });
            }
            self.base.run(spec)
        }
    }
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    ensure_root(&f.paths, "glow").unwrap();
    let old = fs::read(&f.paths.root_cert).unwrap();
    f.write(&f.paths.intermediate_cert, b"old-intermediate");
    let failure = FailFirstStart {
        base: &runner,
        failed: std::cell::Cell::new(false),
    };
    let step = CaStep {
        paths: &f.paths,
        tld: "glow2",
        runner: Some(&failure),
    };
    assert_eq!(step.apply().state, OutcomeState::Failed);
    assert_eq!(fs::read(&f.paths.root_cert).unwrap(), old);
    assert_eq!(fs::read(&f.paths.intermediate_cert).unwrap(), b"old-intermediate");
    assert!(inspect_root(&f.paths, "glow", 90).is_ok());
    assert_eq!(
        runner
            .state
            .borrow()
            .commands
            .iter()
            .filter(|c| c.args.iter().any(|a| a == "start"))
            .count(),
        2
    );
}

#[test]
fn orphaned_caddy_leaves_are_archived_before_new_root() {
    use lantern_system::ca::{CaStep, inspect_root};
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    f.write(&f.paths.certificates.join("host/old.crt"), b"old-leaf");
    let step = CaStep {
        paths: &f.paths,
        tld: "glow",
        runner: Some(&runner),
    };
    assert_eq!(step.apply().state, OutcomeState::Changed);
    assert!(!f.paths.certificates.exists());
    assert!(inspect_root(&f.paths, "glow", 90).is_ok());
    let archive = fs::read_dir(&f.paths.ca_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with(".old-"))
        .unwrap();
    assert_eq!(
        fs::read(archive.join("certificates/host/old.crt")).unwrap(),
        b"old-leaf"
    );
}
#[test]
fn high_ports_still_check_the_actual_kernel_threshold() {
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    f.write(&f.paths.sysctl_live, b"65535");
    let step = PortsStep {
        paths: &f.paths,
        runner: &runner,
        http_port: 8080,
        https_port: 8443,
        caddy: &f.paths.bundled_caddy,
        strategy: PortStrategy::Sysctl,
        from_package: false,
    };
    assert!(matches!(step.check(), StepState::Pending(_)));
    assert_eq!(step.apply().state, OutcomeState::Changed);
    assert!(matches!(step.check(), StepState::Done(_)));
}

#[test]
fn browser_and_system_trust_reject_an_unconstrained_root() {
    use lantern_system::steps::system_trust::SystemTrustStep;
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    let mut params = rcgen::CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign];
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    f.write(&f.paths.root_cert, cert.pem().as_bytes());
    f.write(&f.paths.root_key, key.serialize_pem().as_bytes());
    fs::set_permissions(&f.paths.root_key, fs::Permissions::from_mode(0o600)).unwrap();
    let browser = NssTrustStep {
        paths: &f.paths,
        runner: &runner,
        tld: "glow",
        certutil: Some(Path::new("/usr/bin/certutil")),
    };
    let system = SystemTrustStep {
        paths: &f.paths,
        runner: &runner,
        tld: "glow",
    };
    assert_eq!(browser.apply().state, OutcomeState::Failed);
    assert_eq!(system.apply().state, OutcomeState::Failed);
    assert!(runner.state.borrow().commands.is_empty());
}
#[test]
fn failed_ca_prevents_browser_and_unit_changes() {
    use lantern_system::init::{InitCtx, run_user_steps};
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    let exe = f.paths.home.join("lantern");
    f.write(&exe, b"binary");
    let ctx = InitCtx {
        paths: &f.paths,
        runner: &runner,
        tld: "bad\ntld",
        executable: &exe,
        certutil: Some(Path::new("/usr/bin/certutil")),
        tray_autostart: true,
        ca_step: None,
    };
    let outcomes = run_user_steps(&ctx);
    assert_eq!(outcomes[1].state, OutcomeState::Failed);
    assert_eq!(outcomes[2].state, OutcomeState::Skipped);
    assert_eq!(outcomes[3].state, OutcomeState::Skipped);
    assert!(runner.state.borrow().commands.is_empty());
    assert!(!f.paths.user_unit.exists());
}
#[test]
fn doctor_detects_foreign_api_listener_without_a_socket() {
    use lantern_core::api::CheckStatus;
    use lantern_system::doctor::{DoctorCtx, run};
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    let tcp = f.paths.system(Path::new("/proc/net/tcp"));
    f.write(
        &tcp,
        b"header\n 0: 0100007F:12A9 00000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 999\n",
    );
    let process = f.paths.system(Path::new("/proc/42"));
    fs::create_dir_all(process.join("fd")).unwrap();
    std::os::unix::fs::symlink("socket:[999]", process.join("fd/4")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/old-daemon", process.join("exe")).unwrap();
    let settings = lantern_core::settings::Settings::default();
    let ctx = DoctorCtx {
        paths: &f.paths,
        settings: &settings,
        runner: &runner,
        network: &OfflineNetwork,
        caddy: &f.paths.bundled_caddy,
        certutil: None,
        search_path: vec![],
    };
    let check = run(&ctx).into_iter().find(|c| c.id == "conflict.v1").unwrap();
    assert_eq!(check.status, CheckStatus::Warn);
    assert!(check.detail.contains("pid 42"));
    fs::remove_file(process.join("exe")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/lantern", process.join("exe")).unwrap();
    let check = run(&ctx).into_iter().find(|c| c.id == "conflict.v1").unwrap();
    assert_eq!(check.status, CheckStatus::Ok);
}

#[test]
fn package_setup_without_user_preserves_custom_files_and_skips_ca() {
    use lantern_system::init::{SystemInitCtx, run_system_steps};
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    f.write(&f.paths.sysctl_live, b"1024");
    f.write(&f.paths.sysctl_conf, b"custom sysctl\n");
    f.write(&f.paths.dns_delegate, b"custom delegate\n");
    let settings = lantern_core::settings::Settings::default();
    let ctx = SystemInitCtx {
        paths: &f.paths,
        settings: &settings,
        runner: &runner,
        uid: 0,
        port_strategy: PortStrategy::Sysctl,
        from_package: true,
        migrate_v1: false,
        allow_fake_cleanup: false,
        system_trust_step: None,
    };
    let outcomes = run_system_steps(&ctx);
    assert!(outcomes.iter().all(|o| o.state == OutcomeState::Skipped));
    assert_eq!(fs::read(&f.paths.sysctl_conf).unwrap(), b"custom sysctl\n");
    assert_eq!(fs::read(&f.paths.dns_delegate).unwrap(), b"custom delegate\n");
    assert!(!runner.state.borrow().commands.iter().any(|c| c.mutating));
}

#[test]
fn undo_is_idempotent_and_retains_trust_unless_requested() {
    use lantern_system::init::{SystemInitCtx, run_system_steps, undo_system_steps};
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    f.write(&f.paths.sysctl_live, b"1024");
    lantern_system::ca::ensure_root(&f.paths, "glow").unwrap();
    let settings = lantern_core::settings::Settings::default();
    let ctx = SystemInitCtx {
        paths: &f.paths,
        settings: &settings,
        runner: &runner,
        uid: 1000,
        port_strategy: PortStrategy::Sysctl,
        from_package: false,
        migrate_v1: false,
        allow_fake_cleanup: false,
        system_trust_step: None,
    };
    assert!(
        run_system_steps(&ctx)
            .iter()
            .all(|s| s.state == OutcomeState::Changed)
    );
    let undo = undo_system_steps(&ctx, false);
    assert_eq!(
        undo.iter().map(|s| s.state).collect::<Vec<_>>(),
        vec![
            OutcomeState::Changed,
            OutcomeState::Changed,
            OutcomeState::Skipped
        ]
    );
    assert!(f.paths.system_cert.exists());
    let again = undo_system_steps(&ctx, true);
    assert_eq!(
        again.iter().map(|s| s.state).collect::<Vec<_>>(),
        vec![
            OutcomeState::AlreadyDone,
            OutcomeState::AlreadyDone,
            OutcomeState::Changed
        ]
    );
    assert!(!f.paths.system_cert.exists());
    assert!(
        undo_system_steps(&ctx, true)
            .iter()
            .all(|s| s.state == OutcomeState::AlreadyDone)
    );
    assert!(
        runner
            .state
            .borrow()
            .commands
            .iter()
            .filter(|c| c.mutating)
            .all(|c| c.fake_root)
    );
}

#[test]
fn paths_cover_user_service_browser_and_fake_root_locations() {
    let f = Fixture::new();
    assert_eq!(f.paths.caddy_data, f.paths.dirs.data.join("caddy"));
    assert_eq!(f.paths.postgres_data, f.paths.dirs.data.join("services/postgres"));
    assert_eq!(f.paths.php_fpm_config, f.paths.dirs.state.join("php-fpm.conf"));
    assert_eq!(
        f.paths.php_fpm_socket.as_ref().unwrap(),
        &f.paths.runtime.as_ref().unwrap().join("php-fpm.sock")
    );
    assert_eq!(f.paths.firefox_profiles.len(), 3);
    assert!(
        f.paths
            .v1_sudoers
            .starts_with(f.paths.fake_root.as_ref().unwrap())
    );
    let xdg = f.paths.home.join("custom-config");
    let paths = f.paths.clone().with_user_config_root(&xdg).unwrap();
    assert_eq!(paths.user_unit, xdg.join("systemd/user/lantern.service"));
    assert_eq!(paths.autostart, xdg.join("autostart/lantern-tray.desktop"));
    assert!(paths.with_user_config_root(Path::new("../bad")).is_err());
}

#[test]
fn constrained_root_accepts_local_tls_and_rejects_external_dns_in_memory() {
    use rustls::{
        ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection,
        pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName},
    };
    use std::{io::Cursor, sync::Arc};
    let f = Fixture::new();
    lantern_system::ca::ensure_root(&f.paths, "glow").unwrap();
    let root_pem = fs::read_to_string(&f.paths.root_cert).unwrap();
    let root_key = rcgen::KeyPair::from_pem(&fs::read_to_string(&f.paths.root_key).unwrap()).unwrap();
    let issuer = rcgen::Issuer::from_ca_cert_pem(&root_pem, root_key).unwrap();
    let root_der = lantern_system::steps::nss_trust::certificate_der(root_pem.as_bytes()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(root_der)).unwrap();
    let client_config = Arc::new(
        ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    fn handshake(
        name: &str,
        issuer: &rcgen::Issuer<'_, rcgen::KeyPair>,
        provider: Arc<rustls::crypto::CryptoProvider>,
        client_config: Arc<ClientConfig>,
    ) -> Result<(), String> {
        let mut params = rcgen::CertificateParams::new(vec![name.to_string()]).unwrap();
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::days(1);
        params.not_after = now + time::Duration::days(7);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, issuer).unwrap();
        let server_config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
        let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();
        let mut client =
            ClientConnection::new(client_config, ServerName::try_from(name.to_owned()).unwrap()).unwrap();
        for _ in 0..20 {
            if client.wants_write() {
                let mut bytes = Vec::new();
                client.write_tls(&mut bytes).unwrap();
                server
                    .read_tls(&mut Cursor::new(bytes))
                    .map_err(|e| e.to_string())?;
                server.process_new_packets().map_err(|e| e.to_string())?;
            }
            if server.wants_write() {
                let mut bytes = Vec::new();
                server.write_tls(&mut bytes).unwrap();
                client
                    .read_tls(&mut Cursor::new(bytes))
                    .map_err(|e| e.to_string())?;
                client.process_new_packets().map_err(|e| e.to_string())?;
            }
            if !client.is_handshaking() && !server.is_handshaking() {
                return Ok(());
            }
        }
        Err("handshake did not complete".into())
    }
    assert!(
        handshake(
            "feature.app.glow",
            &issuer,
            provider.clone(),
            client_config.clone()
        )
        .is_ok()
    );
    let outside = handshake("outside.example", &issuer, provider, client_config).unwrap_err();
    assert!(
        outside.to_lowercase().contains("nameconstraint"),
        "unexpected error: {outside}"
    );
}

#[test]
fn doctor_session_classifies_manager_state_and_preserves_command_details() {
    use lantern_core::api::CheckStatus;
    use lantern_system::doctor::{DoctorCtx, run};
    struct ManagerRunner<'a> {
        base: RecordingRunner,
        state: &'a str,
        success: bool,
        environment_error: Option<&'a str>,
    }
    impl CommandRunner for ManagerRunner<'_> {
        fn run(&self, spec: &CommandSpec) -> io::Result<CommandOutput> {
            let args: Vec<_> = spec.args.iter().map(|a| a.to_string_lossy()).collect();
            if args.iter().any(|a| a == "show-environment")
                && let Some(error) = self.environment_error
            {
                return Ok(CommandOutput {
                    success: false,
                    stdout: Vec::new(),
                    stderr: error.as_bytes().to_vec(),
                });
            }
            if args.iter().any(|a| a == "is-system-running") {
                return Ok(CommandOutput {
                    success: self.success,
                    stdout: self.state.as_bytes().to_vec(),
                    stderr: Vec::new(),
                });
            }
            self.base.run(spec)
        }
    }
    let f = Fixture::new();
    let settings = lantern_core::settings::Settings::default();
    for (state, success, error, expected) in [
        ("running\n", true, None, CheckStatus::Ok),
        ("degraded\n", false, None, CheckStatus::Warn),
        ("offline\n", false, None, CheckStatus::Fail),
        ("", false, None, CheckStatus::Fail),
        (
            "running\n",
            true,
            Some("cannot connect to bus"),
            CheckStatus::Fail,
        ),
    ] {
        let runner = ManagerRunner {
            base: RecordingRunner::new(&f),
            state,
            success,
            environment_error: error,
        };
        let ctx = DoctorCtx {
            paths: &f.paths,
            settings: &settings,
            runner: &runner,
            network: &OfflineNetwork,
            caddy: &f.paths.bundled_caddy,
            certutil: None,
            search_path: vec![],
        };
        let checks = run(&ctx);
        let check = checks.iter().find(|c| c.id == "session.systemd").unwrap();
        assert_eq!(check.status, expected, "{state:?} {error:?}");
        assert!(!check.detail.is_empty());
        if state.trim() == "degraded" {
            assert!(check.detail.contains("systemctl --user --failed"));
            assert_eq!(check.fix.as_ref().unwrap().command, "systemctl --user --failed");
        } else if state.trim() == "offline" {
            assert!(check.detail.contains("offline"));
        }
        if let Some(error) = error {
            assert!(check.detail.contains(error));
        }
        let unit = checks.iter().find(|c| c.id == "session.unit").unwrap();
        assert_eq!(unit.status, CheckStatus::Fail);
        assert!(unit.detail.contains("failed without output"));
    }
}

#[test]
fn init_sysctl_and_doctor_port_access_cover_defaults_caps_and_dev_ports() {
    use lantern_core::api::CheckStatus;
    use lantern_system::{
        doctor::{DoctorCtx, run},
        init::{SystemInitCtx, run_system_steps},
    };
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    let settings = lantern_core::settings::Settings::default();
    f.write(&f.paths.sysctl_live, b"1024\n");
    f.write(&f.paths.bundled_caddy, b"fake executable");
    let system = SystemInitCtx {
        paths: &f.paths,
        settings: &settings,
        runner: &runner,
        uid: 1000,
        port_strategy: PortStrategy::Sysctl,
        from_package: false,
        migrate_v1: false,
        allow_fake_cleanup: false,
        system_trust_step: None,
    };
    let outcomes = run_system_steps(&system);
    assert_eq!(
        outcomes.iter().find(|o| o.id == "ports").unwrap().state,
        OutcomeState::Changed
    );
    assert_eq!(
        fs::read_to_string(&f.paths.sysctl_conf).unwrap(),
        "# Managed by Lantern\nnet.ipv4.ip_unprivileged_port_start=80\n"
    );
    assert_eq!(fs::read_to_string(&f.paths.sysctl_live).unwrap(), "80");
    assert!(
        runner
            .state
            .borrow()
            .commands
            .iter()
            .any(|c| c.program == Path::new("/usr/sbin/sysctl")
                && c.args == ["-w", "net.ipv4.ip_unprivileged_port_start=80"]
                && c.fake_root)
    );

    let check = |settings: &lantern_core::settings::Settings| {
        let ctx = DoctorCtx {
            paths: &f.paths,
            settings,
            runner: &runner,
            network: &OfflineNetwork,
            caddy: &f.paths.bundled_caddy,
            certutil: None,
            search_path: vec![],
        };
        run(&ctx).into_iter().find(|c| c.id == "ports.bind").unwrap()
    };
    assert_eq!(check(&settings).status, CheckStatus::Ok);
    f.write(&f.paths.sysctl_live, b"1024");
    let failed = check(&settings);
    assert_eq!(failed.status, CheckStatus::Fail);
    assert_eq!(failed.fix.unwrap().command, "sudo lantern init --system-steps");
    runner.state.borrow_mut().cap = true;
    assert_eq!(check(&settings).status, CheckStatus::Ok);
    assert!(
        runner
            .state
            .borrow()
            .commands
            .iter()
            .any(|c| c.program == Path::new("/usr/sbin/getcap")
                && c.args == [f.paths.bundled_caddy.as_os_str()])
    );
    runner.state.borrow_mut().cap = false;
    let mut dev_settings = settings.clone();
    dev_settings.proxy.http_port = 8080;
    dev_settings.proxy.https_port = 8443;
    assert_eq!(check(&dev_settings).status, CheckStatus::Ok);
    let dev = SystemInitCtx {
        settings: &dev_settings,
        ..system
    };
    assert_eq!(
        run_system_steps(&dev)
            .iter()
            .find(|o| o.id == "ports")
            .unwrap()
            .state,
        OutcomeState::AlreadyDone
    );
    assert_eq!(fs::read_to_string(&f.paths.sysctl_live).unwrap(), "1024");
}

#[test]
fn init_elevation_uses_selected_strategy_and_rechecks_fake_root() {
    use lantern_system::init::{InitPlan, StepReport, SystemInitCtx, complete_init, pkexec_invocation};
    struct ElevationRunner {
        base: RecordingRunner,
        fail: bool,
    }
    impl CommandRunner for ElevationRunner {
        fn run(&self, spec: &CommandSpec) -> io::Result<CommandOutput> {
            if spec.program == Path::new("/usr/bin/pkexec") {
                assert!(spec.fake_root);
                assert!(spec.args.iter().any(|arg| arg == "--ports=setcap"));
                if self.fail {
                    return Ok(CommandOutput {
                        success: false,
                        stdout: Vec::new(),
                        stderr: b"cancelled".to_vec(),
                    });
                }
                self.base.state.borrow_mut().cap = true;
                return Ok(CommandOutput {
                    success: true,
                    stdout: b"{}".to_vec(),
                    stderr: Vec::new(),
                });
            }
            self.base.run(spec)
        }
    }
    let f = Fixture::new();
    f.write(&f.paths.sysctl_live, b"1024");
    let exe = f.paths.home.join("lantern");
    f.write(&exe, b"fake");
    let mut settings = lantern_core::settings::Settings::default();
    settings.domains.tld = "localhost".into();
    let runner = ElevationRunner {
        base: RecordingRunner::new(&f),
        fail: false,
    };
    let trust = FakeStep {
        id: "system_trust",
        root: true,
        done: std::cell::Cell::new(true),
    };
    let system = SystemInitCtx {
        paths: &f.paths,
        settings: &settings,
        runner: &runner,
        uid: 1000,
        port_strategy: PortStrategy::Setcap,
        from_package: false,
        migrate_v1: false,
        allow_fake_cleanup: false,
        system_trust_step: Some(&trust),
    };
    let mut cmd = pkexec_invocation(&exe, 1000).unwrap();
    cmd.args.push("--ports=setcap".into());
    let plan = InitPlan {
        user_outcomes: vec![],
        system_steps: vec![StepReport {
            id: "ports",
            state: StepState::Pending("needs access".into()),
            needs_root: true,
        }],
        pkexec: Some(cmd),
    };
    let result = complete_init(&system, &plan).unwrap();
    assert!(matches!(
        result.iter().find(|s| s.id == "ports").unwrap().state,
        StepState::Done(_)
    ));
    let failure = ElevationRunner {
        base: RecordingRunner::new(&f),
        fail: true,
    };
    let err = complete_init(
        &SystemInitCtx {
            runner: &failure,
            ..system
        },
        &plan,
    )
    .unwrap_err();
    assert!(
        err.contains("cancelled")
            && err.contains("sudo lantern init --system-steps --uid 1000 --ports=setcap"),
        "{err}"
    );
}

#[test]
fn init_persists_the_selected_policy_even_when_live_ports_are_already_open() {
    let f = Fixture::new();
    let runner = RecordingRunner::new(&f);
    f.write(&f.paths.sysctl_live, b"0");
    f.write(&f.paths.bundled_caddy, b"binary");
    let step = PortsStep {
        paths: &f.paths,
        runner: &runner,
        http_port: 80,
        https_port: 443,
        caddy: &f.paths.bundled_caddy,
        strategy: PortStrategy::Sysctl,
        from_package: false,
    };
    assert!(matches!(step.check(), StepState::Done(_)));
    assert_eq!(step.apply().state, OutcomeState::Changed);
    assert!(f.paths.sysctl_conf.exists());
    assert_eq!(fs::read_to_string(&f.paths.sysctl_live).unwrap(), "80");
    let cap = PortsStep {
        strategy: PortStrategy::Setcap,
        ..step
    };
    assert_eq!(cap.apply().state, OutcomeState::Changed);
    assert!(runner.state.borrow().cap);
}
