//! Read-only TLS trust status and constrained CA initialization.

use super::caddy::NetError;
use lantern_system::{
    ca::{self, CaInfo, CaState},
    command::CommandRunner,
    paths::LanternPaths,
    step::{Step, StepState},
    steps::{
        nss_trust::{self, Browser},
        system_trust::SystemTrustStep,
    },
};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Status of the constrained Lantern root CA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootStatus {
    /// No certificate or private key exists.
    Missing,
    /// The certificate and matching private key are valid.
    Ok(CaInfo),
    /// The root, constraint, permissions or key pairing is invalid.
    Invalid(String),
}
/// Status of one browser trust store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreState {
    /// The current root is trusted for TLS.
    Trusted,
    /// The database or Lantern certificate is missing.
    Missing,
    /// A Lantern certificate is present but stale or not trusted.
    Stale,
    /// certutil was not supplied.
    NoCertutil,
    /// The root certificate cannot be checked.
    NoCa,
}
/// Browser trust store inspection result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreStatus {
    /// Browser family.
    pub browser: Browser,
    /// NSS database directory.
    pub path: PathBuf,
    /// Trust state.
    pub state: StoreState,
}
/// Constrained CA and trust status, cached for at most one minute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsStatus {
    /// Local root validity.
    pub ca: RootStatus,
    /// Whether the system trust anchor and hashed link match the root.
    pub system_trusted: bool,
    /// Discovered NSS stores and their status.
    pub stores: Vec<StoreStatus>,
}
/// CA access and read-only trust checks using lantern-system.
pub struct Tls {
    paths: LanternPaths,
    tld: String,
    runner: Arc<dyn CommandRunner + Send + Sync>,
    certutil: Option<PathBuf>,
    cache: Mutex<Option<(Instant, TlsStatus)>>,
}
impl std::fmt::Debug for Tls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tls")
            .field("tld", &self.tld)
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}
impl Tls {
    /// Construct with explicit paths, TLD, command boundary and optional certutil executable.
    pub fn new(
        paths: LanternPaths,
        tld: String,
        runner: Arc<dyn CommandRunner + Send + Sync>,
        certutil: Option<PathBuf>,
    ) -> Self {
        Self {
            paths,
            tld,
            runner,
            certutil,
            cache: Mutex::new(None),
        }
    }
    /// Ensure the root exists; unsafe rotation is reported to the caller.
    pub fn ensure_ca(&self) -> Result<CaState, NetError> {
        let result = ca::ensure_root(&self.paths, &self.tld).map_err(|e| NetError::Invalid(e.to_string()));
        *self.cache.lock().unwrap() = None;
        result
    }
    /// Root certificate path for additive Node.js trust configuration.
    pub fn root_cert_path(&self) -> &Path {
        &self.paths.root_cert
    }
    /// Inspect root and trust stores without changing them.
    pub fn status(&self) -> TlsStatus {
        let mut cache = self.cache.lock().unwrap();
        if let Some((at, status)) = &*cache
            && at.elapsed() < Duration::from_secs(60)
        {
            return status.clone();
        }
        let cert_exists = std::fs::symlink_metadata(&self.paths.root_cert).is_ok();
        let key_exists = std::fs::symlink_metadata(&self.paths.root_key).is_ok();
        let root = if !cert_exists && !key_exists {
            RootStatus::Missing
        } else {
            match ca::inspect_root(&self.paths, &self.tld, 0) {
                Ok(info) => RootStatus::Ok(info),
                Err(e) => RootStatus::Invalid(e.to_string()),
            }
        };
        let system_trusted = matches!(
            SystemTrustStep {
                paths: &self.paths,
                runner: self.runner.as_ref(),
                tld: &self.tld
            }
            .check(),
            StepState::Done(_)
        );
        let root_der = if matches!(root, RootStatus::Ok(_)) {
            ca::read_bounded_nofollow(&self.paths.root_cert, 16384)
                .ok()
                .and_then(|v| nss_trust::certificate_der(&v).ok())
        } else {
            None
        };
        let stores = nss_trust::discover(&self.paths)
            .into_iter()
            .map(|db| {
                let state = match (&root_der, &self.certutil) {
                    (None, _) => StoreState::NoCa,
                    (_, None) => StoreState::NoCertutil,
                    (Some(der), Some(certutil)) if !certutil.is_absolute() => StoreState::NoCertutil,
                    (Some(_), Some(_)) if !db.path.join("cert9.db").is_file() => StoreState::Missing,
                    (Some(der), Some(certutil))
                        if nss_trust::trusted(self.runner.as_ref(), certutil, &db, der) =>
                    {
                        StoreState::Trusted
                    }
                    (Some(_), Some(certutil)) => {
                        let args = ["-L", "-n", "Lantern Local CA", "-a"].map(OsString::from);
                        match self
                            .runner
                            .run(&nss_trust::certutil_command(certutil, &db, &args, false))
                        {
                            Ok(out) if out.success => StoreState::Stale,
                            _ => StoreState::Missing,
                        }
                    }
                };
                StoreStatus {
                    browser: db.browser,
                    path: db.path,
                    state,
                }
            })
            .collect();
        let status = TlsStatus {
            ca: root,
            system_trusted,
            stores,
        };
        *cache = Some((Instant::now(), status.clone()));
        status
    }
}
