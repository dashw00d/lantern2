//! Network assembly and health state, with late Caddy discovery.

use crate::{
    net::{
        caddy::{Caddy, CaddyConfig},
        caddy_config::RenderCtx,
        routes::RoutePlan,
        sync::{ApplyFuture, RouteApplier},
    },
    registry::RegistryHandle,
    runtime::systemd::SystemctlBackend,
};
use lantern_core::{
    api::{Check, CheckStatus, Fix},
    model::Instance,
};
use lantern_system::paths::LanternPaths;
use std::{
    collections::BTreeMap,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::Mutex as AsyncMutex;

fn executable(path: &Path) -> bool {
    path.is_absolute()
        && std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Resolve Caddy on each attempt so installation after daemon startup is detected.
pub fn resolve_caddy(configured: &str, data: &Path, search_path: &str) -> Option<PathBuf> {
    let search: Vec<_> = std::env::split_paths(search_path)
        .filter(|p| p.is_absolute())
        .collect();
    let configured = Path::new(configured);
    if !configured.as_os_str().is_empty() {
        let candidates = if configured.is_absolute() {
            vec![configured.to_path_buf()]
        } else if configured.components().count() == 1 {
            search.iter().map(|dir| dir.join(configured)).collect()
        } else {
            Vec::new()
        };
        if let Some(path) = candidates.into_iter().find(|p| executable(p)) {
            return Some(path);
        }
    }
    std::iter::once(data.join("bin/caddy"))
        .chain(search.into_iter().map(|p| p.join("caddy")))
        .find(|p| executable(p))
}

/// Resolve D22's auto socket to the newest installed system PHP-FPM socket.
pub fn php_socket(configured: &str) -> PathBuf {
    if configured != "auto" {
        return PathBuf::from(configured);
    }
    let mut sockets: Vec<_> = std::fs::read_dir("/run/php")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let version = name.strip_prefix("php")?.strip_suffix("-fpm.sock")?;
            let numbers: Vec<u32> = version
                .split('.')
                .map(str::parse)
                .collect::<Result<_, _>>()
                .ok()?;
            if numbers.is_empty() {
                return None;
            }
            Some((numbers, entry.path()))
        })
        .collect();
    sockets.sort_by(|a, b| b.0.cmp(&a.0));
    sockets
        .into_iter()
        .next()
        .map(|(_, p)| p)
        .unwrap_or_else(|| PathBuf::from("/run/php/php-fpm.sock"))
}

/// Network diagnostics shared by the runtime and system health endpoint.
#[derive(Debug, Default)]
pub struct NetworkHealth {
    caddy_error: Mutex<Option<String>>,
    dns_error: Mutex<Option<String>>,
}
impl NetworkHealth {
    pub fn set_dns_error(&self, message: String) {
        *self.dns_error.lock().unwrap() = Some(message);
    }
    pub fn set_caddy_error(&self, message: Option<String>) {
        *self.caddy_error.lock().unwrap() = message;
    }
    pub fn checks(&self) -> Vec<Check> {
        let mut checks = Vec::new();
        if let Some(message) = self.caddy_error.lock().unwrap().clone() {
            let missing = message == "caddy_missing";
            checks.push(Check {
                id: if missing { "caddy_missing" } else { "caddy.network" }.into(),
                title: "Caddy proxy".into(),
                status: CheckStatus::Warn,
                detail: if missing {
                    "Caddy executable not found; routes are retained until it is installed.".into()
                } else {
                    message
                },
                fix: missing.then(|| Fix {
                    description: "Install Caddy 2.8 or newer, or provide Lantern's bundled binary.".into(),
                    command: "lantern init".into(),
                    needs_root: false,
                }),
            });
        }
        if let Some(message) = self.dns_error.lock().unwrap().clone() {
            checks.push(Check {
                id: "dns.responder".into(),
                title: "DNS responder".into(),
                status: CheckStatus::Fail,
                detail: message,
                fix: None,
            });
        }
        checks
    }
}

