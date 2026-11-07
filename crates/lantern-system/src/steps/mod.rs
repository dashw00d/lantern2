//! Independently checkable installation steps.

pub mod dns_delegate;
pub mod sysctl;
pub mod user_units;
pub mod v1_migrate;

use crate::command::{CommandOutput, CommandRunner, CommandSpec};

pub(crate) fn run_checked(runner: &dyn CommandRunner, spec: &CommandSpec) -> Result<CommandOutput, String> {
    let out = runner
        .run(spec)
        .map_err(|e| format!("{}: {e}", spec.program.display()))?;
    if !out.success {
        return Err(format!(
            "{} failed: {}",
            spec.program.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out)
}

/// Validate the TLD before interpolating it into any configuration.
pub fn valid_tld(tld: &str) -> bool {
    let b = tld.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}
pub mod nss_trust;
