//! Caddy lifecycle and full-config admin client over a private Unix socket.

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Request, StatusCode, client::conn::http1};
use hyper_util::rt::TokioIo;
use lantern_core::ids::InstanceId;
use lantern_system::{ca, paths::LanternPaths};
use std::{
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{net::UnixStream, sync::Mutex};

use super::{
    caddy_config::{RenderCtx, render},
    routes::RoutePlan,
};
use crate::runtime::systemd::{Active, UnitStart, UserSystemd};

/// Network subsystem failure.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// Local filesystem or socket failure.
    #[error("network I/O: {0}")]
    Io(#[from] std::io::Error),
    /// No reachable admin socket.
    #[error("Caddy unavailable: {0}")]
    Unavailable(String),
    /// Caddy rejected the configuration.
    #[error("Caddy rejected configuration: {0}")]
    Rejected(String),
    /// Caddy could not bind a requested listener.
    #[error("Caddy port in use: {0}")]
    PortInUse(String),
    /// Invalid configuration or CA.
    #[error("network configuration: {0}")]
    Invalid(String),
}

/// Unit identity, binary, TLS name and renderer settings for one Caddy instance.
#[derive(Debug, Clone)]
pub struct CaddyConfig {
    /// Absolute Caddy executable path.
    pub binary: PathBuf,
    /// systemd service name, including `.service`.
    pub unit: String,
    /// systemd slice for the service.
    pub slice: String,
    /// TLD constrained by the root CA.
    pub tld: String,
    /// Caddy JSON rendering inputs.
    pub render: RenderCtx,
}

/// Result of a successful config synchronization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// Caddy accepted a new config.
    Applied,
    /// The same config was already applied.
    Unchanged,
}

/// Current unit and socket health.
#[derive(Debug, Clone, Default)]
pub struct CaddyStatus {
    /// Unit is active.
    pub running: bool,
    /// systemd's current main PID.
    pub pid: Option<u32>,
    /// Admin API answered successfully.
    pub admin_ok: bool,
    /// Count in the last accepted plan known to this client.
    pub route_count: usize,
    /// Most recent apply error, if any.
    pub last_error: Option<String>,
}

#[derive(Debug, Default)]
struct Applied {
    json: Option<String>,
    route_count: usize,
    last_error: Option<String>,
}

/// Serialized Caddy config publisher.
pub struct Caddy {
    cfg: CaddyConfig,
    sd: Arc<dyn UserSystemd>,
    paths: LanternPaths,
    applied: Mutex<Applied>,
    lifecycle: Mutex<()>,
}
impl std::fmt::Debug for Caddy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Caddy")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("missing parent"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".lantern-net-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}
fn shell_quote(path: &Path) -> Result<String, NetError> {
    let s = path
        .to_str()
        .ok_or_else(|| NetError::Invalid("non-UTF-8 path".into()))?;
    if !path.is_absolute() || s.contains('\0') {
        return Err(NetError::Invalid("expected absolute NUL-free path".into()));
    }
    Ok(format!("'{}'", s.replace('\'', "'\\''")))
}
async fn request(
    socket: &Path,
    method: &str,
    uri: &str,
    body: String,
) -> Result<(StatusCode, String), NetError> {
    let future = async {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|e| NetError::Unavailable(e.to_string()))?;
        let (mut sender, conn) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| NetError::Unavailable(e.to_string()))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| NetError::Invalid(e.to_string()))?;
        let response = sender
            .send_request(req)
            .await
            .map_err(|e| NetError::Unavailable(e.to_string()))?;
        let status = response.status();
        let bytes = Limited::new(response.into_body(), 16 * 1024 * 1024)
            .collect()
            .await
            .map_err(|e| NetError::Unavailable(e.to_string()))?
            .to_bytes();
        Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
    };
    tokio::time::timeout(Duration::from_secs(20), future)
        .await
        .map_err(|_| NetError::Unavailable("admin request timed out".into()))?
}

