//! Facade-backed tool operations and target selection.

use std::time::Duration;

use lantern_core::{
    api::{CreateInstanceRequest, LogParams, RunTarget},
    ids::{InstanceId, ProjectId, branch_slug},
    model::{ProjectKind, Stream},
};
use rmcp::model::{CallToolResult, ContentBlock};
use serde::Serialize;
use serde_json::{Value, json};

use super::LanternMcp;
use crate::api::facade::{InstanceFilter, ListenerFilter, OperationTicket, ProjectFilter};

type ToolResult = Result<Value, String>;
fn json_value<T: Serialize>(value: T) -> ToolResult {
    serde_json::to_value(value).map_err(|e| e.to_string())
}
fn field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}
fn num(v: &Value, key: &str, default: u32, max: u32) -> u32 {
    v.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(default)
        .min(max)
}
fn failure(e: lantern_core::api::ApiError) -> String {
    format!("{}: {}. Try list_projects or get_project.", e.code, e.message)
}
fn safe_branch(branch: &str) -> bool {
    !branch.is_empty()
        && branch.len() <= 200
        && !branch.starts_with('-')
        && !branch.contains("..")
        && branch
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
}
fn redact(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, v) in m.iter_mut() {
                if k == "env" || k == "run_env" {
                    if let Value::Object(env) = v {
                        for (key, val) in env.iter_mut() {
                            let key = key.to_ascii_lowercase();
                            if ["key", "token", "secret", "passw", "credential", "auth"]
                                .iter()
                                .any(|s| key.contains(s))
                            {
                                *val = Value::String("***".into());
                            }
                        }
                    }
                } else {
                    redact(v);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(redact),
        _ => {}
    }
}

