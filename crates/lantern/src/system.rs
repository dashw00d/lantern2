//! Offline CLI adapter for system initialization and diagnostics.
use lantern_cli::{SystemActions, SystemCommand, args::PortMode};
use lantern_core::{
    api::{Check, CheckStatus, Fix},
    dirs::Dirs,
    settings::{self, ProcessEnv},
};
use lantern_system::{
    command::{CommandOutput, CommandRunner, CommandSpec, ProcessRunner, find_program},
    doctor::{self, DoctorCtx, LocalNetworkProbe, NetworkProbe, Probe},
    init::{self, InitCtx, SystemInitCtx},
    paths::LanternPaths,
    step::StepState,
    steps::sysctl::PortStrategy,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn caddy_selection(configured: &str, data: &Path, search_path: &str) -> (PathBuf, Option<Check>) {
    match lantern_daemon::app::network::resolve_caddy(configured, data, search_path) {
        Some(path) => (path, None),
        None => (
            data.join("bin/caddy"),
            Some(Check {
                id: "caddy_missing".into(),
                title: "Caddy proxy".into(),
                status: CheckStatus::Warn,
                detail: "Caddy executable not found; routes are retained until it is installed.".into(),
                fix: Some(Fix {
                    description: "Install Caddy 2.8 or newer, or provide Lantern's bundled binary.".into(),
                    command: "lantern init".into(),
                    needs_root: false,
                }),
            }),
        ),
    }
}

fn strategy(mode: PortMode) -> PortStrategy {
    match mode {
        PortMode::Sysctl => PortStrategy::Sysctl,
        PortMode::Setcap => PortStrategy::Setcap,
    }
}
fn validate_setcap_selection(mode: PortMode, configured: &str) -> Result<(), String> {
    if mode == PortMode::Setcap && configured != "/usr/lib/lantern/caddy" {
        return Err("--ports=setcap requires [caddy].binary = \"/usr/lib/lantern/caddy\" so the daemon selects the capability-bearing executable".into());
    }
    Ok(())
}
fn step_report(step: &init::StepReport) -> Value {
    let (state, detail) = match &step.state {
        StepState::Done(detail) => ("Done", detail),
        StepState::Pending(detail) => ("Pending", detail),
        StepState::Skipped(detail) => ("Skipped", detail),
        StepState::Failed(detail) => ("Failed", detail),
    };
    json!({"id":step.id, "state":state, "detail":detail, "needs_root":step.needs_root})
}

// A strictly isolated fixture for exercising the actual CLI against a fake root.
struct OfflineFixture;
impl CommandRunner for OfflineFixture {
    fn run(&self, spec: &CommandSpec) -> std::io::Result<CommandOutput> {
        let args: Vec<_> = spec.args.iter().map(|a| a.to_string_lossy()).collect();
        if spec.program == Path::new("/usr/bin/systemctl") && args == ["--user", "show-environment"] {
            return Ok(CommandOutput {
                success: true,
                stdout: b"FIXTURE=1\n".to_vec(),
                stderr: vec![],
            });
        }
        if spec.program == Path::new("/usr/bin/systemctl") && args == ["--user", "is-system-running"] {
            return Ok(CommandOutput {
                success: false,
                stdout: b"degraded\n".to_vec(),
                stderr: vec![],
            });
        }
        if spec.program == Path::new("/usr/bin/systemctl")
            && args == ["--user", "is-enabled", "lantern.service"]
        {
            return Ok(CommandOutput {
                success: false,
                stdout: b"disabled\n".to_vec(),
                stderr: vec![],
            });
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "offline fixture: command unavailable",
        ))
    }
}
impl NetworkProbe for OfflineFixture {
    fn check(&self, _: Probe) -> Result<String, String> {
        Err("offline fixture: network probe disabled".into())
    }
}

