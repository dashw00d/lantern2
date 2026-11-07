//! All Lantern and system-integration paths in one place.
use lantern_core::dirs::Dirs;
use std::path::{Component, Path, PathBuf};

/// Resolved paths for one user, and an optional prefix for system paths.
#[derive(Debug, Clone)]
pub struct LanternPaths {
    /// The core config, state and data directories.
    pub dirs: Dirs,
    /// The target user's home directory.
    pub home: PathBuf,
    /// Lantern's user runtime directory, when a session exists.
    pub runtime: Option<PathBuf>,
    /// Lantern's user cache directory.
    pub cache: PathBuf,
    /// An isolated root for system files, if supplied.
    pub fake_root: Option<PathBuf>,
    /// Root certificate and private key storage.
    pub ca_dir: PathBuf,
    /// Root certificate.
    pub root_cert: PathBuf,
    /// Root private key.
    pub root_key: PathBuf,
    /// Caddy intermediate certificate.
    pub intermediate_cert: PathBuf,
    /// Caddy intermediate key.
    pub intermediate_key: PathBuf,
    /// Caddy's issued certificates directory.
    pub certificates: PathBuf,
    /// Caddy's last applied configuration.
    pub caddy_config: PathBuf,
    /// Downloaded service binaries.
    pub bin: PathBuf,
    /// Per-service data directory.
    pub services: PathBuf,
    /// Caddy admin socket, if a session exists.
    pub caddy_admin: Option<PathBuf>,
    /// User systemd unit path.
    pub user_unit: PathBuf,
    /// User XDG tray autostart path.
    pub autostart: PathBuf,
    /// Persistent sysctl configuration.
    pub sysctl_conf: PathBuf,
    /// Kernel's live port threshold.
    pub sysctl_live: PathBuf,
    /// Resolved DNS delegate.
    pub dns_delegate: PathBuf,
    /// System CA trust anchor.
    pub system_cert: PathBuf,
    /// Hashed trust-store link for the Lantern CA.
    pub system_cert_link: PathBuf,
    /// Bundled Caddy binary.
    pub bundled_caddy: PathBuf,
}

impl LanternPaths {
    /// Construct paths using explicit user/session roots; runtime is the XDG runtime root.
    pub fn new(
        dirs: Dirs,
        home: PathBuf,
        runtime: Option<PathBuf>,
        cache: PathBuf,
        fake_root: Option<PathBuf>,
    ) -> Result<Self, String> {
        for path in [&dirs.config, &dirs.state, &dirs.data, &home, &cache]
            .into_iter()
            .chain(runtime.iter())
            .chain(fake_root.iter())
        {
            if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
                return Err(format!(
                    "path must be absolute and contain no '..': {}",
                    path.display()
                ));
            }
        }
        let system = |path: &str| match &fake_root {
            Some(root) => root.join(path.trim_start_matches('/')),
            None => PathBuf::from(path),
        };
        let ca_dir = dirs.data.join("caddy/pki/authorities/local");
        let caddy_config = dirs.state.join("caddy.json");
        let bin = dirs.data.join("bin");
        let services = dirs.data.join("services");
        let certificates = dirs.data.join("caddy/certificates/local");
        let caddy_admin = runtime.as_ref().map(|p| p.join("lantern/caddy-admin.sock"));
        let runtime = runtime.map(|p| p.join("lantern"));
        let user_unit = home.join(".config/systemd/user/lantern.service");
        let autostart = home.join(".config/autostart/lantern-tray.desktop");
        Ok(Self {
            root_cert: ca_dir.join("root.crt"),
            root_key: ca_dir.join("root.key"),
            intermediate_cert: ca_dir.join("intermediate.crt"),
            intermediate_key: ca_dir.join("intermediate.key"),
            ca_dir,
            caddy_config,
            bin,
            services,
            certificates,
            caddy_admin,
            runtime,
            user_unit,
            autostart,
            sysctl_conf: system("/etc/sysctl.d/60-lantern-ports.conf"),
            sysctl_live: system("/proc/sys/net/ipv4/ip_unprivileged_port_start"),
            dns_delegate: system("/etc/systemd/dns-delegate.d/lantern.dns-delegate"),
            system_cert: system("/usr/local/share/ca-certificates/lantern-local-ca.crt"),
            system_cert_link: system("/etc/ssl/certs/lantern-local-ca.pem"),
            bundled_caddy: system("/usr/lib/lantern/caddy"),
            dirs,
            home,
            cache,
            fake_root,
        })
    }

    /// Prefix a fixed absolute system path with the isolated test root when present.
    pub fn system(&self, path: &Path) -> PathBuf {
        assert!(path.is_absolute() && !path.components().any(|c| matches!(c, Component::ParentDir)));
        match &self.fake_root {
            Some(root) => root.join(path.strip_prefix("/").expect("absolute")),
            None => path.to_owned(),
        }
    }
}
