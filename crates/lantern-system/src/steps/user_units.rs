//! Source-build user unit and optional XDG tray autostart files.
use super::run_checked;
use crate::{
    command::{CommandRunner, CommandSpec},
    file::atomic_write,
    paths::LanternPaths,
    step::{OutcomeState, Step, StepOutcome, StepState, preflight},
};
use std::{ffi::OsString, fs, os::unix::ffi::OsStrExt, path::Path};

fn quote_exec(path: &Path) -> Result<String, String> {
    let s = std::str::from_utf8(path.as_os_str().as_bytes()).map_err(|_| "binary path must be UTF-8")?;
    if !path.is_absolute()
        || s.chars().any(|c| c.is_control())
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("binary path must be absolute without control characters or '..'".into());
    }
    // systemd specifiers and shell-like splitting apply even inside quoted arguments.
    Ok(format!(
        "\"{}\"",
        s.replace('%', "%%")
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "$$")
    ))
}
/// Render a systemd unit using a literal absolute executable.
pub fn unit_contents(executable: &Path) -> Result<String, String> {
    Ok(format!(
        "[Unit]\nDescription=Lantern daemon (local dev-server manager)\n\n[Service]\nType=notify\nNotifyAccess=main\nExecStart={} daemon\nRestart=on-failure\nRestartSec=2\nTimeoutStartSec=30\nEnvironment=RUST_LOG=info\n\n[Install]\nWantedBy=default.target\n",
        quote_exec(executable)?
    ))
}
/// Render a desktop entry for tray autostart.
pub fn autostart_contents(executable: &Path) -> Result<String, String> {
    let exec = quote_exec(executable)?;
    Ok(format!(
        "[Desktop Entry]\nType=Application\nName=Lantern\nExec={exec} gui --tray\nIcon=lantern\nTerminal=false\nStartupNotify=false\nX-GNOME-Autostart-enabled=true\n"
    ))
}
/// Install a source-build unit, enable it, and optionally install tray autostart.
pub struct UserUnitsStep<'a> {
    /// Resolved user paths.
    pub paths: &'a LanternPaths,
    /// Command execution boundary.
    pub runner: &'a dyn CommandRunner,
    /// Absolute Lantern executable.
    pub executable: &'a Path,
    /// Whether the tray should be installed in the user's autostart directory.
    pub tray_autostart: bool,
}
impl std::fmt::Debug for UserUnitsStep<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserUnitsStep")
            .field("executable", &self.executable)
            .finish_non_exhaustive()
    }
}
impl UserUnitsStep<'_> {
    fn cmd(&self, args: &[&str], mutating: bool) -> CommandSpec {
        let mut command = CommandSpec::new("/usr/bin/systemctl", args.iter().copied(), mutating);
        if let Some(runtime) = &self.paths.runtime
            && let Some(root) = runtime.parent()
        {
            command
                .env
                .insert(OsString::from("XDG_RUNTIME_DIR"), root.as_os_str().to_owned());
            command.env.insert(
                OsString::from("DBUS_SESSION_BUS_ADDRESS"),
                OsString::from(format!("unix:path={}/bus", root.display())),
            );
        }
        command
    }
}
impl Step for UserUnitsStep<'_> {
    fn id(&self) -> &'static str {
        "user_units"
    }
    fn description(&self) -> &'static str {
        "Install and enable the user service and tray entry"
    }
    fn needs_root(&self) -> bool {
        false
    }
    fn fix_command(&self) -> &'static str {
        "lantern init --user-steps"
    }
    fn check(&self) -> StepState {
        let unit = match unit_contents(self.executable) {
            Ok(s) => s,
            Err(e) => return StepState::Failed(e),
        };
        let tray = match autostart_contents(self.executable) {
            Ok(s) => s,
            Err(e) => return StepState::Failed(e),
        };
        if self.paths.runtime.is_none() {
            return StepState::Skipped(
                "no user systemd session: log in graphically or enable lingering".into(),
            );
        }
        let packaged = self.executable == Path::new("/usr/bin/lantern");
        let unit_matches = packaged || fs::read(&self.paths.user_unit).is_ok_and(|s| s == unit.as_bytes());
        let tray_matches =
            !self.tray_autostart || fs::read(&self.paths.autostart).is_ok_and(|s| s == tray.as_bytes());
        let enabled = self
            .runner
            .run(&self.cmd(&["--user", "is-enabled", "lantern.service"], false))
            .is_ok_and(|o| o.success);
        if unit_matches && tray_matches && enabled {
            StepState::Done("user service and requested autostart are configured".into())
        } else {
            StepState::Pending("user service or tray entry needs setup".into())
        }
    }
    fn apply(&self) -> StepOutcome {
        if let Err(out) = preflight(self.id(), self.check()) {
            return out;
        }
        let result = (|| -> Result<(), String> {
            if self.executable != Path::new("/usr/bin/lantern") {
                let contents = unit_contents(self.executable)?;
                if atomic_write(&self.paths.user_unit, contents.as_bytes(), 0o644)
                    .map_err(|e| e.to_string())?
                {
                    run_checked(self.runner, &self.cmd(&["--user", "daemon-reload"], true))?;
                }
            }
            if self.tray_autostart {
                atomic_write(
                    &self.paths.autostart,
                    autostart_contents(self.executable)?.as_bytes(),
                    0o644,
                )
                .map_err(|e| e.to_string())?;
            }
            run_checked(
                self.runner,
                &self.cmd(&["--user", "enable", "--now", "lantern.service"], true),
            )?;
            Ok(())
        })();
        match result {
            Ok(()) => StepOutcome::new(
                self.id(),
                OutcomeState::Changed,
                "installed and enabled user service",
            ),
            Err(e) => StepOutcome::new(self.id(), OutcomeState::Failed, e),
        }
    }
}
