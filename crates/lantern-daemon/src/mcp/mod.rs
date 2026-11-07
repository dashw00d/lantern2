//! Stateless MCP transport over the daemon facade.

mod api_client;
mod args;
mod descriptions;
mod dispatch;
mod resources;
mod tools;
pub mod upstream;

use std::{sync::Arc, time::Duration};

use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::router::{prompt::PromptRouter, tool::ToolRouter},
    model::{Implementation, ServerCapabilities, ServerConfig},
    model::{
        ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams,
        ReadResourceResponse,
    },
    prompt_handler,
    service::RequestContext,
    tool_handler,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    },
};
use tokio_util::sync::CancellationToken;

use crate::api::facade::DaemonFacade;

/// Configuration for the mountable MCP service.
#[derive(Clone)]
pub struct McpConfig {
    /// Cancellation is owned by the daemon and shared with its HTTP server.
    pub shutdown: CancellationToken,
    /// Destination restrictions shared by project API calls and intel fetches.
    pub upstream_policy: upstream::UpstreamPolicy,
    /// Injectable HTTP transport; it receives only policy-validated requests.
    pub api_client: Arc<dyn upstream::ToolApiClient>,
    /// Enable API calls only after the facade preserves all risk metadata, including critical.
    pub trust_risk_metadata: bool,
    /// User setting permitting critical calls after explicit confirmation.
    pub allow_critical_calls: bool,
}
impl Default for McpConfig {
    fn default() -> Self {
        Self {
            shutdown: CancellationToken::new(),
            upstream_policy: upstream::UpstreamPolicy::default(),
            api_client: upstream::default_client(),
            trust_risk_metadata: false,
            allow_critical_calls: false,
        }
    }
}
impl std::fmt::Debug for McpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConfig")
            .field("upstream_policy", &self.upstream_policy)
            .field("trust_risk_metadata", &self.trust_risk_metadata)
            .field("allow_critical_calls", &self.allow_critical_calls)
            .finish_non_exhaustive()
    }
}

/// Cheap per-request MCP handler.
#[derive(Clone)]
pub struct LanternMcp {
    facade: Arc<dyn DaemonFacade>,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
    api: api_client::ApiState,
}

impl std::fmt::Debug for LanternMcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LanternMcp").finish_non_exhaustive()
    }
}

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for LanternMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_prompts().enable_resources().build())
            .with_server_info(Implementation::new("lantern", env!("CARGO_PKG_VERSION")))
            .with_instructions("Lantern manages local dev servers. Use list_projects or list_instances, start_project, get_project_logs, get_project_docs and get_project_endpoints. Branches are instances; use start_branch for new worktrees. Long actions return an operation_id. Call only registered project APIs, and ask the user before confirming risky calls.")
    }
    async fn list_resources(
        &self,
        r: Option<PaginatedRequestParams>,
        c: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        self.resource_list(r, c).await
    }
    async fn list_resource_templates(
        &self,
        r: Option<PaginatedRequestParams>,
        c: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        self.resource_templates(r, c).await
    }
    async fn read_resource(
        &self,
        r: ReadResourceRequestParams,
        c: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        self.resource_read(r, c).await
    }
}

/// Tower service that can be mounted at `/mcp` behind the daemon's API guard.
pub type McpService = StreamableHttpService<LanternMcp, NeverSessionManager>;

/// Create a stateless streamable HTTP service; Host and Origin are checked by the API guard.
pub fn service(facade: Arc<dyn DaemonFacade>, config: McpConfig) -> McpService {
    let handler = LanternMcp {
        facade,
        tool_router: LanternMcp::tool_router(),
        prompt_router: LanternMcp::prompt_router(),
        api: api_client::ApiState::new(
            config.upstream_policy,
            config.api_client,
            config.trust_risk_metadata,
            config.allow_critical_calls,
        ),
    };
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
