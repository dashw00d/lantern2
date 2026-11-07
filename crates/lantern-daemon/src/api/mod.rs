//! REST and SSE server (docs/15-api-mcp.md).

mod dispatch;
mod error;
pub mod facade;
mod guard;
mod sse;

use crate::{events::EventBus, operations::OperationTracker};
use axum::{Extension, Router, middleware};
use facade::DaemonFacade;
use std::{fmt, io, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Semaphore};
use tokio_util::sync::CancellationToken;

/// Startup values used by the security guard and greeting; changes require a restart.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// Loopback HTTP port.
    pub port: u16,
    /// DNS top-level domain, such as `glow`.
    pub tld: String,
    /// Optional non-default public gateway port.
    pub gateway_port: Option<u16>,
    /// Version sent in the root and SSE greeting.
    pub version: String,
    /// Identifier for this daemon run.
    pub boot_id: String,
    /// Deadline for calls into daemon subsystems.
    pub module_timeout: Duration,
    /// Interval between SSE keepalive comments.
    pub sse_ping_interval: Duration,
}
impl ApiConfig {
    /// Construct a startup configuration with a 15-second module deadline.
    pub fn new(port: u16, tld: impl Into<String>, boot_id: impl Into<String>) -> Self {
        Self {
            port,
            tld: tld.into(),
            gateway_port: None,
            version: env!("CARGO_PKG_VERSION").into(),
            boot_id: boot_id.into(),
            module_timeout: Duration::from_secs(15),
            sse_ping_interval: Duration::from_secs(15),
        }
    }
}
/// Shared HTTP state; the operation tracker and event bus must be the daemon's own.
#[derive(Clone)]
pub struct ApiState {
    /// Adapter to the daemon subsystems.
    pub facade: Arc<dyn DaemonFacade>,
    /// Shared state event bus.
    pub bus: EventBus,
    /// Shared operation tracker.
    pub operations: OperationTracker,
    /// Immutable startup configuration.
    pub config: ApiConfig,
    /// Shutdown for direct router use; `serve` supplies the caller's stream token.
    pub shutdown: CancellationToken,
    sse_slots: Arc<Semaphore>,
}
impl fmt::Debug for ApiState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiState")
            .field("config", &self.config)
            .field("bus", &self.bus)
            .field("operations", &self.operations)
            .finish_non_exhaustive()
    }
}
impl ApiState {
    /// Construct state with a 32-stream SSE limit.
    pub fn new(
        facade: Arc<dyn DaemonFacade>,
        bus: EventBus,
        operations: OperationTracker,
        config: ApiConfig,
    ) -> Self {
        Self {
            facade,
            bus,
            operations,
            config,
            shutdown: CancellationToken::new(),
            sse_slots: Arc::new(Semaphore::new(32)),
        }
    }
}
/// Construct the REST/SSE router with security enforced on every request.
pub fn router(state: ApiState) -> Router {
    let config = state.config.clone();
    Router::new()
        .fallback(dispatch::handle)
        .with_state(state)
        .layer(middleware::from_fn_with_state(config, guard::check))
        .layer(middleware::from_fn(guard::headers))
}
/// Serve the listener until cancellation, closing SSE streams before graceful shutdown completes.
pub async fn serve(listener: TcpListener, router: Router, shutdown: CancellationToken) -> io::Result<()> {
    let router = router.layer(Extension(shutdown.clone()));
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
}