/// Level-triggered Caddy adapter, holding desired plans when Caddy is absent.
pub struct CaddyApplier {
    pub configured: String,
    pub search_path: String,
    pub paths: LanternPaths,
    pub unit: String,
    pub slice: String,
    pub tld: String,
    pub render: RenderCtx,
    pub health: Arc<NetworkHealth>,
    manager: AsyncMutex<Option<(PathBuf, Arc<Caddy>)>>,
    last_pid: AsyncMutex<Option<u32>>,
}
impl std::fmt::Debug for CaddyApplier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaddyApplier")
            .field("unit", &self.unit)
            .finish_non_exhaustive()
    }
}
impl CaddyApplier {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        configured: String,
        search_path: String,
        paths: LanternPaths,
        unit: String,
        slice: String,
        tld: String,
        render: RenderCtx,
        health: Arc<NetworkHealth>,
    ) -> Self {
        Self {
            configured,
            search_path,
            paths,
            unit,
            slice,
            tld,
            render,
            health,
            manager: AsyncMutex::new(None),
            last_pid: AsyncMutex::new(None),
        }
    }
    /// Resolve the currently usable executable without starting it.
    pub fn binary(&self) -> Option<PathBuf> {
        resolve_caddy(&self.configured, &self.paths.dirs.data, &self.search_path)
    }
    /// Inspect the configured unit and invalidate the last config after a restart.
    pub async fn check_health(&self) {
        if self.binary().is_none() {
            self.health.set_caddy_error(Some("caddy_missing".into()));
            return;
        }
        let manager = self
            .manager
            .lock()
            .await
            .as_ref()
            .map(|(_, manager)| manager.clone());
        let Some(manager) = manager else { return };
        let status = manager.status().await;
        let mut last_pid = self.last_pid.lock().await;
        if *last_pid != status.pid || !status.running || !status.admin_ok {
            manager.invalidate().await;
            *last_pid = status.pid;
        }
        if !status.running || !status.admin_ok {
            self.health.set_caddy_error(Some(
                status
                    .last_error
                    .unwrap_or_else(|| "Caddy unit or admin endpoint is unavailable".into()),
            ));
        }
        if let Some(pid) = status.pid
            && std::fs::read_link(format!("/proc/{pid}/exe"))
                .is_ok_and(|path| path.to_string_lossy().ends_with(" (deleted)"))
        {
            if let Err(error) = manager.restart().await {
                self.health.set_caddy_error(Some(error.to_string()));
            }
            *last_pid = None;
        }
    }
    async fn apply_plan(&self, plan: &RoutePlan) -> Result<(), String> {
        let Some(binary) = self.binary() else {
            self.health.set_caddy_error(Some("caddy_missing".into()));
            // Absence must not block a project's lifecycle; the desired route is retained.
            return Ok(());
        };
        let manager = {
            let mut current = self.manager.lock().await;
            if current.as_ref().is_none_or(|(path, _)| *path != binary) {
                let caddy = Caddy::new(
                    CaddyConfig {
                        binary: binary.clone(),
                        unit: self.unit.clone(),
                        slice: self.slice.clone(),
                        tld: self.tld.clone(),
                        render: self.render.clone(),
                    },
                    Arc::new(SystemctlBackend),
                    self.paths.clone(),
                )
                .map_err(|e| e.to_string())?;
                *current = Some((binary, Arc::new(caddy)));
            }
            current.as_ref().unwrap().1.clone()
        };
        let result = manager.apply(plan).await.map(|_| ()).map_err(|e| e.to_string());
        self.health.set_caddy_error(result.as_ref().err().cloned());
        result
    }
}
impl RouteApplier for CaddyApplier {
    fn apply<'a>(&'a self, plan: &'a RoutePlan) -> ApplyFuture<'a> {
        Box::pin(self.apply_plan(plan))
    }
}

/// Recover complete instances from the registry's public snapshot.
pub fn registry_instances(registry: &RegistryHandle) -> Vec<Instance> {
    registry
        .instances()
        .into_iter()
        .filter_map(|v| registry.instance(&v.id))
        .collect()
}

/// Build paths whose runtime component is the daemon's effective private runtime directory.
pub fn paths(
    dirs: lantern_core::dirs::Dirs,
    home: PathBuf,
    runtime: PathBuf,
    cache: PathBuf,
) -> Result<LanternPaths, String> {
    let mut paths = LanternPaths::new(dirs, home, None, cache, None)?;
    paths.caddy_admin = Some(runtime.join("caddy-admin.sock"));
    paths.runtime = Some(runtime);
    Ok(paths)
}

/// Build renderer settings from daemon configuration.
pub fn render(
    paths: &LanternPaths,
    settings: &lantern_core::settings::Settings,
    php_socket: PathBuf,
) -> RenderCtx {
    RenderCtx {
        gateway: ([127, 0, 0, 1], settings.daemon.port).into(),
        admin_socket: paths.caddy_admin.clone().expect("runtime path supplied"),
        storage: paths.caddy_data.clone(),
        root_cert: paths.root_cert.clone(),
        root_key: paths.root_key.clone(),
        php_socket,
        php_env: BTreeMap::new(),
        proxy: settings.proxy.clone(),
    }
}
