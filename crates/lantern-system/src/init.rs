//! Synchronous init composition and privilege-boundary checks.
use crate::{
    command::{CommandRunner, CommandSpec},
    paths::LanternPaths,
    step::{OutcomeState, Step, StepOutcome, StepState},
    steps::{
        dns_delegate::DnsDelegateStep,
        nss_trust::NssTrustStep,
        sysctl::{PortStrategy, PortsStep},
        user_units::UserUnitsStep,
        v1_migrate::V1MigrateStep,
    },
};
use lantern_core::settings::Settings;
use std::{ffi::OsString, fs, net::SocketAddr, path::Path};

/// Inputs for user-owned initialization.
pub struct InitCtx<'a> {
    /// User paths.
    pub paths: &'a LanternPaths,
    /// Command execution boundary.
    pub runner: &'a dyn CommandRunner,
    /// Configured local TLD.
    pub tld: &'a str,
    /// Lantern executable.
    pub executable: &'a Path,
    /// Absolute certutil path, when installed.
    pub certutil: Option<&'a Path>,
    /// Opt in to tray autostart.
    pub tray_autostart: bool,
    /// Optional CA step override, primarily for tests; defaults to the built-in CA.
    pub ca_step: Option<&'a dyn Step>,
}
impl std::fmt::Debug for InitCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InitCtx")
            .field("executable", &self.executable)
            .finish_non_exhaustive()
    }
}
/// Inputs for root-owned initialization. The caller must load settings from the target user.
pub struct SystemInitCtx<'a> {
    /// Target user's resolved paths.
    pub paths: &'a LanternPaths,
    /// Validated settings loaded for the target user.
    pub settings: &'a Settings,
    /// Command execution boundary.
    pub runner: &'a dyn CommandRunner,
    /// Numeric non-root target user; zero denotes package setup without user context.
    pub uid: u32,
    /// Selected privileged-port strategy.
    pub port_strategy: PortStrategy,
    /// Preserve existing files during package setup and omit trust.
    pub from_package: bool,
    /// Include the v1 detection/cleanup step.
    pub migrate_v1: bool,
    /// Allow guarded v1 cleanup inside a fake root only.
    pub allow_fake_cleanup: bool,
    /// Optional system-trust step override; defaults to the built-in trust step.
    pub system_trust_step: Option<&'a dyn Step>,
}
impl std::fmt::Debug for SystemInitCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemInitCtx")
            .field("uid", &self.uid)
            .field("port_strategy", &self.port_strategy)
            .finish_non_exhaustive()
    }
}
fn effective_uid() -> u32 {
    nix::unistd::Uid::effective().as_raw()
}

