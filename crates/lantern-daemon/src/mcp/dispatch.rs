//! Facade-backed tool operations and target selection.

use std::time::Duration;

use lantern_core::{
    api::{CreateInstanceRequest, LogParams, RunTarget},
    ids::{InstanceId, ProjectId, branch_slug},
    model::{HealthState, HealthSummary, ProjectKind, Stream},
};
use rmcp::{
    RoleServer,
    model::{CallToolResult, ContentBlock, ProgressNotificationParam},
    service::RequestContext,
};
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
fn discovery_value(id: &InstanceId, snapshot: &lantern_core::api::IntelSnapshot, manifest: &Value) -> Value {
    json!({"instance":id,"docs_auto":manifest.get("docs_auto"),"api_auto":manifest.get("api_auto"),"discovered_docs":snapshot.docs.iter().filter(|d|d.source == lantern_core::api::IntelSource::Discovered).collect::<Vec<_>>(),"discovered_endpoints":snapshot.endpoints.iter().filter(|d|d.source == lantern_core::api::IntelSource::Discovered).collect::<Vec<_>>(),"docs_available":snapshot.docs,"endpoints_available":snapshot.endpoints,"discovery":snapshot.discovery,"stale":snapshot.stale})
}
fn health_value(health: HealthSummary) -> Value {
    let status = match health.state {
        HealthState::Healthy => "healthy",
        HealthState::Failing => "unhealthy",
        HealthState::Unknown => "unknown",
    };
    json!({"status":status,"latency_ms":health.latency_ms,"checked_at":health.checked_at,"error":health.last_error,"history":health.recent})
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
            let named_project = if name.contains('@') {
                Some(
                    InstanceId::parse(name)
                        .map_err(|e| format!("{e}. Try list_instances."))?
                        .project(),
                )
            } else if !name.is_empty() {
                Some(self.project_id(name).await?)
            } else {
                None
            };
            if named_project.is_some_and(|project| project != view.project) {
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
            if let Some(found) = instances
                .iter()
                .find(|v| v.checkout.branch.as_deref() == Some(branch))
                .or_else(|| {
                    instances
                        .iter()
                        .find(|v| v.id.slug() == Some(branch_slug(branch).as_str()))
                })
            {
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
    pub(super) async fn invoke(
        &self,
        name: &str,
        args: Value,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let limit = match name {
            "call_tool_api" => num(&args, "timeout_seconds", 50, 300) + 5,
            "refresh_discovery" => 30,
            "start_project" | "stop_project" | "restart_project" | "start_branch" | "get_operation" => 60,
            _ => 10,
        };
        let started = tokio::time::Instant::now();
        let token = ctx.meta.get_progress_token();
        let mut ticker = tokio::time::interval_at(started + Duration::from_secs(3), Duration::from_secs(3));
        let work = tokio::time::timeout(Duration::from_secs(u64::from(limit)), self.dispatch(name, &args));
        tokio::pin!(work);
        let outcome = loop {
            tokio::select! {
                result = &mut work => break result,
                _ = ctx.ct.cancelled() => return CallToolResult::error(vec![ContentBlock::text("Cancelled by the client. Any operation already started continues; use get_operation.")]),
                _ = ticker.tick(), if token.is_some() => {
                    let elapsed = started.elapsed().as_secs_f64();
                    if let Some(token) = token.clone() {
                        let progress = ProgressNotificationParam::new(token, elapsed).with_total(f64::from(limit)).with_message(format!("Working ({}s)", elapsed as u64));
                        let _ = ctx.peer.notify_progress(progress).await;
                    }
                }
            }
        };
        match outcome {
            Ok(Ok(value)) => {
                if let Some(image) = value.get("$lantern_image")
                    && let (Some(data), Some(mime)) = (
                        image.get("data").and_then(Value::as_str),
                        image.get("mime").and_then(Value::as_str),
                    )
                {
                    return CallToolResult::success(vec![ContentBlock::image(data, mime)]);
                }
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
            Ok(Err(e)) => {
                let mut message: String = e.chars().take(39_800).collect();
                if e.chars().count() > 39_800 {
                    message.push_str(" [truncated]");
                }
                if !message.contains("Try ") && !message.contains("repeat with confirm=true") {
                    message.push_str(" Try get_project_endpoints or list_projects.");
                }
                CallToolResult::error(vec![ContentBlock::text(message)])
            }
            Err(_) => CallToolResult::error(vec![ContentBlock::text(
                "Timed out. Any operation already started continues; use get_operation.",
            )]),
        }
    }
    async fn ticket(&self, ticket: OperationTicket, id: Option<&InstanceId>, _wait: u32) -> ToolResult {
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
                        q: None,
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
                            != Some(if status == "error" { "crashed" } else { status })
                    {
                        continue;
                    }
                    let manifest = self.facade.manifest(p.primary.clone()).await.ok();
                    let description = manifest
                        .as_ref()
                        .and_then(|m| m.get("description"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    let branches: Vec<String> = self
                        .facade
                        .project_instances(p.project.id.clone())
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|v| v.checkout.branch)
                        .collect();
                    if let Some(query) = field(a, "query") {
                        let q = query.to_lowercase();
                        let matches = p.project.id.as_str().to_lowercase().contains(&q)
                            || p.project.name.to_lowercase().contains(&q)
                            || description
                                .as_str()
                                .is_some_and(|d| d.to_lowercase().contains(&q))
                            || p.project.tags.iter().any(|t| t.to_lowercase().contains(&q))
                            || branches.iter().any(|b| b.to_lowercase().contains(&q));
                        if !matches {
                            continue;
                        }
                    }
                    let routing = p.routing.clone();
                    out.push(json!({"id":p.project.id,"name":p.project.name,"path":p.project.root,"domain":p.url.trim_start_matches("https://").trim_start_matches("http://"),"url":p.url,"kind":p.project.kind,"status":p.status,"port":p.port,"description":description,"tags":p.project.tags,"enabled":p.project.enabled,"instances":p.instance_count,"branches":branches,"docs_count":p.docs_count,"endpoints_count":p.endpoints_count,"risk":routing.as_ref().and_then(|r|r.risk),"requires_confirmation":routing.as_ref().is_some_and(|r|r.requires_confirmation),"triggers":routing.as_ref().map(|r|r.triggers.clone()).unwrap_or_default(),"routing":routing}));
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
                let mut manifest = self.facade.manifest(id.clone()).await.map_err(failure)?;
                redact(&mut manifest);
                let docs = self.facade.docs(id.clone()).await.unwrap_or_default();
                let endpoints = self
                    .facade
                    .endpoints(id.clone(), None, Some(200))
                    .await
                    .unwrap_or_default();
                let discovery = self
                    .facade
                    .intel(id.clone(), None)
                    .await
                    .ok()
                    .map(|s| s.discovery);
                let mut out = serde_json::to_value(&detail).map_err(|e| e.to_string())?;
                if let Some(object) = out.as_object_mut() {
                    object.insert("path".into(), json!(detail.summary.project.root));
                    object.insert("domain".into(), json!(view.domain));
                    object.insert("url".into(), json!(view.url));
                    object.insert("status".into(), json!(view.runtime.status));
                    object.insert("port".into(), json!(view.runtime.port));
                    object.insert("primary_instance".into(), json!(detail.summary.primary));
                    object.insert("instance".into(), json!(view));
                    for key in [
                        "description",
                        "health_endpoint",
                        "base_url",
                        "upstream_url",
                        "routing",
                    ] {
                        if let Some(value) = manifest.get(key) {
                            object.insert(key.into(), value.clone());
                        }
                    }
                    object.insert(
                        "run_cmd".into(),
                        manifest.pointer("/run/cmd").cloned().unwrap_or(Value::Null),
                    );
                    object.insert(
                        "run_env".into(),
                        manifest.pointer("/run/env").cloned().unwrap_or_else(|| json!({})),
                    );
                    object.insert("docs_available".into(), json!(docs));
                    object.insert("endpoints_available".into(), json!(endpoints));
                    object.insert("discovery".into(), json!(discovery));
                    object.insert("manifest".into(), manifest);
                }
                Ok(out)
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
                    Some(&id),
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
                let id = ticket.instance.as_ref().map(|v| v.id.clone());
                self.ticket(ticket, id.as_ref(), num(a, "wait_seconds", 45, 55))
                    .await
            }
            "get_operation" => {
                let id = field(a, "operation_id").unwrap_or_default();
                if !id.starts_with("mcp-job-") {
                    return Err("Lifecycle operation lookup is unavailable until the daemon facade is wired to its operation tracker. The operation continues independently.".into());
                }
                self.job_result(id, false).await
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
                let snapshot = self.facade.intel(id.clone(), None).await.map_err(failure)?;
                let manifest = self.facade.manifest(id.clone()).await.map_err(failure)?;
                Ok(discovery_value(&id, &snapshot, &manifest))
            }
            "refresh_discovery" => {
                if field(a, "name").is_some() || field(a, "instance").is_some() {
                    let id = self.target_args(a).await?;
                    let snapshot = self.facade.refresh_intel(id.clone()).await.map_err(failure)?;
                    let manifest = self.facade.manifest(id.clone()).await.map_err(failure)?;
                    Ok(discovery_value(&id, &snapshot, &manifest))
                } else {
                    let report = self.facade.refresh_all_intel().await.map_err(failure)?;
                    if !report.errors.is_empty() {
                        return json_value(report);
                    }
                    let projects = self
                        .facade
                        .projects(ProjectFilter {
                            enabled: Some("any".into()),
                            ..Default::default()
                        })
                        .await
                        .map_err(failure)?;
                    let mut result = Vec::new();
                    for project in projects {
                        let id = project.primary;
                        let snapshot = self.facade.intel(id.clone(), None).await.map_err(failure)?;
                        let manifest = self.facade.manifest(id.clone()).await.map_err(failure)?;
                        result.push(discovery_value(&id, &snapshot, &manifest));
                    }
                    json_value(result)
                }
            }
            "check_health" => {
                if field(a, "name").is_some() || field(a, "instance").is_some() {
                    let id = self.target_args(a).await?;
                    let health = if a.get("refresh").and_then(Value::as_bool).unwrap_or(false) {
                        self.facade.check_health(id).await.map_err(failure)?
                    } else {
                        self.facade.instance(id).await.map_err(failure)?.runtime.health
                    };
                    Ok(health_value(health))
                } else {
                    let system = self.facade.system_health().await.map_err(failure)?;
                    let checks = |name: &str| {
                        system
                            .checks
                            .iter()
                            .find(|c| c.id.starts_with(name))
                            .map(|c| json!(c))
                            .unwrap_or(Value::Null)
                    };
                    let instances = self
                        .facade
                        .instances(InstanceFilter::default())
                        .await
                        .map_err(failure)?;
                    let mut projects = serde_json::Map::new();
                    for instance in instances {
                        let health = if a.get("refresh").and_then(Value::as_bool).unwrap_or(false) {
                            self.facade
                                .check_health(instance.id.clone())
                                .await
                                .map_err(failure)?
                        } else {
                            instance.runtime.health
                        };
                        projects.insert(instance.id.to_string(), health_value(health));
                    }
                    Ok(
                        json!({"dns":checks("dns"),"caddy":checks("caddy"),"tls":checks("tls"),"daemon":checks("daemon"),"projects":projects}),
                    )
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
                let ports = self.facade.ports(project.clone()).await.map_err(failure)?;
                let listeners = self
                    .facade
                    .listeners(ListenerFilter {
                        project,
                        ..Default::default()
                    })
                    .await
                    .unwrap_or_default();
                let mut result = serde_json::Map::new();
                for (id, port) in ports.instances {
                    let mut value = serde_json::to_value(port).map_err(|e| e.to_string())?;
                    if let Some(listener) = listeners.iter().find(|l| l.matched.as_ref() == Some(&id))
                        && let Some(object) = value.as_object_mut()
                    {
                        object.insert(
                            "source".into(),
                            json!(if listener.managed { "managed" } else { "external" }),
                        );
                        object.insert("pid".into(), json!(listener.pid));
                        object.insert("branch".into(), json!(listener.branch));
                    }
                    result.insert(id.to_string(), value);
                }
                Ok(Value::Object(result))
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
