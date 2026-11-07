//! MCP tool registration and dispatch.

use super::{LanternMcp, args::*, descriptions};
use rmcp::{handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router};

#[tool_router(vis = "pub(super)")]
impl LanternMcp {
    #[tool(description = descriptions::LIST_PROJECTS, annotations(read_only_hint = true))]
    async fn list_projects(&self, Parameters(args): Parameters<ListProjectsArgs>) -> CallToolResult {
        self.invoke(
            "list_projects",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::SEARCH_PROJECTS, annotations(read_only_hint = true))]
    async fn search_projects(&self, Parameters(args): Parameters<SearchProjectsArgs>) -> CallToolResult {
        self.invoke(
            "search_projects",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::GET_PROJECT, annotations(read_only_hint = true))]
    async fn get_project(&self, Parameters(args): Parameters<TargetArgs>) -> CallToolResult {
        self.invoke(
            "get_project",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::LIST_INSTANCES, annotations(read_only_hint = true))]
    async fn list_instances(&self, Parameters(args): Parameters<ListInstancesArgs>) -> CallToolResult {
        self.invoke(
            "list_instances",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::START_PROJECT, annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true))]
    async fn start_project(&self, Parameters(args): Parameters<LifecycleArgs>) -> CallToolResult {
        self.invoke(
            "start_project",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::STOP_PROJECT, annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true))]
    async fn stop_project(&self, Parameters(args): Parameters<LifecycleArgs>) -> CallToolResult {
        self.invoke(
            "stop_project",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::RESTART_PROJECT, annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true))]
    async fn restart_project(&self, Parameters(args): Parameters<LifecycleArgs>) -> CallToolResult {
        self.invoke(
            "restart_project",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::START_BRANCH, annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true))]
    async fn start_branch(&self, Parameters(args): Parameters<StartBranchArgs>) -> CallToolResult {
        self.invoke(
            "start_branch",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::GET_OPERATION, annotations(read_only_hint = true))]
    async fn get_operation(&self, Parameters(args): Parameters<GetOperationArgs>) -> CallToolResult {
        self.invoke(
            "get_operation",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::GET_PROJECT_LOGS, annotations(read_only_hint = true))]
    async fn get_project_logs(&self, Parameters(args): Parameters<GetLogsArgs>) -> CallToolResult {
        self.invoke(
            "get_project_logs",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::GET_PROJECT_DOCS, annotations(read_only_hint = true))]
    async fn get_project_docs(&self, Parameters(args): Parameters<GetDocsArgs>) -> CallToolResult {
        self.invoke(
            "get_project_docs",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::GET_PROJECT_ENDPOINTS, annotations(read_only_hint = true))]
    async fn get_project_endpoints(&self, Parameters(args): Parameters<GetEndpointsArgs>) -> CallToolResult {
        self.invoke(
            "get_project_endpoints",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::GET_PROJECT_DISCOVERY, annotations(read_only_hint = true))]
    async fn get_project_discovery(&self, Parameters(args): Parameters<TargetArgs>) -> CallToolResult {
        self.invoke(
            "get_project_discovery",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::REFRESH_DISCOVERY, annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true))]
    async fn refresh_discovery(&self, Parameters(args): Parameters<OptionalTargetArgs>) -> CallToolResult {
        self.invoke(
            "refresh_discovery",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::CHECK_HEALTH, annotations(read_only_hint = true))]
    async fn check_health(&self, Parameters(args): Parameters<HealthArgs>) -> CallToolResult {
        self.invoke(
            "check_health",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::GET_DEPENDENCIES, annotations(read_only_hint = true))]
    async fn get_dependencies(&self, Parameters(args): Parameters<DependencyArgs>) -> CallToolResult {
        self.invoke(
            "get_dependencies",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::GET_PORTS, annotations(read_only_hint = true))]
    async fn get_ports(&self, Parameters(args): Parameters<PortsArgs>) -> CallToolResult {
        self.invoke(
            "get_ports",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::LIST_LISTENERS, annotations(read_only_hint = true))]
    async fn list_listeners(&self, Parameters(args): Parameters<ListenersArgs>) -> CallToolResult {
        self.invoke(
            "list_listeners",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::LIST_TOOLS, annotations(read_only_hint = true))]
    async fn list_tools(&self, Parameters(args): Parameters<ListToolsArgs>) -> CallToolResult {
        self.invoke(
            "list_tools",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::CALL_TOOL_API, annotations(read_only_hint = false, destructive_hint = true, open_world_hint = true))]
    async fn call_tool_api(&self, Parameters(args): Parameters<CallApiArgs>) -> CallToolResult {
        self.invoke(
            "call_tool_api",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }

    #[tool(description = descriptions::GET_JOB_RESULT, annotations(read_only_hint = true))]
    async fn get_job_result(&self, Parameters(args): Parameters<JobArgs>) -> CallToolResult {
        self.invoke(
            "get_job_result",
            serde_json::to_value(args).expect("MCP arguments serialize"),
        )
        .await
    }
}
