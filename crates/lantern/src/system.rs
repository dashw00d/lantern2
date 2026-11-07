//! Offline CLI adapter for system initialization and diagnostics.
use lantern_cli::{SystemActions, SystemCommand};
use lantern_core::{
    dirs::Dirs,
    settings::{self, ProcessEnv},
};
use lantern_system::{
    command::{ProcessRunner, find_program},
    doctor::{self, DoctorCtx, LocalNetworkProbe},
    init::{self, InitCtx, SystemInitCtx},
    paths::LanternPaths,
    steps::sysctl::PortStrategy,
};
use serde_json::{Value, json};
use std::path::PathBuf;

pub struct System;
impl SystemActions for System {
    fn execute(&self, command: &SystemCommand) -> Result<Value, String> {
        if let SystemCommand::InitSystem { uid } = command {
            let user = init::ResolvedUser::from_uid(*uid)?;
            let runner = ProcessRunner {
                fake_root: user.paths.fake_root.is_some(),
            };
            let context = SystemInitCtx {
                paths: &user.paths,
                settings: &user.settings,
                runner: &runner,
                uid: *uid,
                port_strategy: PortStrategy::Sysctl,
                from_package: false,
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
        let runner = ProcessRunner {
            fake_root: paths.fake_root.is_some(),
        };
        let network = LocalNetworkProbe {
            paths: &paths,
            settings: &settings,
            runner: &runner,
        };
        let caddy = if settings.caddy.binary.is_empty() {
            paths.bundled_caddy.clone()
        } else {
            PathBuf::from(&settings.caddy.binary)
        };
        let certutil = find_program(&["/usr/bin/certutil", "/usr/local/bin/certutil"]);
        match command {
            SystemCommand::Doctor { fix } => {
                if *fix {
                    return Err("doctor --fix is not implemented; use the reported fix commands".into());
                }
                let checks = doctor::run(&DoctorCtx {
                    paths: &paths,
                    settings: &settings,
                    runner: &runner,
                    network: &network,
                    caddy: &caddy,
                    certutil: certutil.as_deref(),
                    search_path: std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                        .collect(),
                });
                let ok = checks.iter().all(|c| {
                    matches!(
                        c.status,
                        lantern_core::api::CheckStatus::Ok | lantern_core::api::CheckStatus::Skipped
                    )
                });
                Ok(json!({"ok":ok, "checked_at":lantern_core::Timestamp::now(), "checks":checks}))
            }
            SystemCommand::Init { yes } => {
                if !yes {
                    return Err("init requires --yes to apply the user setup steps".into());
                }
                let executable = std::env::current_exe().map_err(|e| e.to_string())?;
                let user = InitCtx {
                    paths: &paths,
                    runner: &runner,
                    tld: &settings.domains.tld,
                    executable: &executable,
                    certutil: certutil.as_deref(),
                    tray_autostart: false,
                    ca_step: None,
                };
                let system = SystemInitCtx {
                    paths: &paths,
                    settings: &settings,
                    runner: &runner,
                    uid: rustix::process::getuid().as_raw(),
                    port_strategy: PortStrategy::Sysctl,
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
                let root: Vec<_> = plan
                    .system_steps
                    .iter()
                    .map(|s| json!({"id":s.id, "state":format!("{:?}",s.state), "needs_root":s.needs_root}))
                    .collect();
                let pkexec = plan.pkexec.map(|command| json!({"program":command.program, "args":command.args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>() }));
                Ok(json!({"user_steps":outcomes,"system_steps":root,"pkexec":pkexec}))
            }
            SystemCommand::InitSystem { .. } => unreachable!("handled before loading caller environment"),
            SystemCommand::Uninstall { .. } => Err("uninstall is not implemented by lantern-system".into()),
        }
    }
}
