//! Synchronous init composition and privilege-boundary checks.
use crate::{
    command::{CommandRunner, CommandSpec},
    paths::LanternPaths,
    step::{OutcomeState, Step, StepOutcome},
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
    /// Lantern executable.
    pub executable: &'a Path,
    /// Absolute certutil path, when installed.
    pub certutil: Option<&'a Path>,
    /// Opt in to tray autostart.
    pub tray_autostart: bool,
    /// CA generation step; required to complete initialization.
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
    /// Numeric non-root target user.
    pub uid: u32,
    /// Selected privileged-port strategy.
    pub port_strategy: PortStrategy,
    /// Preserve existing files during package setup and omit trust.
    pub from_package: bool,
    /// Include the v1 detection/cleanup step.
    pub migrate_v1: bool,
    /// Allow guarded v1 cleanup inside a fake root only.
    pub allow_fake_cleanup: bool,
    /// System trust step; required except during package setup.
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
fn effective_uid() -> Result<u32, String> {
    let status = fs::read_to_string("/proc/self/status").map_err(|e| e.to_string())?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|s| s.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| "cannot determine effective uid".into())
}
fn validate_system(ctx: &SystemInitCtx<'_>) -> Result<SocketAddr, String> {
    if ctx.uid == 0 {
        return Err("target uid must not be root".into());
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
    if ctx.paths.fake_root.is_none() {
        if effective_uid()? != 0 {
            return Err("system steps require root".into());
        }
        let passwd = fs::read_to_string("/etc/passwd").map_err(|e| e.to_string())?;
        let home = passwd
            .lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.split(':').collect();
                (fields.len() >= 7 && fields[2].parse::<u32>().ok() == Some(ctx.uid))
                    .then(|| Path::new(fields[5]).to_path_buf())
            })
            .next()
            .ok_or("target uid not found in /etc/passwd")?;
        if !home.is_absolute()
            || home != ctx.paths.home
            || ctx.paths.dirs.config != home.join(".config/lantern")
            || ctx.paths.dirs.data != home.join(".local/share/lantern")
        {
            return Err("root steps require paths resolved from the target user's passwd entry, not the caller environment".into());
        }
    }
    Ok(listen)
}
/// Run user steps, returning a separate result for each action.
pub fn run_user_steps(ctx: &InitCtx<'_>) -> Vec<StepOutcome> {
    if ctx.paths.fake_root.is_none() && effective_uid().is_ok_and(|uid| uid == 0) {
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
    outcomes.push(match ctx.ca_step {
        Some(step) if !step.needs_root() => step.apply(),
        Some(_) => StepOutcome::new("ca", OutcomeState::Failed, "CA step must not require root"),
        None => StepOutcome::new("ca", OutcomeState::Failed, "CA generation is not available"),
    });
    outcomes.push(
        NssTrustStep {
            paths: ctx.paths,
            runner: ctx.runner,
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
            None => StepOutcome::new(
                "system_trust",
                OutcomeState::Failed,
                "system CA trust is not available",
            ),
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