pub struct System;
impl SystemActions for System {
    fn execute(&self, command: &SystemCommand) -> Result<Value, String> {
        if let SystemCommand::InitSystem {
            uid,
            ports,
            from_package,
        } = command
        {
            let uid = match uid {
                Some(uid) => *uid,
                None if *from_package => 0,
                None => ["PKEXEC_UID", "SUDO_UID"]
                    .iter()
                    .find_map(|key| {
                        std::env::var(key)
                            .ok()
                            .and_then(|s| s.parse::<u32>().ok())
                            .filter(|uid| *uid != 0)
                    })
                    .ok_or("--uid is required unless invoked through sudo or pkexec")?,
            };
            let (paths, settings) = if uid == 0 && *from_package {
                let root = PathBuf::from("/");
                let dirs = Dirs {
                    config: root.join("nonexistent/lantern/config"),
                    state: root.join("nonexistent/lantern/state"),
                    data: root.join("nonexistent/lantern/data"),
                };
                (
                    LanternPaths::new(
                        dirs,
                        root.clone(),
                        None,
                        root.join("nonexistent/lantern/cache"),
                        None,
                    )?,
                    lantern_core::settings::Settings::default(),
                )
            } else {
                let user = init::ResolvedUser::from_uid(uid)?;
                (user.paths, user.settings)
            };
            validate_setcap_selection(*ports, &settings.caddy.binary)?;
            let runner = ProcessRunner {
                fake_root: paths.fake_root.is_some(),
            };
            let context = SystemInitCtx {
                paths: &paths,
                settings: &settings,
                runner: &runner,
                uid,
                port_strategy: strategy(*ports),
                from_package: *from_package,
                migrate_v1: false,
                allow_fake_cleanup: false,
                system_trust_step: None,
            };
            let outcomes = init::run_system_steps(&context);
            return Ok(
                json!({"system_steps":outcomes.iter().map(|o| json!({"id":o.id,"state":format!("{:?}",o.state),"detail":o.detail})).collect::<Vec<_>>() }),
            );
        }
        let env = ProcessEnv;
        let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is required")?);
        let dirs = Dirs::from_env(&env)?;
        let settings = settings::load(&dirs.config.join("config.toml"), &env, &home, None)
            .map_err(|e| e.to_string())?
            .settings;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        let cache = std::env::var_os("LANTERN_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache/lantern"));
        let paths = LanternPaths::new(dirs, home, runtime, cache, None)?;
        let offline = std::env::var_os("LANTERN_TEST_OFFLINE_DOCTOR").is_some_and(|v| v == "1")
            && matches!(command, SystemCommand::Doctor { .. });
        if offline && paths.fake_root.is_none() {
            return Err("LANTERN_TEST_OFFLINE_DOCTOR requires LANTERN_FAKE_ROOT".into());
        }
        let real_runner = ProcessRunner {
            fake_root: paths.fake_root.is_some(),
        };
        let fixture = OfflineFixture;
        let runner: &dyn CommandRunner = if offline { &fixture } else { &real_runner };
        let local_network = LocalNetworkProbe {
            paths: &paths,
            settings: &settings,
            runner,
        };
        let network: &dyn NetworkProbe = if offline { &fixture } else { &local_network };
        let search_path = if offline {
            String::new()
        } else {
            std::env::var_os("PATH")
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        let (caddy, missing_caddy) = caddy_selection(&settings.caddy.binary, &paths.dirs.data, &search_path);
        let certutil = if offline {
            None
        } else {
            find_program(&["/usr/bin/certutil", "/usr/local/bin/certutil"])
        };
        match command {
            SystemCommand::Doctor { fix } => {
                if *fix {
                    return Err("doctor --fix is not implemented; use the reported fix commands".into());
                }
                let mut checks = doctor::run(&DoctorCtx {
                    paths: &paths,
                    settings: &settings,
                    runner,
                    network,
                    caddy: &caddy,
                    certutil: certutil.as_deref(),
                    search_path: std::env::split_paths(&search_path).collect(),
                });
                if let Some(check) = missing_caddy {
                    checks.push(check);
                }
                let ok = checks.iter().all(|c| {
                    matches!(
                        c.status,
                        lantern_core::api::CheckStatus::Ok
                            | lantern_core::api::CheckStatus::Warn
                            | lantern_core::api::CheckStatus::Skipped
                    )
                });
                Ok(json!({"ok":ok, "checked_at":lantern_core::Timestamp::now(), "checks":checks}))
            }
            SystemCommand::Init { ports, .. } => {
                validate_setcap_selection(*ports, &settings.caddy.binary)?;
                if *ports == PortMode::Setcap && caddy != paths.bundled_caddy {
                    return Err(format!(
                        "--ports=setcap requires the selected Caddy to be {}; set [caddy].binary to this root-owned executable",
                        paths.bundled_caddy.display()
                    ));
                }
                let executable = std::env::current_exe().map_err(|e| e.to_string())?;
                let user = InitCtx {
                    paths: &paths,
                    runner,
                    tld: &settings.domains.tld,
                    executable: &executable,
                    certutil: certutil.as_deref(),
                    tray_autostart: false,
                    ca_step: None,
                };
                let system = SystemInitCtx {
                    paths: &paths,
                    settings: &settings,
                    runner,
                    uid: rustix::process::getuid().as_raw(),
                    port_strategy: strategy(*ports),
                    from_package: false,
                    migrate_v1: false,
                    allow_fake_cleanup: false,
                    system_trust_step: None,
                };
                let plan = init::run_init(&user, &system)?;
                let outcomes: Vec<_> = plan
                    .user_outcomes
                    .iter()
                    .map(|o| json!({"id":o.id, "state":format!("{:?}",o.state), "detail":o.detail}))
                    .collect();
                let (root, pending) = match init::complete_init(&system, &plan) {
                    Ok(steps) => (steps.iter().map(step_report).collect::<Vec<_>>(), false),
                    Err(error) => (
                        vec![
                            json!({"id":"system_steps", "state":"Failed", "detail":error, "needs_root":true}),
                        ],
                        true,
                    ),
                };
                let pkexec = plan.pkexec.filter(|_| pending).map(|command| json!({"program":command.program, "args":command.args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>() }));
                Ok(json!({"user_steps":outcomes,"system_steps":root,"pkexec":pkexec}))
            }
            SystemCommand::InitSystem { .. } => unreachable!("handled before loading caller environment"),
            SystemCommand::Uninstall { .. } => Err("uninstall is not implemented by lantern-system".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{caddy_selection, validate_setcap_selection};
    use lantern_cli::args::PortMode;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn setcap_requires_explicit_packaged_caddy_selection() {
        assert!(validate_setcap_selection(PortMode::Sysctl, "").is_ok());
        assert!(validate_setcap_selection(PortMode::Setcap, "").is_err());
        assert!(validate_setcap_selection(PortMode::Setcap, "/home/user/caddy").is_err());
        assert!(validate_setcap_selection(PortMode::Setcap, "/usr/lib/lantern/caddy").is_ok());
    }

    #[test]
    fn wire_caddy_selection_reports_missing_and_finds_bundled_binary() {
        let root = std::env::temp_dir().join(format!("lantern-test-wire-doctor-{}", std::process::id()));
        fs::create_dir_all(root.join("bin")).unwrap();
        let (path, check) = caddy_selection("", &root, "");
        assert_eq!(path, root.join("bin/caddy"));
        let check = check.unwrap();
        assert_eq!(check.id, "caddy_missing");
        assert!(check.fix.unwrap().description.contains("Install Caddy"));
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(caddy_selection("", &root, "").1.is_none());
        fs::remove_dir_all(root).unwrap();
    }
}