/// User identity, paths and settings resolved independently of root's environment.
#[derive(Debug, Clone)]
pub struct ResolvedUser {
    /// The passwd-resolved numeric user.
    pub uid: u32,
    /// Default user paths derived from the passwd home.
    pub paths: LanternPaths,
    /// Settings loaded from the user's config without environment overrides.
    pub settings: Settings,
}
impl ResolvedUser {
    /// Resolve a non-root user through the system account database and load their config.
    pub fn from_uid(uid: u32) -> Result<Self, String> {
        if uid == 0 {
            return Err("target uid must not be root".into());
        }
        let user = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
            .map_err(|e| e.to_string())?
            .ok_or("target uid not found in account database")?;
        let home = user.dir;
        let dirs = lantern_core::dirs::Dirs {
            config: home.join(".config/lantern"),
            state: home.join(".local/state/lantern"),
            data: home.join(".local/share/lantern"),
        };
        let env = std::collections::BTreeMap::<String, String>::new();
        let settings = lantern_core::settings::load(&dirs.config.join("config.toml"), &env, &home, None)
            .map_err(|e| e.to_string())?
            .settings;
        let cache = home.join(".cache/lantern");
        let paths = LanternPaths::new(dirs, home, None, cache, None)?;
        Ok(Self { uid, paths, settings })
    }
}
fn validate_settings(ctx: &SystemInitCtx<'_>) -> Result<SocketAddr, String> {
    if ctx.uid == 0 && !ctx.from_package {
        return Err("target uid must not be root".into());
    }
    if ctx.uid == 0 && ctx.from_package {
        let defaults = Settings::default();
        if ctx.settings.domains.tld != defaults.domains.tld
            || ctx.settings.dns.listen != defaults.dns.listen
            || ctx.settings.proxy.http_port != defaults.proxy.http_port
            || ctx.settings.proxy.https_port != defaults.proxy.https_port
        {
            return Err("package setup without a user requires default settings".into());
        }
    }
    if !crate::steps::valid_tld(&ctx.settings.domains.tld) {
        return Err("invalid TLD".into());
    }
    if ctx.settings.proxy.http_port == 0 || ctx.settings.proxy.https_port == 0 {
        return Err("ports must be nonzero".into());
    }
    let listen: SocketAddr = ctx
        .settings
        .dns
        .listen
        .parse()
        .map_err(|_| "invalid DNS listener")?;
    if !listen.ip().is_loopback() || listen.port() == 0 {
        return Err("DNS listener must be loopback and nonzero".into());
    }
    Ok(listen)
}
fn validate_system(ctx: &SystemInitCtx<'_>) -> Result<SocketAddr, String> {
    let listen = validate_settings(ctx)?;
    if ctx.paths.fake_root.is_none() {
        if effective_uid() != 0 {
            return Err("system steps require root".into());
        }
        if ctx.from_package && ctx.uid == 0 {
            return Ok(listen);
        }
        let resolved = ResolvedUser::from_uid(ctx.uid)?;
        if resolved.paths.home != ctx.paths.home
            || resolved.paths.dirs.config != ctx.paths.dirs.config
            || resolved.paths.dirs.data != ctx.paths.dirs.data
        {
            return Err("root steps require paths resolved from the target user's account, not the caller environment".into());
        }
        if resolved.settings.domains.tld != ctx.settings.domains.tld
            || resolved.settings.dns.listen != ctx.settings.dns.listen
            || resolved.settings.proxy.http_port != ctx.settings.proxy.http_port
            || resolved.settings.proxy.https_port != ctx.settings.proxy.https_port
        {
            return Err("root steps require settings loaded from the target user's config".into());
        }
    }

    Ok(listen)
}
/// Run user steps, returning a separate result for each action.
pub fn run_user_steps(ctx: &InitCtx<'_>) -> Vec<StepOutcome> {
    if ctx.paths.fake_root.is_none() && effective_uid() == 0 {
        return vec![StepOutcome::new(
            "user_steps",
            OutcomeState::Failed,
            "run user steps as your user, not root",
        )];
    }

    let mut outcomes = Vec::new();
    let dirs = [
        &ctx.paths.dirs.config,
        &ctx.paths.dirs.state,
        &ctx.paths.dirs.data,
        &ctx.paths.cache,
    ];
    let missing = dirs.iter().any(|dir| !dir.is_dir());
    let directory_result = dirs.into_iter().try_for_each(|dir| {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    });
    if let Err(e) = directory_result {
        return vec![StepOutcome::new(
            "directories",
            OutcomeState::Failed,
            e.to_string(),
        )];
    }
    outcomes.push(StepOutcome::new(
        "directories",
        if missing {
            OutcomeState::Changed
        } else {
            OutcomeState::AlreadyDone
        },
        "user directories exist",
    ));
    let ca_outcome = match ctx.ca_step {
        Some(step) if !step.needs_root() => step.apply(),
        Some(_) => StepOutcome::new("ca", OutcomeState::Failed, "CA step must not require root"),
        None => crate::ca::CaStep {
            paths: ctx.paths,
            tld: ctx.tld,
            runner: Some(ctx.runner),
        }
        .apply(),
    };
    let ca_failed = ca_outcome.state == OutcomeState::Failed;
    outcomes.push(ca_outcome);
    if ca_failed {
        outcomes.push(StepOutcome::new(
            "nss_trust",
            OutcomeState::Skipped,
            "CA step failed",
        ));
        outcomes.push(StepOutcome::new(
            "user_units",
            OutcomeState::Skipped,
            "CA step failed",
        ));
        return outcomes;
    }
    outcomes.push(
        NssTrustStep {
            paths: ctx.paths,
            runner: ctx.runner,
            tld: ctx.tld,
            certutil: ctx.certutil,
        }
        .apply(),
    );
    outcomes.push(
        UserUnitsStep {
            paths: ctx.paths,
            runner: ctx.runner,
            executable: ctx.executable,
            tray_autostart: ctx.tray_autostart,
        }
        .apply(),
    );
    outcomes
}
/// Run root steps after validating identity, paths and untrusted settings.
pub fn run_system_steps(ctx: &SystemInitCtx<'_>) -> Vec<StepOutcome> {
    let listen = match validate_system(ctx) {
        Ok(value) => value,
        Err(e) => return vec![StepOutcome::new("system_steps", OutcomeState::Failed, e)],
    };
    let mut outcomes = Vec::new();
    outcomes.push(
        PortsStep {
            paths: ctx.paths,
            runner: ctx.runner,
            http_port: ctx.settings.proxy.http_port,
            https_port: ctx.settings.proxy.https_port,
            caddy: &ctx.paths.bundled_caddy,
            strategy: ctx.port_strategy,
            from_package: ctx.from_package,
        }
        .apply(),
    );
    outcomes.push(
        DnsDelegateStep {
            paths: ctx.paths,
            runner: ctx.runner,
            tld: &ctx.settings.domains.tld,
            listen,
            from_package: ctx.from_package,
        }
        .apply(),
    );
    outcomes.push(if ctx.from_package {
        StepOutcome::new(
            "system_trust",
            OutcomeState::Skipped,
            "package setup has no user CA",
        )
    } else {
        match ctx.system_trust_step {
            Some(step) if step.needs_root() => step.apply(),
            Some(_) => StepOutcome::new(
                "system_trust",
                OutcomeState::Failed,
                "system trust step must require root",
            ),
            None => crate::steps::system_trust::SystemTrustStep {
                paths: ctx.paths,
                runner: ctx.runner,
                tld: &ctx.settings.domains.tld,
            }
            .apply(),
        }
    });
    if ctx.migrate_v1 {
        outcomes.push(
            V1MigrateStep {
                paths: ctx.paths,
                runner: ctx.runner,
                allow_fake_cleanup: ctx.allow_fake_cleanup,
            }
            .apply(),
        );
    }
    outcomes
}
/// Build, but never execute, a pkexec command for the canonical current binary.
pub fn pkexec_invocation(executable: &Path, uid: u32) -> Result<CommandSpec, String> {
    if uid == 0 {
        return Err("target uid must not be root".into());
    }
    let executable = fs::canonicalize(executable).map_err(|e| e.to_string())?;
    if !executable.is_absolute() || !executable.is_file() {
        return Err("Lantern executable must be an absolute regular file".into());
    }
    Ok(CommandSpec::new(
        "/usr/bin/pkexec",
        [
            executable.into_os_string(),
            OsString::from("init"),
            OsString::from("--system-steps"),
            OsString::from("--uid"),
            OsString::from(uid.to_string()),
        ],
        true,
    ))
}

