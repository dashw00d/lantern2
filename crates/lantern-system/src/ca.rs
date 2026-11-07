//! Generate and inspect a self-signed, name-constrained Lantern root.
use crate::{
    file::atomic_write,
    paths::LanternPaths,
    step::{OutcomeState, Step, StepOutcome, StepState},
    steps::{nss_trust::certificate_der, valid_tld},
};
use ::time::{Duration, OffsetDateTime};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, GeneralSubtree, IsCa, KeyPair,
    KeyUsagePurpose, NameConstraints,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read},
    os::unix::fs::PermissionsExt,
    path::Path,
};
use x509_parser::{extensions::GeneralName, prelude::*};

/// CA inspection or creation failure.
#[derive(Debug, thiserror::Error)]
pub enum CaError {
    /// A file operation failed.
    #[error("CA file: {0}")]
    Io(#[from] io::Error),
    /// Certificate, key, or settings are invalid.
    #[error("invalid CA: {0}")]
    Invalid(String),
    /// Existing files need coordinated rotation before replacement.
    #[error("CA rotation requires stopping Caddy and coordinated replacement: {0}")]
    RotationRequired(String),
}
/// Inspected root certificate properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaInfo {
    /// Lowercase hex SHA-256 of the certificate DER.
    pub fingerprint_sha256: String,
    /// Expiry as Unix seconds.
    pub expires_at: i64,
    /// Start of validity as Unix seconds.
    pub not_before: i64,
    /// The sole permitted DNS subtree.
    pub permitted_subtree: String,
    /// Whether the name constraint extension is critical.
    pub constraint_critical: bool,
}
/// Outcome of ensuring the local root exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaState {
    /// An existing valid root was retained.
    Existing(CaInfo),
    /// A new root was created.
    Created(CaInfo),
}
/// Read a bounded regular file without following its final symlink.
pub fn read_bounded_nofollow(path: &Path, limit: u64) -> Result<Vec<u8>, CaError> {
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let file = fs::File::from(fd);
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > limit {
        return Err(CaError::Invalid(
            "CA input must be a regular file of at most 16 KiB".into(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(CaError::Invalid("CA input exceeds size limit".into()));
    }
    Ok(bytes)
}
/// Verify the root's signature, CA role and exact critical DNS constraint.
pub fn inspect_der(der: &[u8], tld: &str) -> Result<CaInfo, CaError> {
    if !valid_tld(tld) {
        return Err(CaError::Invalid("invalid TLD".into()));
    }
    let (rest, cert) =
        parse_x509_certificate(der).map_err(|e| CaError::Invalid(format!("certificate DER: {e}")))?;
    if !rest.is_empty() {
        return Err(CaError::Invalid("trailing certificate data".into()));
    }
    if cert.subject() != cert.issuer() || cert.verify_signature(None).is_err() {
        return Err(CaError::Invalid("root must be self-signed".into()));
    }
    if !cert.is_ca()
        || !cert
            .key_usage()
            .map_err(|e| CaError::Invalid(e.to_string()))?
            .is_some_and(|usage| usage.value.key_cert_sign())
    {
        return Err(CaError::Invalid("root must be a certificate-signing CA".into()));
    }
    let constraints = cert
        .name_constraints()
        .map_err(|e| CaError::Invalid(e.to_string()))?
        .ok_or_else(|| CaError::Invalid("name constraints are missing".into()))?;
    let expected = format!(".{tld}");
    let permitted = constraints
        .value
        .permitted_subtrees
        .as_deref()
        .unwrap_or_default();
    if !constraints.critical
        || permitted.len() != 1
        || !matches!(&permitted[0].base, GeneralName::DNSName(name) if *name == expected)
        || constraints
            .value
            .excluded_subtrees
            .as_ref()
            .is_some_and(|v| !v.is_empty())
    {
        return Err(CaError::Invalid(format!(
            "critical permitted DNS subtree must be exactly {expected}"
        )));
    }
    Ok(CaInfo {
        fingerprint_sha256: hex::encode(Sha256::digest(der)),
        expires_at: cert.validity().not_after.timestamp(),
        not_before: cert.validity().not_before.timestamp(),
        permitted_subtree: expected,
        constraint_critical: constraints.critical,
    })
}
/// Inspect a PEM root certificate and verify its constraint.
pub fn inspect_pem(pem: &[u8], tld: &str) -> Result<CaInfo, CaError> {
    let der = certificate_der(pem).map_err(CaError::Invalid)?;
    inspect_der(&der, tld)
}
/// Re-encode a verified certificate as canonical PEM.
pub fn verified_pem(pem: &[u8], tld: &str) -> Result<(Vec<u8>, CaInfo), CaError> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let der = certificate_der(pem).map_err(CaError::Invalid)?;
    let info = inspect_der(&der, tld)?;
    let b64 = STANDARD.encode(der);
    let mut encoded = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in b64.as_bytes().chunks(64) {
        encoded.push_str(std::str::from_utf8(line).map_err(|e| CaError::Invalid(e.to_string()))?);
        encoded.push('\n');
    }
    encoded.push_str("-----END CERTIFICATE-----\n");
    Ok((encoded.into_bytes(), info))
}
fn existing(paths: &LanternPaths, tld: &str, minimum_days: i64) -> Result<CaInfo, CaError> {
    let pem = read_bounded_nofollow(&paths.root_cert, 16 * 1024)?;
    let info = inspect_pem(&pem, tld)?;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    if info.not_before > now || info.expires_at <= now + minimum_days * 86400 {
        return Err(CaError::Invalid(format!(
            "CA is not currently valid for at least {minimum_days} days"
        )));
    }
    let key_bytes = read_bounded_nofollow(&paths.root_key, 16 * 1024)?;
    if fs::symlink_metadata(&paths.root_key)?.permissions().mode() & 0o777 != 0o600 {
        return Err(CaError::Invalid("root.key must have mode 0600".into()));
    }
    let key =
        KeyPair::from_pem(std::str::from_utf8(&key_bytes).map_err(|e| CaError::Invalid(e.to_string()))?)
            .map_err(|e| CaError::Invalid(e.to_string()))?;
    let der = certificate_der(&pem).map_err(CaError::Invalid)?;
    let (_, cert) = parse_x509_certificate(&der).map_err(|e| CaError::Invalid(e.to_string()))?;
    if key.public_key_raw() != cert.public_key().subject_public_key.data.as_ref() {
        return Err(CaError::Invalid("root key does not match certificate".into()));
    }
    Ok(info)
}
/// Inspect the user CA, private-key mode and key/certificate pairing.
pub fn inspect_root(paths: &LanternPaths, tld: &str, minimum_days: i64) -> Result<CaInfo, CaError> {
    existing(paths, tld, minimum_days)
}
fn generate_material(tld: &str) -> Result<(Vec<u8>, Vec<u8>, CaInfo), CaError> {
    let now = OffsetDateTime::now_utc();
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::OrganizationName, "Lantern");
    params
        .distinguished_name
        .push(DnType::CommonName, "Lantern Local CA");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.name_constraints = Some(NameConstraints {
        permitted_subtrees: vec![GeneralSubtree::DnsName(format!(".{tld}"))],
        excluded_subtrees: vec![],
    });
    params.not_before = now - Duration::days(1);
    params.not_after = now + Duration::days(3652);
    let key = KeyPair::generate().map_err(|e| CaError::Invalid(e.to_string()))?;
    let cert = params
        .self_signed(&key)
        .map_err(|e| CaError::Invalid(e.to_string()))?;
    let info = inspect_der(cert.der().as_ref(), tld)?;
    Ok((key.serialize_pem().into_bytes(), cert.pem().into_bytes(), info))
}
/// Create a new root, or retain an existing valid root; unsafe rotation is refused.
pub fn ensure_root(paths: &LanternPaths, tld: &str) -> Result<CaState, CaError> {
    if !valid_tld(tld) {
        return Err(CaError::Invalid("invalid TLD".into()));
    }
    let cert_exists = fs::symlink_metadata(&paths.root_cert).is_ok();
    let key_exists = fs::symlink_metadata(&paths.root_key).is_ok();
    let old_caddy_state = [
        &paths.intermediate_cert,
        &paths.intermediate_key,
        &paths.certificates,
    ]
    .into_iter()
    .any(|path| fs::symlink_metadata(path).is_ok());
    if !cert_exists && !key_exists && old_caddy_state {
        return Err(CaError::RotationRequired(
            "orphaned Caddy certificates must be archived before creating a new root".into(),
        ));
    }
    if cert_exists || key_exists {
        return existing(paths, tld, 90)
            .map(CaState::Existing)
            .map_err(|e| CaError::RotationRequired(e.to_string()));
    }
    fs::create_dir_all(&paths.ca_dir)?;
    let dir = fs::symlink_metadata(&paths.ca_dir)?;
    if !dir.file_type().is_dir() {
        return Err(CaError::Invalid("CA directory must be a real directory".into()));
    }
    fs::set_permissions(&paths.ca_dir, fs::Permissions::from_mode(0o700))?;
    let (key_pem, cert_pem, info) = generate_material(tld)?;
    atomic_write(&paths.root_key, &key_pem, 0o600)?;
    if let Err(e) = atomic_write(&paths.root_cert, &cert_pem, 0o644) {
        let _ = fs::remove_file(&paths.root_key);
        return Err(e.into());
    }
    Ok(CaState::Created(info))
}
/// Rotate a stale or invalid root while coordinating Caddy and preserving old files.
pub fn rotate_root(
    paths: &LanternPaths,
    tld: &str,
    runner: &dyn crate::command::CommandRunner,
) -> Result<CaInfo, CaError> {
    use crate::{command::CommandSpec, steps::run_checked};
    use std::{ffi::OsString, path::PathBuf};
    if !valid_tld(tld) {
        return Err(CaError::Invalid("invalid TLD".into()));
    }
    let runtime =
        paths.runtime.as_ref().and_then(|p| p.parent()).ok_or_else(|| {
            CaError::RotationRequired("no user systemd session for Caddy coordination".into())
        })?;
    let command = |args: &[&str], mutating: bool| {
        let mut spec = CommandSpec::new("/usr/bin/systemctl", args.iter().copied(), mutating);
        spec.env
            .insert(OsString::from("XDG_RUNTIME_DIR"), runtime.as_os_str().to_owned());
        spec.env.insert(
            OsString::from("DBUS_SESSION_BUS_ADDRESS"),
            OsString::from(format!("unix:path={}/bus", runtime.display())),
        );
        spec
    };
    if fs::symlink_metadata(&paths.ca_dir).is_err_and(|e| e.kind() == io::ErrorKind::NotFound) {
        fs::create_dir_all(&paths.ca_dir)?;
        fs::set_permissions(&paths.ca_dir, fs::Permissions::from_mode(0o700))?;
    }
    if !fs::symlink_metadata(&paths.ca_dir).is_ok_and(|m| m.file_type().is_dir()) {
        return Err(CaError::Invalid("CA directory must be a real directory".into()));
    }
    let candidates = [
        (&paths.root_key, "root.key"),
        (&paths.root_cert, "root.crt"),
        (&paths.intermediate_key, "intermediate.key"),
        (&paths.intermediate_cert, "intermediate.crt"),
        (&paths.certificates, "certificates"),
    ];
    for (path, name) in candidates {
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(CaError::Invalid(format!("refusing symlinked {name}")));
            }
            Ok(meta)
                if (name == "certificates" && !meta.is_dir())
                    || (name != "certificates" && !meta.is_file()) =>
            {
                return Err(CaError::Invalid(format!("unexpected file type for {name}")));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let (key, cert, info) = generate_material(tld)?;
    let status = runner
        .run(&command(&["--user", "is-active", "lantern-caddy.service"], false))
        .map_err(CaError::Io)?;
    let was_active = if status.success {
        true
    } else if matches!(
        String::from_utf8_lossy(&status.stdout).trim(),
        "inactive" | "failed" | "unknown"
    ) {
        false
    } else {
        return Err(CaError::RotationRequired(
            "could not determine Caddy state".into(),
        ));
    };
    if was_active {
        run_checked(
            runner,
            &command(&["--user", "stop", "lantern-caddy.service"], true),
            paths.fake_root.is_some(),
        )
        .map_err(CaError::RotationRequired)?;
    }
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut written_key = false;
    let mut written_cert = false;
    let mut archive: Option<PathBuf> = None;
    let attempt = (|| -> Result<(), CaError> {
        use std::os::unix::fs::DirBuilderExt;
        let now = OffsetDateTime::now_utc().unix_timestamp();
        for suffix in 0..1000 {
            let dir = paths.ca_dir.join(format!(".old-{now}-{suffix}"));
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => {
                    archive = Some(dir);
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        let dir = archive
            .as_ref()
            .ok_or_else(|| CaError::Invalid("could not create unique CA archive".into()))?;
        for (source, name) in candidates {
            if fs::symlink_metadata(source).is_ok() {
                let destination = dir.join(name);
                fs::rename(source, &destination)?;
                moved.push((source.to_path_buf(), destination));
            }
        }
        atomic_write(&paths.root_key, &key, 0o600)?;
        written_key = true;
        atomic_write(&paths.root_cert, &cert, 0o644)?;
        written_cert = true;
        if was_active {
            run_checked(
                runner,
                &command(&["--user", "start", "lantern-caddy.service"], true),
                paths.fake_root.is_some(),
            )
            .map_err(CaError::RotationRequired)?;
        }
        Ok(())
    })();
    if let Err(error) = attempt {
        let mut recovery_errors = Vec::new();
        if written_cert && let Err(e) = fs::remove_file(&paths.root_cert) {
            recovery_errors.push(e.to_string());
        }
        if written_key && let Err(e) = fs::remove_file(&paths.root_key) {
            recovery_errors.push(e.to_string());
        }
        for (source, destination) in moved.iter().rev() {
            if let Err(e) = fs::rename(destination, source) {
                recovery_errors.push(e.to_string());
            }
        }
        if let Some(dir) = archive {
            let _ = fs::remove_dir(dir);
        }
        if was_active
            && let Err(e) = run_checked(
                runner,
                &command(&["--user", "start", "lantern-caddy.service"], true),
                paths.fake_root.is_some(),
            )
        {
            recovery_errors.push(format!("Caddy restart: {e}"));
        }
        return if recovery_errors.is_empty() {
            Err(error)
        } else {
            Err(CaError::RotationRequired(format!(
                "{error}; recovery incomplete: {}",
                recovery_errors.join("; ")
            )))
        };
    }
    Ok(info)
}
/// User step that creates the local root and coordinates necessary rotations.
pub struct CaStep<'a> {
    /// User paths.
    pub paths: &'a LanternPaths,
    /// Configured TLD.
    pub tld: &'a str,
    /// Command boundary used to stop and restart Caddy during rotation.
    pub runner: Option<&'a dyn crate::command::CommandRunner>,
}
impl std::fmt::Debug for CaStep<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaStep")
            .field("tld", &self.tld)
            .finish_non_exhaustive()
    }
}
impl Step for CaStep<'_> {
    fn id(&self) -> &'static str {
        "ca"
    }
    fn description(&self) -> &'static str {
        "Create the name-constrained local CA"
    }
    fn needs_root(&self) -> bool {
        false
    }
    fn fix_command(&self) -> &'static str {
        "lantern init --user-steps"
    }
    fn check(&self) -> StepState {
        if !valid_tld(self.tld) {
            return StepState::Failed("invalid TLD".into());
        }
        let cert = fs::symlink_metadata(&self.paths.root_cert).is_ok();
        let key = fs::symlink_metadata(&self.paths.root_key).is_ok();
        if !cert && !key {
            return StepState::Pending("local CA has not been created".into());
        }
        match existing(self.paths, self.tld, 90) {
            Ok(info) => StepState::Done(format!("CA {} is valid", info.fingerprint_sha256)),
            Err(e) => StepState::Pending(format!("CA needs coordinated rotation: {e}")),
        }
    }
    fn apply(&self) -> StepOutcome {
        match ensure_root(self.paths, self.tld) {
            Ok(CaState::Existing(_)) => {
                StepOutcome::new(self.id(), OutcomeState::AlreadyDone, "local CA is valid")
            }
            Ok(CaState::Created(_)) => StepOutcome::new(
                self.id(),
                OutcomeState::Changed,
                "created name-constrained local CA",
            ),
            Err(CaError::RotationRequired(_)) if self.runner.is_some() => {
                match rotate_root(self.paths, self.tld, self.runner.expect("checked")) {
                    Ok(_) => StepOutcome::new(
                        self.id(),
                        OutcomeState::Changed,
                        "rotated CA; browser and system trust must be refreshed",
                    ),
                    Err(e) => StepOutcome::new(self.id(), OutcomeState::Failed, e.to_string()),
                }
            }
            Err(e) => StepOutcome::new(self.id(), OutcomeState::Failed, e.to_string()),
        }
    }
}
