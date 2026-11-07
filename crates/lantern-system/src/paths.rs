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
    /// Caddy storage root.
    pub caddy_data: PathBuf,
    /// Mailpit's data directory.
    pub mailpit_data: PathBuf,
    /// Redis's data directory.
    pub redis_data: PathBuf,
    /// Parent of per-major Postgres data directories.
    pub postgres_data: PathBuf,
    /// Postgres Unix socket directory for the user session.
    pub postgres_socket: Option<PathBuf>,
    /// Managed PHP-FPM configuration.
    pub php_fpm_config: PathBuf,
    /// Managed PHP-FPM socket in the user session.
    pub php_fpm_socket: Option<PathBuf>,
    /// Managed PHP-FPM PID file in the user session.
    pub php_fpm_pid: Option<PathBuf>,
    /// Shared Chrome-family NSS database.
    pub chrome_nss: PathBuf,
    /// Chromium Snap current home.
    pub chromium_snap_home: PathBuf,
    /// Chromium Snap NSS database.
    pub chromium_snap_nss: PathBuf,
    /// Roots containing Firefox profiles: native, Snap, Flatpak.
    pub firefox_profiles: [PathBuf; 3],
    /// v1 system daemon unit.
    pub v1_daemon_unit: PathBuf,
    /// v1 sudoers rule.
    pub v1_sudoers: PathBuf,
    /// v1 Caddy site directory.
    pub v1_caddy_sites: PathBuf,
    /// System Caddyfile checked for the v1 signature.
    pub v1_caddyfile: PathBuf,
    /// v1 NetworkManager DNS override.
    pub v1_networkmanager_dns: PathBuf,
    /// v1 dnsmasq DNS override.
    pub v1_dnsmasq_dns: PathBuf,
    /// Legacy installation directory.
    pub v1_install: PathBuf,
    /// Legacy CLI symlink.
    pub v1_cli: PathBuf,
    /// System directory containing CA anchors.
    pub system_ca_dir: PathBuf,
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
        let fake_root = fake_root.or_else(|| {
            std::env::var_os("LANTERN_FAKE_ROOT")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        });
        if fake_root.as_ref().is_some_and(|p| {
            p == Path::new("/") || std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
        }) {
            return Err("fake root must not be / or a symlink".into());
        }
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
            caddy_data: dirs.data.join("caddy"),
            mailpit_data: services.join("mailpit"),
            redis_data: services.join("redis"),
            postgres_data: services.join("postgres"),
            postgres_socket: runtime.as_ref().map(|p| p.join("pg")),
            php_fpm_config: dirs.state.join("php-fpm.conf"),
            php_fpm_socket: runtime.as_ref().map(|p| p.join("php-fpm.sock")),
            php_fpm_pid: runtime.as_ref().map(|p| p.join("php-fpm.pid")),
            chrome_nss: home.join(".pki/nssdb"),
            chromium_snap_home: home.join("snap/chromium/current"),
            chromium_snap_nss: home.join("snap/chromium/current/.pki/nssdb"),
            firefox_profiles: [
                home.join(".mozilla/firefox"),
                home.join("snap/firefox/common/.mozilla/firefox"),
                home.join(".var/app/org.mozilla.firefox/.mozilla/firefox"),
            ],
            v1_daemon_unit: system("/etc/systemd/system/lanternd.service"),
            v1_sudoers: system("/etc/sudoers.d/lantern"),
            v1_caddy_sites: system("/etc/caddy/sites.d"),
            v1_caddyfile: system("/etc/caddy/Caddyfile"),
            v1_networkmanager_dns: system("/etc/NetworkManager/dnsmasq.d/lantern.conf"),
            v1_dnsmasq_dns: system("/etc/dnsmasq.d/lantern.conf"),
            v1_install: system("/opt/lantern"),
            v1_cli: system("/usr/local/bin/lantern"),
            system_ca_dir: system("/usr/local/share/ca-certificates"),
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

    /// Override the XDG config root used for the user unit and tray entry.
    pub fn with_user_config_root(mut self, root: &Path) -> Result<Self, String> {
        if !root.is_absolute() || root.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err("XDG config root must be absolute and contain no '..'".into());
        }
        self.user_unit = root.join("systemd/user/lantern.service");
        self.autostart = root.join("autostart/lantern-tray.desktop");
        Ok(self)
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
