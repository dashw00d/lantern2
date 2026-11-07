//! Browser NSS profile discovery and certificate trust.
use super::run_checked;
use crate::{
    command::{CommandRunner, CommandSpec},
    paths::LanternPaths,
    step::{OutcomeState, Step, StepOutcome, StepState, preflight},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

/// NSS trust-store category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Browser {
    /// Chrome-family shared NSS database.
    Chrome,
    /// Firefox profile NSS database.
    Firefox,
}
/// An NSS database and its browser family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NssDatabase {
    /// Browser family.
    pub browser: Browser,
    /// Directory containing the NSS database.
    pub path: PathBuf,
    /// True when the database may need initialization.
    pub initialize: bool,
}
/// Discover the shared, Snap and Firefox/Flatpak NSS databases without creating them.
pub fn discover(paths: &LanternPaths) -> Vec<NssDatabase> {
    let home = &paths.home;
    let mut result = vec![NssDatabase {
        browser: Browser::Chrome,
        path: home.join(".pki/nssdb"),
        initialize: true,
    }];
    if home.join("snap/chromium/current").is_dir() {
        result.push(NssDatabase {
            browser: Browser::Chrome,
            path: home.join("snap/chromium/current/.pki/nssdb"),
            initialize: true,
        });
    }
    for root in [
        home.join(".mozilla/firefox"),
        home.join("snap/firefox/common/.mozilla/firefox"),
        home.join(".var/app/org.mozilla.firefox/.mozilla/firefox"),
    ] {
        if let Ok(entries) = fs::read_dir(root) {
            let mut dirs: Vec<_> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.join("cert9.db").is_file())
                .collect();
            dirs.sort();
            result.extend(dirs.into_iter().map(|path| NssDatabase {
                browser: Browser::Firefox,
                path,
                initialize: false,
            }));
        }
    }
    result
}
/// Decode one PEM-encoded certificate into DER.
pub fn certificate_der(pem: &[u8]) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(pem).map_err(|e| e.to_string())?;
    let start = text
        .find("-----BEGIN CERTIFICATE-----")
        .ok_or("missing certificate PEM header")?
        + "-----BEGIN CERTIFICATE-----".len();
    let rest = &text[start..];
    let end = rest
        .find("-----END CERTIFICATE-----")
        .ok_or("missing certificate PEM footer")?;
    let body: String = rest[..end].chars().filter(|c| !c.is_ascii_whitespace()).collect();
    STANDARD.decode(body).map_err(|e| e.to_string())
}
/// Render the certutil command for an NSS database.
pub fn certutil_command(
    program: &Path,
    db: &NssDatabase,
    operation: &[OsString],
    mutating: bool,
) -> CommandSpec {
    let mut args = vec![OsString::from("-d"), {
        let mut s = OsString::from("sql:");
        s.push(&db.path);
        s
    }];
    args.extend_from_slice(operation);
    CommandSpec::new(program, args, mutating)
}
/// Compare the DER and SSL trusted-CA flag in one NSS database with the Lantern root.
pub fn trusted(runner: &dyn CommandRunner, certutil: &Path, db: &NssDatabase, root_der: &[u8]) -> bool {
    let args = ["-L", "-n", "Lantern Local CA", "-a"].map(OsString::from);
    let matches = runner
        .run(&certutil_command(certutil, db, &args, false))
        .is_ok_and(|o| o.success && certificate_der(&o.stdout).is_ok_and(|der| der == root_der));
    if !matches {
        return false;
    }
    let listing = [OsString::from("-L")];
    runner
        .run(&certutil_command(certutil, db, &listing, false))
        .is_ok_and(|o| {
            o.success
                && String::from_utf8_lossy(&o.stdout).lines().any(|line| {
                    line.trim_start()
                        .strip_prefix("Lantern Local CA")
                        .and_then(|s| s.split_whitespace().next())
                        .is_some_and(|flags| flags.split(',').next().is_some_and(|ssl| ssl.contains('C')))
                })
        })
}
/// Install the Lantern root into all discovered NSS stores.
pub struct NssTrustStep<'a> {
    /// Resolved paths.
    pub paths: &'a LanternPaths,
    /// Command execution boundary.
    pub runner: &'a dyn CommandRunner,
    /// Absolute path to certutil, if installed.
    pub certutil: Option<&'a Path>,
}
impl std::fmt::Debug for NssTrustStep<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NssTrustStep")
            .field("certutil", &self.certutil)
            .finish_non_exhaustive()
    }
}
impl Step for NssTrustStep<'_> {
    fn id(&self) -> &'static str {
        "nss_trust"
    }
    fn description(&self) -> &'static str {
        "Trust Lantern's CA in browser NSS databases"
    }
    fn needs_root(&self) -> bool {
        false
    }
    fn fix_command(&self) -> &'static str {
        "lantern init --user-steps"
    }
    fn check(&self) -> StepState {
        let cert = match fs::read(&self.paths.root_cert) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return StepState::Skipped("CA not created yet".into());
            }
            Err(e) => return StepState::Failed(e.to_string()),
        };
        let der = match certificate_der(&cert) {
            Ok(d) => d,
            Err(e) => return StepState::Failed(e),
        };
        let Some(certutil) = self.certutil.filter(|p| p.is_absolute()) else {
            return StepState::Failed("certutil is not installed (install libnss3-tools)".into());
        };
        let dbs = discover(self.paths);
        let missing = dbs
            .iter()
            .filter(|db| !trusted(self.runner, certutil, db, &der))
            .count();
        if missing == 0 {
            StepState::Done("all discovered browser databases trust the CA".into())
        } else {
            StepState::Pending(format!("{missing} browser database(s) need trust"))
        }
    }
    fn apply(&self) -> StepOutcome {
        if let Err(out) = preflight(self.id(), self.check()) {
            return out;
        }
        let result = (|| -> Result<(), String> {
            let root = fs::read(&self.paths.root_cert).map_err(|e| e.to_string())?;
            let der = certificate_der(&root)?;
            let certutil = self.certutil.ok_or("certutil is not installed")?;
            for db in discover(self.paths) {
                if trusted(self.runner, certutil, &db, &der) {
                    continue;
                }
                if db.initialize && !db.path.join("cert9.db").exists() {
                    fs::create_dir_all(&db.path).map_err(|e| e.to_string())?;
                    let args = ["-N", "--empty-password"].map(OsString::from);
                    run_checked(self.runner, &certutil_command(certutil, &db, &args, true))?;
                }
                // Delete only when a certificate by this nickname exists; never ignore database errors from deletion.
                let lookup = ["-L", "-n", "Lantern Local CA", "-a"].map(OsString::from);
                if self
                    .runner
                    .run(&certutil_command(certutil, &db, &lookup, false))
                    .map_err(|e| e.to_string())?
                    .success
                {
                    let args = ["-D", "-n", "Lantern Local CA"].map(OsString::from);
                    run_checked(self.runner, &certutil_command(certutil, &db, &args, true))?;
                }
                let mut args = ["-A", "-t", "C,,", "-n", "Lantern Local CA", "-i"]
                    .map(OsString::from)
                    .to_vec();
                args.push(self.paths.root_cert.as_os_str().to_owned());
                run_checked(self.runner, &certutil_command(certutil, &db, &args, true))?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => StepOutcome::new(
                self.id(),
                OutcomeState::Changed,
                "installed browser trust; restart browsers",
            ),
            Err(e) => StepOutcome::new(self.id(), OutcomeState::Failed, e),
        }
    }
}
