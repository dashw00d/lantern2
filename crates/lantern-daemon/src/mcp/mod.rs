//! Stateless MCP transport over the daemon facade.

use std::{sync::Arc, time::Duration};

use rmcp::{
    ServerHandler,
    model::{Implementation, ServerCapabilities, ServerConfig},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    },
};
use tokio_util::sync::CancellationToken;

use crate::api::facade::DaemonFacade;

/// Configuration for the mountable MCP service.
#[derive(Debug, Clone, Default)]
pub struct McpConfig {
    /// Cancellation is owned by the daemon and shared with its HTTP server.
    pub shutdown: CancellationToken,
}

/// Cheap per-request MCP handler.
#[derive(Clone)]
pub struct LanternMcp {
    _facade: Arc<dyn DaemonFacade>,
}

impl std::fmt::Debug for LanternMcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LanternMcp").finish_non_exhaustive()
    }
}

impl ServerHandler for LanternMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().build())
            .with_server_info(Implementation::new("lantern", env!("CARGO_PKG_VERSION")))
    }
}

/// Tower service that can be mounted at `/mcp` behind the daemon's API guard.
pub type McpService = StreamableHttpService<LanternMcp, NeverSessionManager>;

/// Create a stateless streamable HTTP service; Host and Origin are checked by the API guard.
pub fn service(facade: Arc<dyn DaemonFacade>, config: McpConfig) -> McpService {
    let handler = LanternMcp { _facade: facade };
    let transport = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .disable_allowed_hosts()
        .with_max_request_body_bytes(1 << 20)
        .with_sse_keep_alive(Some(Duration::from_secs(15)))
        .with_cancellation_token(config.shutdown);
    StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(NeverSessionManager::default()),
        transport,
    )
}