/// The read-only state of a root step before elevation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepReport {
    /// Stable step identifier.
    pub id: &'static str,
    /// Current state.
    pub state: StepState,
    /// Whether applying it requires root.
    pub needs_root: bool,
}
/// Result of user initialization and the remaining root-step plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitPlan {
    /// User-step results.
    pub user_outcomes: Vec<StepOutcome>,
    /// Read-only root-step observations.
    pub system_steps: Vec<StepReport>,
    /// Proposed pkexec invocation, only when actionable root work remains.
    pub pkexec: Option<CommandSpec>,
}
/// Inspect the root steps without needing root or changing state.
pub fn plan_system_steps(ctx: &SystemInitCtx<'_>) -> Result<Vec<StepReport>, String> {
    let listen = validate_settings(ctx)?;
    let ports = PortsStep {
        paths: ctx.paths,
        runner: ctx.runner,
        http_port: ctx.settings.proxy.http_port,
        https_port: ctx.settings.proxy.https_port,
        caddy: &ctx.paths.bundled_caddy,
        strategy: ctx.port_strategy,
        from_package: ctx.from_package,
    };
    let dns = DnsDelegateStep {
        paths: ctx.paths,
        runner: ctx.runner,
        tld: &ctx.settings.domains.tld,
        listen,
        from_package: ctx.from_package,
    };
    let report = |step: &dyn Step| StepReport {
        id: step.id(),
        state: step.check(),
        needs_root: step.needs_root(),
    };
    let mut steps = vec![
        StepReport {
            id: ports.id(),
            state: ports.configuration_check(),
            needs_root: ports.needs_root(),
        },
        report(&dns),
    ];
    if ctx.from_package {
        steps.push(StepReport {
            id: "system_trust",
            state: StepState::Skipped("package setup has no user CA".into()),
            needs_root: true,
        });
    } else {
        steps.push(match ctx.system_trust_step {
            Some(step) if step.needs_root() => report(step),
            Some(_) => StepReport {
                id: "system_trust",
                state: StepState::Failed("system trust step must require root".into()),
                needs_root: true,
            },
            None => report(&crate::steps::system_trust::SystemTrustStep {
                paths: ctx.paths,
                runner: ctx.runner,
                tld: &ctx.settings.domains.tld,
            }),
        });
    }
    if ctx.migrate_v1 {
        steps.push(report(&V1MigrateStep {
            paths: ctx.paths,
            runner: ctx.runner,
            allow_fake_cleanup: ctx.allow_fake_cleanup,
        }));
    }
    Ok(steps)
}
/// Run user steps and construct, but do not execute, an elevation plan when needed.
pub fn run_init(user: &InitCtx<'_>, system: &SystemInitCtx<'_>) -> Result<InitPlan, String> {
    if user.tld != system.settings.domains.tld {
        return Err("user and system steps must use the same TLD".into());
    }
    if user.paths.home != system.paths.home
        || user.paths.root_cert != system.paths.root_cert
        || user.paths.fake_root != system.paths.fake_root
    {
        return Err("user and system steps must target the same user and root".into());
    }
    let user_outcomes = run_user_steps(user);
    let system_steps = plan_system_steps(system)?;
    let any_failed = user_outcomes.iter().any(|o| o.state == OutcomeState::Failed)
        || system_steps
            .iter()
            .any(|s| matches!(s.state, StepState::Failed(_)));
    let pending = system_steps
        .iter()
        .any(|s| matches!(s.state, StepState::Pending(_)));
    let pkexec = if !any_failed && pending {
        let mut command = pkexec_invocation(user.executable, system.uid)?;
        command.args.push("--json".into());
        if system.port_strategy == PortStrategy::Setcap {
            command.args.push("--ports=setcap".into());
        }
        Some(command)
    } else {
        None
    };
    Ok(InitPlan {
        user_outcomes,
        system_steps,
        pkexec,
    })
}