impl Caddy {
    /// Construct a Caddy manager with injected paths and systemd boundary.
    pub fn new(cfg: CaddyConfig, sd: Arc<dyn UserSystemd>, paths: LanternPaths) -> Result<Self, NetError> {
        if paths.caddy_admin.as_ref() != Some(&cfg.render.admin_socket)
            || cfg.render.storage != paths.caddy_data
            || cfg.render.root_cert != paths.root_cert
            || cfg.render.root_key != paths.root_key
            || !cfg.render.gateway.ip().is_loopback()
        {
            return Err(NetError::Invalid(
                "admin, storage and CA paths must match Lantern paths; gateway must be loopback".into(),
            ));
        }
        shell_quote(&cfg.binary)?;
        Ok(Self {
            cfg,
            sd,
            paths,
            applied: Mutex::new(Applied::default()),
            lifecycle: Mutex::new(()),
        })
    }
    /// Ensure the unit is running and its admin endpoint answers.
    pub async fn ensure_running(&self) -> Result<(), NetError> {
        let _guard = self.lifecycle.lock().await;
        ca::ensure_root(&self.paths, &self.cfg.tld).map_err(|e| NetError::Invalid(e.to_string()))?;
        let state = self
            .sd
            .poll(std::slice::from_ref(&self.cfg.unit))
            .await
            .map_err(|e| NetError::Unavailable(e.to_string()))?;
        if !state
            .iter()
            .any(|s| s.unit == self.cfg.unit && s.active == Active::Active)
        {
            if !self.paths.caddy_config.exists() {
                let initial = serde_json::to_vec_pretty(
                    &render(&RoutePlan::default(), &self.cfg.render).map_err(NetError::Invalid)?,
                )
                .map_err(|e| NetError::Invalid(e.to_string()))?;
                atomic_write(&self.paths.caddy_config, &initial)?;
            }
            let runtime = self
                .paths
                .runtime
                .as_ref()
                .ok_or_else(|| NetError::Invalid("missing XDG runtime directory".into()))?;
            std::fs::create_dir_all(runtime)?;
            std::fs::set_permissions(runtime, std::fs::Permissions::from_mode(0o700))?;
            let script = self.paths.dirs.state.join("caddy-launch.sh");
            let command = format!(
                "#!/bin/bash\nexec {} run --config {}\n",
                shell_quote(&self.cfg.binary)?,
                shell_quote(&self.paths.caddy_config)?
            );
            atomic_write(&script, command.as_bytes())?;
            let spec = UnitStart {
                unit: self.cfg.unit.clone(),
                instance: InstanceId::parse("caddy").map_err(|e| NetError::Invalid(e.to_string()))?,
                slice: self.cfg.slice.clone(),
                cwd: self.paths.dirs.state.clone(),
                script,
                post_stop: None,
                stop_timeout: Duration::from_secs(10),
            };
            self.sd
                .start(&spec)
                .await
                .map_err(|e| NetError::Unavailable(e.to_string()))?;
        }
        let until = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if tokio::time::timeout_at(
                until,
                request(&self.cfg.render.admin_socket, "GET", "/config/", String::new()),
            )
            .await
            .is_ok_and(|r| r.is_ok_and(|(s, _)| s.is_success()))
            {
                return Ok(());
            }
            if tokio::time::Instant::now() >= until {
                return Err(NetError::Unavailable(
                    "admin socket did not answer within 5 seconds".into(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    /// Apply a full plan, writing the startup config only after Caddy accepts it.
    pub async fn apply(&self, plan: &RoutePlan) -> Result<ApplyOutcome, NetError> {
        let mut state = self.applied.lock().await;
        let result = self.apply_inner(plan, &mut state).await;
        state.last_error = result.as_ref().err().map(ToString::to_string);
        result
    }
    async fn apply_inner(&self, plan: &RoutePlan, state: &mut Applied) -> Result<ApplyOutcome, NetError> {
        let value = render(plan, &self.cfg.render).map_err(NetError::Invalid)?;
        let text = serde_json::to_string_pretty(&value).map_err(|e| NetError::Invalid(e.to_string()))?;
        if state.json.as_deref() == Some(&text)
            && request(&self.cfg.render.admin_socket, "GET", "/config/", String::new())
                .await
                .is_ok_and(|(s, body)| {
                    s.is_success()
                        && serde_json::from_str::<serde_json::Value>(&body)
                            .is_ok_and(|current| current == value)
                })
        {
            return Ok(ApplyOutcome::Unchanged);
        }
        let mut response = request(&self.cfg.render.admin_socket, "POST", "/load", text.clone()).await;
        if response.is_err() {
            self.ensure_running().await?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            response = request(&self.cfg.render.admin_socket, "POST", "/load", text.clone()).await;
        }
        let (status, body) = response?;
        if !status.is_success() {
            return Err(if body.to_ascii_lowercase().contains("address already in use") {
                NetError::PortInUse(body)
            } else {
                NetError::Rejected(format!("HTTP {status}: {body}"))
            });
        }
        atomic_write(&self.paths.caddy_config, text.as_bytes())?;
        state.json = Some(text);
        state.route_count = plan.routes.len();
        Ok(ApplyOutcome::Applied)
    }
    /// Query unit and admin health without mutating either.
    pub async fn status(&self) -> CaddyStatus {
        let snapshots = self
            .sd
            .poll(std::slice::from_ref(&self.cfg.unit))
            .await
            .unwrap_or_default();
        let unit = snapshots.iter().find(|s| s.unit == self.cfg.unit);
        let admin_ok = request(&self.cfg.render.admin_socket, "GET", "/config/", String::new())
            .await
            .is_ok_and(|(s, _)| s.is_success());
        let state = self.applied.lock().await;
        CaddyStatus {
            running: unit.is_some_and(|u| u.active == Active::Active),
            pid: unit.and_then(|u| u.main_pid),
            admin_ok,
            route_count: state.route_count,
            last_error: state.last_error.clone(),
        }
    }
    /// Restart only the configured unit and wait for its admin endpoint.
    pub async fn restart(&self) -> Result<(), NetError> {
        self.sd
            .stop(&self.cfg.unit, Duration::from_secs(10))
            .await
            .map_err(|e| NetError::Unavailable(e.to_string()))?;
        self.applied.lock().await.json = None;
        self.ensure_running().await
    }
}