impl LanternMcp {
    pub(super) async fn project_id(&self, name: &str) -> Result<ProjectId, String> {
        let projects = self
            .facade
            .projects(ProjectFilter {
                enabled: Some("any".into()),
                ..Default::default()
            })
            .await
            .map_err(failure)?;
        let found = projects
            .iter()
            .find(|p| p.project.id.as_str() == name)
            .or_else(|| {
                projects.iter().find(|p| {
                    p.project.name.eq_ignore_ascii_case(name)
                        || p.project.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
                })
            });
        found.map(|p| p.project.id.clone()).ok_or_else(|| {
            format!(
                "Project '{name}' not found. Available: {}. Try list_projects.",
                projects
                    .iter()
                    .map(|p| p.project.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    }
    pub(super) async fn target(
        &self,
        name: &str,
        instance: Option<&str>,
        branch: Option<&str>,
    ) -> Result<InstanceId, String> {
        if let Some(raw) = instance.or_else(|| name.contains('@').then_some(name)) {
            let id = InstanceId::parse(raw).map_err(|e| format!("{e}. Try list_instances."))?;
            let view = self.facade.instance(id.clone()).await.map_err(failure)?;
            if !name.contains('@') && !name.is_empty() && self.project_id(name).await? != view.project {
                return Err(
                    "instance_mismatch: instance belongs to another project. Try list_instances.".into(),
                );
            }
            return Ok(id);
        }
        let project = self.project_id(name).await?;
        if let Some(branch) = branch {
            let instances = self
                .facade
                .project_instances(project.clone())
                .await
                .map_err(failure)?;
            if let Some(found) = instances.iter().find(|v| {
                v.checkout.branch.as_deref() == Some(branch)
                    || v.id.slug() == Some(branch_slug(branch).as_str())
            }) {
                return Ok(found.id.clone());
            }
            return Err(format!(
                "No instance of '{project}' for branch '{branch}'. Existing: {}. To run it, call start_branch.",
                instances
                    .iter()
                    .map(|v| format!(
                        "{} ({})",
                        v.id,
                        v.checkout.branch.as_deref().unwrap_or("detached")
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let id = InstanceId::primary(&project);
        self.facade.instance(id.clone()).await.map_err(failure)?;
        Ok(id)
    }
    async fn target_args(&self, v: &Value) -> Result<InstanceId, String> {
        self.target(
            field(v, "name").unwrap_or_default(),
            field(v, "instance"),
            field(v, "branch"),
        )
        .await
    }
    pub(super) async fn invoke(&self, name: &str, args: Value) -> CallToolResult {
        let limit = match name {
            "call_tool_api" => num(&args, "timeout_seconds", 50, 300) + 5,
            "refresh_discovery" => 30,
            "start_project" | "stop_project" | "restart_project" | "start_branch" | "get_operation" => 60,
            _ => 10,
        };
        let outcome =
            tokio::time::timeout(Duration::from_secs(u64::from(limit)), self.dispatch(name, &args)).await;
        match outcome {
            Ok(Ok(value)) => {
                let mut text = match value {
                    Value::String(s) => s,
                    value => value.to_string(),
                };
                if text.chars().count() > 40_000 {
                    text = text.chars().take(40_000).collect::<String>();
                    text.push_str("\n[truncated; narrow the request or page the results]");
                }
                CallToolResult::success(vec![ContentBlock::text(text)])
            }
            Ok(Err(e)) => CallToolResult::error(vec![ContentBlock::text(e)]),
            Err(_) => CallToolResult::error(vec![ContentBlock::text(
                "Timed out. Any operation already started continues; use get_operation.",
            )]),
        }
    }
    async fn ticket(&self, ticket: OperationTicket, id: &InstanceId, _wait: u32) -> ToolResult {
        // The facade does not yet expose the shared tracker. Return the ticket immediately;
        // integration must supply operation lookup and bounded waits through that facade.
        Ok(
            json!({"status":"in_progress", "operation_id":ticket.id, "instance":id, "message":"Use get_operation to check progress."}),
        )
    }
    async fn dispatch(&self, name: &str, a: &Value) -> ToolResult {
        match name {
            "list_projects" | "search_projects" | "list_tools" => {
                let kind = if name == "list_tools" {
                    None
                } else {
                    field(a, "kind").map(str::to_owned)
                };
                let projects = self
                    .facade
                    .projects(ProjectFilter {
                        tag: field(a, "tag").map(str::to_owned),
                        kind,
                        q: field(a, "query").map(str::to_owned),
                        enabled: Some("any".into()),
                    })
                    .await
                    .map_err(failure)?;
                let mut out = Vec::new();
                for p in projects {
                    if name == "list_tools"
                        && !matches!(p.project.kind, ProjectKind::Tool | ProjectKind::Capability)
                    {
                        continue;
                    }
                    if let Some(kind) = field(a, "kind")
                        && name == "list_tools"
                        && serde_json::to_value(p.project.kind)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .as_deref()
                            != Some(kind)
                    {
                        continue;
                    }
                    if let Some(status) = field(a, "status")
                        && serde_json::to_value(p.status)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .as_deref()
                            != Some(status)
                    {
                        continue;
                    }
                    out.push(json!({"id":p.project.id,"name":p.project.name,"path":p.project.root,"domain":p.url.trim_start_matches("https://").trim_start_matches("http://"),"url":p.url,"kind":p.project.kind,"status":p.status,"port":p.port,"tags":p.project.tags,"enabled":p.project.enabled,"instances":p.instance_count,"docs_count":p.docs_count,"endpoints_count":p.endpoints_count,"routing":p.routing}));
                }
                json_value(out)
            }
            "list_instances" => {
                let project = match field(a, "project") {
                    Some(p) => Some(self.project_id(p).await?),
                    None => None,
                };
                let mut items = self
                    .facade
                    .instances(InstanceFilter {
                        project,
                        status: field(a, "status").map(str::to_owned),
                        ..Default::default()
                    })
                    .await
                    .map_err(failure)?;
                if let Some(branch) = field(a, "branch") {
                    items.retain(|v| v.checkout.branch.as_deref() == Some(branch));
                }
                json_value(items)
            }
            "get_project" => {
                let id = self.target_args(a).await?;
                let detail = self.facade.project(id.project()).await.map_err(failure)?;
                let view = self.facade.instance(id.clone()).await.map_err(failure)?;
                let mut manifest = self.facade.manifest(id).await.map_err(failure)?;
                redact(&mut manifest);
                Ok(json!({"project":detail,"instance":view,"manifest":manifest}))
            }
            "start_project" | "stop_project" | "restart_project" => {
                let id = self.target_args(a).await?;
                let ticket = match name {
                    "start_project" => self.facade.start(id.clone()).await,
                    "stop_project" => self.facade.stop(id.clone()).await,
                    _ => self.facade.restart(id.clone()).await,
                }
                .map_err(failure)?;
                self.ticket(
                    ticket,
                    &id,
                    num(
                        a,
                        "wait_seconds",
                        if name == "start_project" {
                            30
                        } else if name == "stop_project" {
                            20
                        } else {
                            40
                        },
                        55,
                    ),
                )
                .await
            }
            "start_branch" => {
                let branch = field(a, "branch").unwrap_or_default();
                if !safe_branch(branch) || field(a, "base").is_some_and(|b| !safe_branch(b)) {
                    return Err("Invalid branch or base. Try list_instances.".into());
                }
                let project = self.project_id(field(a, "name").unwrap_or_default()).await?;
                let target = match field(a, "base") {
                    Some(base) => RunTarget::NewBranch {
                        name: branch.into(),
                        base: Some(base.into()),
                    },
                    None => RunTarget::Branch(branch.into()),
                };
                let ticket = self
                    .facade
                    .run_branch(
                        project.clone(),
                        CreateInstanceRequest {
                            target,
                            fetch: true,
                            start: a.get("start").and_then(Value::as_bool).unwrap_or(true),
                            pin: None,
                        },
                    )
                    .await
                    .map_err(failure)?;
                let id = ticket
                    .instance
                    .as_ref()
                    .map(|v| v.id.clone())
                    .unwrap_or_else(|| InstanceId::primary(&project));
                self.ticket(ticket, &id, num(a, "wait_seconds", 45, 55)).await
            }
            "get_operation" => {
                self.job_result(field(a, "operation_id").unwrap_or_default(), false)
                    .await
            }
            "get_job_result" => {
                self.job_result(field(a, "job_id").unwrap_or_default(), true)
                    .await
            }
            "get_project_logs" => {
                let id = self.target_args(a).await?;
                let stream = match field(a, "stream").unwrap_or("all") {
                    "all" => None,
                    "stdout" => Some(Stream::Stdout),
                    "stderr" => Some(Stream::Stderr),
                    "system" => Some(Stream::System),
                    "setup" => Some(Stream::Setup),
                    _ => return Err("Invalid stream. Try get_project_logs with stream=all.".into()),
                };
                let page = self
                    .facade
                    .logs(
                        id.clone(),
                        LogParams {
                            tail: Some(num(a, "tail", 200, 2000)),
                            since: field(a, "since").map(str::to_owned),
                            stream,
                            grep: field(a, "grep").map(str::to_owned),
                            format: None,
                        },
                    )
                    .await
                    .map_err(failure)?;
                let mut text = format!("# {}: {} of {} lines\n", id, page.lines.len(), page.buffered);
                for line in page.lines {
                    if line.stream == Stream::Stderr {
                        text.push_str("[stderr] ");
                    }
                    text.push_str(&line.line);
                    text.push('\n');
                }
                Ok(Value::String(text))
            }
            "get_project_docs" => {
                let id = self.target_args(a).await?;
                if let Some(path) = field(a, "path") {
                    json_value(
                        self.facade
                            .doc(
                                id,
                                path.into(),
                                Some(u64::from(num(a, "offset", 0, u32::MAX))),
                                Some(u64::from(num(a, "max_chars", 40_000, 40_000))),
                            )
                            .await
                            .map_err(failure)?,
                    )
                } else {
                    json_value(self.facade.docs(id).await.map_err(failure)?)
                }
            }
            "get_project_endpoints" => {
                let id = self.target_args(a).await?;
                json_value(
                    self.facade
                        .endpoints(
                            id,
                            field(a, "q").map(str::to_owned),
                            Some(num(a, "limit", 200, 2000)),
                        )
                        .await
                        .map_err(failure)?,
                )
            }
            "get_project_discovery" => {
                let id = self.target_args(a).await?;
                json_value(self.facade.intel(id, None).await.map_err(failure)?)
            }
            "refresh_discovery" => {
                if field(a, "name").is_some() || field(a, "instance").is_some() {
                    let id = self.target_args(a).await?;
                    json_value(self.facade.refresh_intel(id).await.map_err(failure)?)
                } else {
                    json_value(self.facade.refresh_all_intel().await.map_err(failure)?)
                }
            }
            "check_health" => {
                if field(a, "name").is_some() || field(a, "instance").is_some() {
                    let id = self.target_args(a).await?;
                    if a.get("refresh").and_then(Value::as_bool).unwrap_or(false) {
                        json_value(self.facade.check_health(id).await.map_err(failure)?)
                    } else {
                        json_value(self.facade.instance(id).await.map_err(failure)?.runtime.health)
                    }
                } else {
                    json_value(self.facade.system_health().await.map_err(failure)?)
                }
            }
            "get_dependencies" => {
                let project = match field(a, "name") {
                    Some(n) => Some(self.project_id(n).await?),
                    None => None,
                };
                json_value(self.facade.dependencies(project).await.map_err(failure)?)
            }
            "get_ports" => {
                let project = match field(a, "project") {
                    Some(n) => Some(self.project_id(n).await?),
                    None => None,
                };
                json_value(self.facade.ports(project).await.map_err(failure)?.instances)
            }
            "list_listeners" => {
                let project = match field(a, "project") {
                    Some(n) => Some(self.project_id(n).await?),
                    None => None,
                };
                let filter = ListenerFilter {
                    project,
                    port: a
                        .get("port")
                        .and_then(Value::as_u64)
                        .and_then(|n| u16::try_from(n).ok()),
                    matched: a
                        .get("unmatched_only")
                        .and_then(Value::as_bool)
                        .filter(|b| *b)
                        .map(|_| false),
                    include_hidden: None,
                };
                json_value(self.facade.listeners(filter).await.map_err(failure)?)
            }
            "call_tool_api" => self.call_api(a).await,
            _ => Err("Unknown tool. Try list_projects.".into()),
        }
    }
}
