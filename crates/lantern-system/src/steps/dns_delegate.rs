//! The systemd-resolved DNS delegate file.
use super::{run_checked, valid_tld};
use crate::{
    command::{CommandRunner, CommandSpec},
    file::atomic_write,
    paths::LanternPaths,
    step::{OutcomeState, Step, StepOutcome, StepState, preflight},
};
use std::{fs, net::SocketAddr};

/// Render the exact resolved delegate for validated inputs.
pub fn delegate_contents(tld: &str, listen: SocketAddr) -> Result<String, String> {
    if !valid_tld(tld) {
        return Err("invalid TLD".into());
    }
    if !listen.ip().is_loopback() || listen.port() == 0 {
        return Err("DNS listener must be a loopback address with a nonzero port".into());
    }
    Ok(format!(
        "# Managed by Lantern (lantern init). Do not edit; changes are overwritten.\n# Sends every lookup below \".{tld}\" to Lantern's local DNS responder (run by `lantern daemon`).\n# \"~\" = route-only: this server is never used for any other name.\n[Delegate]\nDNS={listen}\nDomains=~{tld}\n"
    ))
}
/// Install the route-only resolved delegate.
pub struct DnsDelegateStep<'a> {
    /// Resolved paths.
    pub paths: &'a LanternPaths,
    /// Command execution boundary.
    pub runner: &'a dyn CommandRunner,
    /// Configured TLD.
    pub tld: &'a str,
    /// DNS responder address.
    pub listen: SocketAddr,
    /// Preserve a pre-existing file during package setup.
    pub from_package: bool,
}
impl std::fmt::Debug for DnsDelegateStep<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DnsDelegateStep")
            .field("tld", &self.tld)
            .field("listen", &self.listen)
            .finish_non_exhaustive()
    }
}
impl Step for DnsDelegateStep<'_> {
    fn id(&self) -> &'static str {
        "dns"
    }
    fn description(&self) -> &'static str {
        "Route local domain queries to Lantern"
    }
    fn needs_root(&self) -> bool {
        true
    }
    fn fix_command(&self) -> &'static str {
        "sudo lantern init --system-steps"
    }
    fn check(&self) -> StepState {
        let contents = match delegate_contents(self.tld, self.listen) {
            Ok(s) => s,
            Err(e) => return StepState::Failed(e),
        };
        if self.tld == "localhost" {
            return StepState::Skipped("localhost needs no DNS delegate".into());
        }
        if fs::read(&self.paths.dns_delegate).is_ok_and(|s| s == contents.as_bytes()) {
            return StepState::Done("DNS delegate matches settings".into());
        }
        if self.from_package && self.paths.dns_delegate.exists() {
            return StepState::Skipped("existing delegate is preserved".into());
        }
        StepState::Pending("DNS delegate missing or differs".into())
    }
    fn apply(&self) -> StepOutcome {
        if let Err(out) = preflight(self.id(), self.check()) {
            return out;
        }
        let result = delegate_contents(self.tld, self.listen)
            .and_then(|s| {
                atomic_write(&self.paths.dns_delegate, s.as_bytes(), 0o644).map_err(|e| e.to_string())
            })
            .and_then(|_| {
                run_checked(
                    self.runner,
                    &CommandSpec::new("/usr/bin/systemctl", ["reload", "systemd-resolved"], true),
                )
                .map(|_| ())
            });
        match result {
            Ok(()) => StepOutcome::new(
                self.id(),
                OutcomeState::Changed,
                "wrote delegate and reloaded systemd-resolved",
            ),
            Err(e) => StepOutcome::new(self.id(), OutcomeState::Failed, e),
        }
    }
}
