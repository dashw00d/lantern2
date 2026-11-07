//! MCP input schemas.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

fn number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    v.map(|v| match v {
        serde_json::Value::String(s) => s.parse().map_err(serde::de::Error::custom),
        serde_json::Value::Number(n) => n
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| serde::de::Error::custom("expected unsigned integer")),
        _ => Err(serde::de::Error::custom("expected unsigned integer")),
    })
    .transpose()
}

fn port_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u16>, D::Error> {
    number(d)?
        .map(|n| u16::try_from(n).map_err(serde::de::Error::custom))
        .transpose()
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListProjectsArgs {
    /// Filter by tag.
    pub tag: Option<String>,
    /// Filter by project kind.
    pub kind: Option<String>,
    /// Filter by primary status.
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchProjectsArgs {
    /// Search name, description, tags or branch.
    pub query: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SetupProjectArgs {
    /// Absolute path of the project folder (where lantern.yaml is or should go).
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TargetArgs {
    /// Project name or id; an instance id also works.
    pub name: String,
    /// Existing instance id; takes precedence over branch.
    pub instance: Option<String>,
    /// Branch of an existing instance; omit for primary.
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListInstancesArgs {
    /// Restrict to project.
    pub project: Option<String>,
    /// Filter by status.
    pub status: Option<String>,
    /// Filter by branch.
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LifecycleArgs {
    /// Project name or id; an instance id also works.
    pub name: String,
    /// Existing instance id; takes precedence over branch.
    pub instance: Option<String>,
    /// Branch of an existing instance; omit for primary.
    pub branch: Option<String>,
    /// Seconds to wait; maximum 55.
    #[serde(default, deserialize_with = "number")]
    pub wait_seconds: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StartBranchArgs {
    /// Project name or id.
    pub name: String,
    /// Git branch to run.
    pub branch: String,
    /// Base for a new branch.
    pub base: Option<String>,
    /// Start the server; defaults to true.
    pub start: Option<bool>,
    /// Seconds to wait; default 45, maximum 55.
    #[serde(default, deserialize_with = "number")]
    pub wait_seconds: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GetOperationArgs {
    /// Operation id returned by a long-running tool.
    pub operation_id: String,
    /// Seconds to wait; maximum 55.
    #[serde(default, deserialize_with = "number")]
    pub wait_seconds: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GetLogsArgs {
    /// Project name or id; an instance id also works.
    pub name: String,
    /// Existing instance id; takes precedence over branch.
    pub instance: Option<String>,
    /// Branch of an existing instance; omit for primary.
    pub branch: Option<String>,
    /// Number of lines; default 200, maximum 2000.
    #[serde(default, deserialize_with = "number")]
    pub tail: Option<u32>,
    /// Time or duration, such as 15m.
    pub since: Option<String>,
    /// stdout, stderr, or all.
    pub stream: Option<String>,
    /// Case-insensitive text filter.
    pub grep: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GetDocsArgs {
    /// Project name or id; an instance id also works.
    pub name: String,
    /// Existing instance id; takes precedence over branch.
    pub instance: Option<String>,
    /// Branch of an existing instance; omit for primary.
    pub branch: Option<String>,
    /// Listed document path; omit to list.
    pub path: Option<String>,
    /// Character offset.
    #[serde(default, deserialize_with = "number")]
    pub offset: Option<u32>,
    /// Maximum characters; default 40000.
    #[serde(default, deserialize_with = "number")]
    pub max_chars: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GetEndpointsArgs {
    /// Project name or id; an instance id also works.
    pub name: String,
    /// Existing instance id; takes precedence over branch.
    pub instance: Option<String>,
    /// Branch of an existing instance; omit for primary.
    pub branch: Option<String>,
    /// Filter by path, description, or category.
    pub q: Option<String>,
    /// Maximum endpoints; default 200.
    #[serde(default, deserialize_with = "number")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OptionalTargetArgs {
    /// Project name or id; omit for all.
    pub name: Option<String>,
    /// Existing instance id.
    pub instance: Option<String>,
    /// Existing branch.
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HealthArgs {
    /// Project name or id; omit for all.
    pub name: Option<String>,
    /// Existing instance id.
    pub instance: Option<String>,
    /// Existing branch.
    pub branch: Option<String>,
    /// Run a new health check.
    pub refresh: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DependencyArgs {
    /// Project name; omit for all.
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PortsArgs {
    /// Restrict to a project.
    pub project: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListenersArgs {
    /// Restrict to a project.
    pub project: Option<String>,
    /// Restrict to a port.
    #[serde(default, deserialize_with = "port_number")]
    pub port: Option<u16>,
    /// Only listeners with no matching instance.
    pub unmatched_only: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListToolsArgs {
    /// Filter by tag.
    pub tag: Option<String>,
    /// Filter by status.
    pub status: Option<String>,
    /// Filter by tool or capability.
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallApiArgs {
    /// Registered project name or id.
    pub tool: String,
    /// Existing instance id.
    pub instance: Option<String>,
    /// Existing branch.
    pub branch: Option<String>,
    /// GET, POST, PUT, PATCH or DELETE; default GET.
    pub method: Option<String>,
    /// Absolute path on the registered server, optionally with query.
    pub path: String,
    /// JSON string, object or array.
    #[schemars(with = "Option<BodySchema>")]
    pub body: Option<serde_json::Value>,
    /// Run as a job.
    pub r#async: Option<bool>,
    /// Timeout seconds; default 50, maximum 300.
    #[serde(default, deserialize_with = "number")]
    pub timeout_seconds: Option<u32>,
    /// Response cap; default 100000, maximum 1000000.
    #[serde(default, deserialize_with = "number")]
    pub max_bytes: Option<u32>,
    /// True only after user approval for a risky call.
    pub confirm: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JobArgs {
    /// Job id returned by call_tool_api.
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PromptArgs {
    /// Project to diagnose.
    pub name: String,
    /// Existing instance id.
    pub instance: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DependencyPromptArgs {
    /// Project whose dependencies to trace.
    pub name: String,
}

// Schema-only: request bodies are forwarded as strings, JSON objects, or arrays.
#[allow(dead_code)]
#[derive(schemars::JsonSchema)]
#[serde(untagged)]
enum BodySchema {
    String(String),
    Object(std::collections::BTreeMap<String, serde_json::Value>),
    Array(Vec<serde_json::Value>),
}