fn root_failure_detail(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Some(text.into());
    };
    if let Some(message) = value
        .get("error")
        .and_then(|v| v.get("message"))
        .and_then(serde_json::Value::as_str)
    {
        return Some(message.into());
    }
    let details = value
        .get("system_steps")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|step| step.get("state").and_then(serde_json::Value::as_str) == Some("Failed"))
        .map(|step| {
            format!(
                "{}: {}",
                step.get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("step"),
                step.get("detail")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("failed")
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    if details.is_empty() { None } else { Some(details) }
}

/// Execute a pending root plan through the injected runner and recheck its results.
pub fn complete_init(system: &SystemInitCtx<'_>, plan: &InitPlan) -> Result<Vec<StepReport>, String> {
    let Some(mut command) = plan.pkexec.clone() else {
        return Ok(plan.system_steps.clone());
    };
    for key in [
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env.insert(key.into(), value);
        }
    }
    command.fake_root = system.paths.fake_root.is_some();
    let fallback = format!(
        "sudo lantern init --system-steps --uid {}{}",
        system.uid,
        if system.port_strategy == PortStrategy::Setcap {
            " --ports=setcap"
        } else {
            ""
        }
    );
    let result = system
        .runner
        .run(&command)
        .map_err(|e| format!("could not run pkexec: {e}; run {fallback}"))?;
    if !result.success {
        let detail = root_failure_detail(&result.stderr)
            .or_else(|| root_failure_detail(&result.stdout))
            .unwrap_or_default();
        return Err(format!(
            "root setup failed{}; run {fallback}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        ));
    }
    let steps = plan_system_steps(system)?;
    if steps
        .iter()
        .any(|step| matches!(step.state, StepState::Pending(_) | StepState::Failed(_)))
    {
        return Err(format!("root setup did not pass its recheck; run {fallback}"));
    }
    Ok(steps)
}

/// Undo installed root files, optionally removing the system trust anchor.
pub fn undo_system_steps(ctx: &SystemInitCtx<'_>, untrust_ca: bool) -> Vec<StepOutcome> {
    if let Err(e) = validate_system(ctx) {
        return vec![StepOutcome::new("system_steps", OutcomeState::Failed, e)];
    }
    fn remove_step(
        ctx: &SystemInitCtx<'_>,
        id: &'static str,
        path: &Path,
        reload: Option<CommandSpec>,
    ) -> StepOutcome {
        use std::os::unix::fs::PermissionsExt;
        let metadata = match fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_file() => meta,
            Ok(_) => {
                return StepOutcome::new(
                    id,
                    OutcomeState::Failed,
                    "refusing to remove a non-regular system file",
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return StepOutcome::new(id, OutcomeState::AlreadyDone, "file already absent");
            }
            Err(e) => return StepOutcome::new(id, OutcomeState::Failed, e.to_string()),
        };
        let bytes = match crate::ca::read_bounded_nofollow(path, 16 * 1024) {
            Ok(v) => v,
            Err(e) => return StepOutcome::new(id, OutcomeState::Failed, e.to_string()),
        };
        if let Err(e) = fs::remove_file(path) {
            return StepOutcome::new(id, OutcomeState::Failed, e.to_string());
        }
        if let Some(command) = reload
            && let Err(e) = crate::steps::run_checked(ctx.runner, &command, ctx.paths.fake_root.is_some())
        {
            let restore = crate::file::atomic_write(path, &bytes, metadata.permissions().mode() & 0o777);
            return StepOutcome::new(
                id,
                OutcomeState::Failed,
                match restore {
                    Ok(_) => e,
                    Err(r) => format!("{e}; restore failed: {r}"),
                },
            );
        }
        StepOutcome::new(id, OutcomeState::Changed, "removed system file")
    }
    let mut results = vec![
        remove_step(ctx, "ports", &ctx.paths.sysctl_conf, None),
        remove_step(
            ctx,
            "dns",
            &ctx.paths.dns_delegate,
            Some(CommandSpec::new(
                "/usr/bin/systemctl",
                ["reload", "systemd-resolved"],
                true,
            )),
        ),
    ];
    if untrust_ca {
        results.push(remove_step(
            ctx,
            "system_trust",
            &ctx.paths.system_cert,
            Some(CommandSpec::new(
                "/usr/sbin/update-ca-certificates",
                std::iter::empty::<&str>(),
                true,
            )),
        ));
    } else {
        results.push(StepOutcome::new(
            "system_trust",
            OutcomeState::Skipped,
            "system CA retained without --untrust-ca",
        ));
    }
    results
}
