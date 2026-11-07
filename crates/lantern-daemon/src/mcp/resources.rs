//! MCP resources and prompt templates.

use rmcp::{
    ErrorData, RoleServer,
    handler::server::wrapper::Parameters,
    model::{
        ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams, PromptMessage,
        ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource, ResourceContents,
        ResourceTemplate, Role,
    },
    prompt, prompt_router,
    service::RequestContext,
};
use serde_json::{Value, json};

use super::{
    LanternMcp,
    args::{DependencyPromptArgs, PromptArgs},
};
use crate::api::facade::{InstanceFilter, ListenerFilter, ProjectFilter};

#[prompt_router(vis = "pub(super)")]
impl LanternMcp {
    #[prompt(
        description = "Diagnose an unhealthy service using health, details, logs, dependencies and listeners."
    )]
    async fn diagnose_service(&self, Parameters(args): Parameters<PromptArgs>) -> Vec<PromptMessage> {
        let target = args
            .instance
            .map(|v| format!(" and instance '{v}'"))
            .unwrap_or_default();
        vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Please diagnose the service '{}'{}: check_health, get_project, get_project_logs and get_dependencies. If the port looks wrong, call get_ports or list_listeners. Identify the likely cause, suggest specific remediation, note downstream impact, and suggest relevant deploy commands.",
                args.name, target
            ),
        )]
    }
    #[prompt(description = "Trace a project's dependencies and assess its blast radius.")]
    async fn dependency_trace(
        &self,
        Parameters(args): Parameters<DependencyPromptArgs>,
    ) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Trace the dependency chain for project '{}': use get_dependencies, get_project for dependents, and check_health for all related projects. Summarize its direct dependencies, projects that depend on it, health, and the blast radius if it fails.",
                args.name
            ),
        )]
    }
}

impl LanternMcp {
    pub(super) async fn resource_list(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(
            [
                ("lantern:///projects", "projects"),
                ("lantern:///projects/docs", "project_docs"),
                ("lantern:///projects/discovery", "project_discovery"),
                ("lantern:///instances", "instances"),
                ("lantern:///listeners", "listeners"),
            ]
            .into_iter()
            .map(|(uri, name)| Resource::new(uri, name).with_mime_type("application/json"))
            .collect(),
        ))
    }
    pub(super) async fn resource_templates(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new("lantern:///projects/{name}/docs", "project_docs"),
            ResourceTemplate::new("lantern:///projects/{name}/docs/{+path}", "project_doc"),
        ]))
    }
    async fn resource_docs(&self, name: &str) -> Result<Value, String> {
        let id = self.target(name, None, None).await?;
        let docs = self.facade.docs(id.clone()).await.map_err(|e| e.message)?;
        let mut out = Vec::new();
        for doc in docs.into_iter().filter(|d| d.exists) {
            if let Ok(content) = self
                .facade
                .doc(id.clone(), doc.path.clone(), Some(0), Some(65_536))
                .await
            {
                out.push(
                    json!({"path":doc.path,"kind":doc.kind,"source":doc.source,"content":content.content}),
                );
                if serde_json::to_vec(&out).map_or(true, |v| v.len() > 1_000_000) {
                    out.pop();
                    break;
                }
            }
        }
        Ok(json!({"name":name,"docs":out}))
    }
    pub(super) async fn resource_read(
        &self,
        req: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let uri = req.uri;
        let not_found = || ErrorData::resource_not_found("resource_not_found", Some(json!({"uri":uri})));
        let map_error = |e: lantern_core::api::ApiError| ErrorData::internal_error(e.message, None);
        let (value, mime) = match uri.as_str() {
            "lantern:///projects" => (
                serde_json::to_value(
                    self.facade
                        .projects(ProjectFilter {
                            enabled: Some("any".into()),
                            ..Default::default()
                        })
                        .await
                        .map_err(map_error)?,
                )
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?,
                "application/json",
            ),
            "lantern:///instances" => (
                serde_json::to_value(
                    self.facade
                        .instances(InstanceFilter::default())
                        .await
                        .map_err(map_error)?,
                )
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?,
                "application/json",
            ),
            "lantern:///listeners" => (
                serde_json::to_value(
                    self.facade
                        .listeners(ListenerFilter::default())
                        .await
                        .map_err(map_error)?,
                )
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?,
                "application/json",
            ),
            "lantern:///projects/docs" | "lantern:///projects/discovery" => {
                let projects = self
                    .facade
                    .projects(ProjectFilter {
                        enabled: Some("any".into()),
                        ..Default::default()
                    })
                    .await
                    .map_err(map_error)?;
                let mut result = Vec::new();
                for project in projects {
                    let item = if uri.ends_with("/docs") {
                        self.resource_docs(project.project.id.as_str()).await.ok()
                    } else {
                        self.facade
                            .intel(project.primary, None)
                            .await
                            .ok()
                            .and_then(|s| serde_json::to_value(s).ok())
                    };
                    if let Some(item) = item {
                        result.push(item);
                        if serde_json::to_vec(&result).map_or(true, |v| v.len() > 1_000_000) {
                            result.pop();
                            break;
                        }
                    }
                }
                (json!(result), "application/json")
            }
            _ => {
                let rest = uri.strip_prefix("lantern:///projects/").ok_or_else(not_found)?;
                let (raw_name, suffix) = rest.split_once("/docs").ok_or_else(not_found)?;
                let name = percent_encoding::percent_decode_str(raw_name)
                    .decode_utf8()
                    .map_err(|_| not_found())?;
                if suffix.is_empty() {
                    (
                        self.resource_docs(&name).await.map_err(|_| not_found())?,
                        "application/json",
                    )
                } else {
                    let path = suffix.strip_prefix('/').ok_or_else(not_found)?;
                    let path = percent_encoding::percent_decode_str(path)
                        .decode_utf8()
                        .map_err(|_| not_found())?;
                    let id = self.target(&name, None, None).await.map_err(|_| not_found())?;
                    let doc = self
                        .facade
                        .doc(id, path.to_string(), Some(0), Some(65_536))
                        .await
                        .map_err(|_| not_found())?;
                    (
                        Value::String(doc.content),
                        if path.ends_with(".md") {
                            "text/markdown"
                        } else {
                            "text/plain"
                        },
                    )
                }
            }
        };
        let text = if mime == "application/json" {
            value.to_string()
        } else {
            value.as_str().unwrap_or_default().to_owned()
        };
        Ok(ReadResourceResult::new(vec![ResourceContents::text(text, uri).with_mime_type(mime)]).into())
    }
}
