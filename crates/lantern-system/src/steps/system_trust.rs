//! Safe, rechecked installation of the user CA into the system trust store.
use super::run_checked;
use crate::{
    ca::{self, CaInfo},
    command::{CommandRunner, CommandSpec},
    file::atomic_write,
    paths::LanternPaths,
    step::{OutcomeState, Step, StepOutcome, StepState, preflight},
};
use std::{fs, io, os::unix::fs::PermissionsExt};

/// Install a validated user CA into the system trust store.
pub struct SystemTrustStep<'a> {
    /// Resolved system and user paths.
    pub paths: &'a LanternPaths,
    /// Command execution boundary.
    pub runner: &'a dyn CommandRunner,
    /// Target DNS TLD.
    pub tld: &'a str,
}
impl std::fmt::Debug for SystemTrustStep<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemTrustStep")
            .field("tld", &self.tld)
            .finish_non_exhaustive()
    }
}
impl SystemTrustStep<'_> {
    fn validated_root(&self) -> Result<(Vec<u8>, CaInfo), ca::CaError> {
        let bytes = ca::read_bounded_nofollow(&self.paths.root_cert, 16 * 1024)?;
        let (pem, info) = ca::verified_pem(&bytes, self.tld)?;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        if info.not_before > now || info.expires_at <= now {
            return Err(ca::CaError::Invalid(
                "root certificate is not currently valid".into(),
            ));
        }
        Ok((pem, info))
    }
}
impl Step for SystemTrustStep<'_> {
    fn id(&self) -> &'static str {
        "system_trust"
    }
    fn description(&self) -> &'static str {
        "Trust the constrained CA in the system store"
    }
    fn needs_root(&self) -> bool {
        true
    }
    fn fix_command(&self) -> &'static str {
        "sudo lantern init --system-steps"
    }
    fn check(&self) -> StepState {
        let (_, info) = match self.validated_root() {
            Ok(value) => value,
            Err(ca::CaError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                return StepState::Skipped("CA not created yet; run lantern init as your user first".into());
            }
            Err(e) => return StepState::Failed(e.to_string()),
        };
        let installed = match ca::read_bounded_nofollow(&self.paths.system_cert, 16 * 1024) {
            Ok(bytes) => ca::inspect_pem(&bytes, self.tld)
                .is_ok_and(|installed| installed.fingerprint_sha256 == info.fingerprint_sha256),
            Err(ca::CaError::Io(e)) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return StepState::Failed(e.to_string()),
        };
        if installed && self.paths.system_cert_link.exists() {
            StepState::Done("system store trusts the current CA".into())
        } else {
            StepState::Pending("system trust does not match the current CA".into())
        }
    }
    fn apply(&self) -> StepOutcome {
        if let Err(out) = preflight(self.id(), self.check()) {
            return out;
        }
        let result = (|| -> Result<(), String> {
            let (pem, _) = self.validated_root().map_err(|e| e.to_string())?;
            let old = match fs::symlink_metadata(&self.paths.system_cert) {
                Ok(meta) if !meta.file_type().is_file() => {
                    return Err("system CA target is not a regular file".into());
                }
                Ok(meta) => Some((
                    ca::read_bounded_nofollow(&self.paths.system_cert, 16 * 1024)
                        .map_err(|e| e.to_string())?,
                    meta.permissions().mode() & 0o777,
                )),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.to_string()),
            };
            atomic_write(&self.paths.system_cert, &pem, 0o644).map_err(|e| e.to_string())?;
            if let Err(e) = run_checked(
                self.runner,
                &CommandSpec::new(
                    "/usr/sbin/update-ca-certificates",
                    std::iter::empty::<&str>(),
                    true,
                ),
                self.paths.fake_root.is_some(),
            ) {
                let restored = match old {
                    Some((bytes, mode)) => atomic_write(&self.paths.system_cert, &bytes, mode).map(|_| ()),
                    None => fs::remove_file(&self.paths.system_cert),
                };
                return match restored {
                    Ok(()) => Err(e),
                    Err(restore) => Err(format!("{e}; failed to restore CA file: {restore}")),
                };
            }
            Ok(())
        })();
        match result {
            Ok(()) => StepOutcome::new(
                self.id(),
                OutcomeState::Changed,
                "installed constrained system CA",
            ),
            Err(e) => StepOutcome::new(self.id(), OutcomeState::Failed, e),
        }
    }
}
