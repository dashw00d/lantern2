//! Unprivileged port access, by sysctl or the file-capability alternative.
use super::run_checked;
use crate::{
    command::{CommandRunner, CommandSpec},
    file::atomic_write,
    paths::LanternPaths,
    step::{OutcomeState, Step, StepOutcome, StepState, preflight},
};
use std::{ffi::OsString, fs, path::Path};

/// User-selected privileged-port policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortStrategy {
    /// Set the unprivileged port threshold system-wide.
    Sysctl,
    /// Grant the selected Caddy executable CAP_NET_BIND_SERVICE.
    Setcap,
}
/// Configure and verify unprivileged port access.
pub struct PortsStep<'a> {
    /// Resolved system paths.
    pub paths: &'a LanternPaths,
    /// Command execution boundary.
    pub runner: &'a dyn CommandRunner,
    /// HTTP listener port.
    pub http_port: u16,
    /// HTTPS listener port.
    pub https_port: u16,
    /// Selected Caddy executable, absolute and trusted by the caller.
    pub caddy: &'a Path,
    /// Which policy to apply when neither already suffices.
    pub strategy: PortStrategy,
    /// Package setup must not overwrite an existing sysctl file.
    pub from_package: bool,
}
impl std::fmt::Debug for PortsStep<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortsStep")
            .field("strategy", &self.strategy)
            .field("caddy", &self.caddy)
            .finish_non_exhaustive()
    }
}
impl PortsStep<'_> {
    fn validate(&self) -> Result<(), String> {
        if self.http_port == 0 || self.https_port == 0 {
            return Err("ports must be in 1..=65535".into());
        }
        if !self.caddy.is_absolute()
            || self
                .caddy
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("Caddy executable must be an absolute path without '..'".into());
        }
        Ok(())
    }
    fn has_cap(&self) -> bool {
        let spec = CommandSpec::new("/usr/sbin/getcap", [self.caddy.as_os_str()], false);
        self.runner.run(&spec).is_ok_and(|out| {
            out.success
                && String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .any(|field| {
                        field.split_once('=').is_some_and(|(caps, flags)| {
                            caps.split(',').any(|cap| cap == "cap_net_bind_service")
                                && flags.contains('e')
                                && flags.contains('p')
                        })
                    })
        })
    }
}
impl Step for PortsStep<'_> {
    fn id(&self) -> &'static str {
        "ports"
    }
    fn description(&self) -> &'static str {
        "Allow Caddy to bind the configured ports"
    }
    fn needs_root(&self) -> bool {
        true
    }
    fn fix_command(&self) -> &'static str {
        "sudo lantern init --system-steps"
    }
    fn check(&self) -> StepState {
        if let Err(e) = self.validate() {
            return StepState::Failed(e);
        }
        let min = self.http_port.min(self.https_port);
        if min >= 1024 {
            return StepState::Done("both ports are unprivileged".into());
        }
        if fs::read_to_string(&self.paths.sysctl_live)
            .ok()
            .and_then(|s| s.trim().parse::<u16>().ok())
            .is_some_and(|v| v <= min)
        {
            return StepState::Done("live port threshold permits the configured ports".into());
        }
        if self.has_cap() {
            return StepState::Done("Caddy has CAP_NET_BIND_SERVICE".into());
        }
        if self.from_package && self.paths.sysctl_conf.exists() && self.strategy == PortStrategy::Sysctl {
            return StepState::Skipped("existing sysctl configuration is preserved".into());
        }
        StepState::Pending("privileged ports require sysctl or a Caddy capability".into())
    }
    fn apply(&self) -> StepOutcome {
        if let Err(out) = preflight(self.id(), self.check()) {
            return out;
        }
        let result = match self.strategy {
            PortStrategy::Sysctl => {
                let threshold = 80u16.min(self.http_port.min(self.https_port));
                let text = format!("# Managed by Lantern\nnet.ipv4.ip_unprivileged_port_start={threshold}\n");
                atomic_write(&self.paths.sysctl_conf, text.as_bytes(), 0o644)
                    .map_err(|e| e.to_string())
                    .and_then(|_| {
                        run_checked(
                            self.runner,
                            &CommandSpec::new(
                                "/usr/sbin/sysctl",
                                [
                                    OsString::from("-w"),
                                    OsString::from(format!(
                                        "net.ipv4.ip_unprivileged_port_start={threshold}"
                                    )),
                                ],
                                true,
                            ),
                        )
                        .map(|_| ())
                    })
            }
            PortStrategy::Setcap => {
                if self.caddy != self.paths.bundled_caddy
                    || !fs::symlink_metadata(self.caddy).is_ok_and(|m| m.file_type().is_file())
                {
                    Err("setcap is restricted to the regular bundled Caddy executable".into())
                } else {
                    run_checked(
                        self.runner,
                        &CommandSpec::new(
                            "/usr/sbin/setcap",
                            [
                                OsString::from("cap_net_bind_service=+ep"),
                                self.caddy.as_os_str().to_owned(),
                            ],
                            true,
                        ),
                    )
                    .map(|_| ())
                }
            }
        };
        match result {
            Ok(()) => StepOutcome::new(
                self.id(),
                OutcomeState::Changed,
                "configured privileged port access",
            ),
            Err(e) => StepOutcome::new(self.id(), OutcomeState::Failed, e),
        }
    }
}
