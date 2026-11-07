//! REST routing, request decoding, and operation responses.

use super::{
    ApiState,
    error::HttpError,
    facade::{FacadeResult, InstanceFilter, ListenerFilter, OperationTicket, ProjectFilter, ServiceAction},
    sse,
};
use axum::{
    body::{Bytes, to_bytes},
    extract::{Request, State},
    http::{Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use lantern_core::{
    api::*,
    ids::{InstanceId, ProjectId},
};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{collections::BTreeMap, future::Future, time::Duration};
use tokio::time::Instant;

pub(crate) async fn within<T>(
    state: &ApiState,
    label: &str,
    future: impl Future<Output = FacadeResult<T>>,
) -> Result<T, HttpError> {
    within_deadline(label, state.config.module_timeout, future).await
}
async fn within_deadline<T>(
    label: &str,
    deadline: Duration,
    future: impl Future<Output = FacadeResult<T>>,
) -> Result<T, HttpError> {
    match tokio::time::timeout(deadline, future).await {
        Ok(result) => result.map_err(HttpError::from),
        Err(_) => {
            tracing::warn!(module = label, "daemon API module deadline expired");
            Err(HttpError::new("timeout", format!("{label} timed out")))
        }
    }
}
fn query<T: DeserializeOwned>(s: &str) -> Result<T, HttpError> {
    serde_urlencoded::from_str(s).map_err(|e| HttpError::new("bad_request", format!("invalid query: {e}")))
}
fn body<T: DeserializeOwned>(bytes: &Bytes, optional: bool) -> Result<T, HttpError> {
    let bytes: &[u8] = if optional && bytes.is_empty() {
        b"{}"
    } else {
        bytes
    };
    serde_json::from_slice(bytes).map_err(|e| HttpError::new("invalid_json", format!("invalid JSON: {e}")))
}
fn project(s: &str) -> Result<ProjectId, HttpError> {
    ProjectId::parse(s).map_err(|e| HttpError::new("bad_request", e.to_string()))
}
fn instance(s: &str) -> Result<InstanceId, HttpError> {
    InstanceId::parse(s).map_err(|e| HttpError::new("bad_request", e.to_string()))
}
fn output<T: Serialize>(data: T) -> Response {
    output_status(StatusCode::OK, data)
}
fn output_status<T: Serialize>(status: StatusCode, data: T) -> Response {
    match serde_json::to_vec(&data) {
        Ok(bytes) => (
            status,
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            bytes,
        )
            .into_response(),
        Err(_) => HttpError::new("internal", "could not serialize response").into_response(),
    }
}
fn list<T: Serialize>(items: Vec<T>) -> Response {
    output(List { items })
}
fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}
fn method_error() -> HttpError {
    HttpError::new("method_not_allowed", "method not allowed")
}
fn wait_params(q: &str) -> Result<WaitParams, HttpError> {
    let params: WaitParams = query(q)?;
    if params.timeout.is_some_and(|v| v > 600) {
        return Err(HttpError::new(
            "bad_request",
            "timeout must be at most 600 seconds",
        ));
    }
    Ok(params)
}
fn active(status: OperationStatus) -> bool {
    matches!(status, OperationStatus::Queued | OperationStatus::Running)
}
fn operation_failure(op: &Operation) -> HttpError {
    let error = op.error.clone().unwrap_or_else(|| ApiError {
        code: "operation_failed".into(),
        message: "operation failed".into(),
        details: json!({}),
    });
    HttpError::operation(error, &op.id.0)
}
async fn operation(state: &ApiState, ticket: OperationTicket, w: WaitParams) -> Result<Response, HttpError> {
    let deadline = Instant::now() + Duration::from_secs(u64::from(w.timeout.unwrap_or(60)));
    let op = if w.wait {
        state
            .operations
            .wait(&ticket.id, deadline.saturating_duration_since(Instant::now()))
            .await
    } else {
        state.operations.get(&ticket.id)
    }
    .ok_or_else(|| HttpError::new("internal", "facade returned an untracked operation"))?;
    if w.wait && matches!(op.status, OperationStatus::Failed | OperationStatus::Cancelled) {
        return Err(operation_failure(&op));
    }
    let mut view = ticket.instance;
    if w.wait
        && op.status == OperationStatus::Succeeded
        && let Some(id) = &op.instance
        && let Ok(fresh) = within_deadline(
            "registry",
            state
                .config
                .module_timeout
                .min(deadline.saturating_duration_since(Instant::now())),
            state.facade.instance(id.clone()),
        )
        .await
    {
        view = Some(fresh);
    }
    let status = if active(op.status) || (!w.wait && !op.noop && ticket.forgotten != Some(true)) {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok(output_status(
        status,
        OperationResponse {
            operation: op,
            instance: view,
            created: ticket.created,
            forgotten: ticket.forgotten,
        },
    ))
}
async fn operations(
    state: &ApiState,
    tickets: Vec<OperationTicket>,
    w: WaitParams,
) -> Result<Response, HttpError> {
    let deadline = Instant::now() + Duration::from_secs(u64::from(w.timeout.unwrap_or(60)));
    let mut all = Vec::with_capacity(tickets.len());
    for ticket in tickets {
        let op = if w.wait {
            state
                .operations
                .wait(&ticket.id, deadline.saturating_duration_since(Instant::now()))
                .await
        } else {
            state.operations.get(&ticket.id)
        }
        .ok_or_else(|| HttpError::new("internal", "facade returned an untracked operation"))?;
        if w.wait && matches!(op.status, OperationStatus::Failed | OperationStatus::Cancelled) {
            return Err(operation_failure(&op));
        }
        all.push(op);
    }
    let status = if all.iter().any(|op| active(op.status)) || (!w.wait && all.iter().any(|op| !op.noop)) {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok(output_status(status, OperationsResponse { operations: all }))
}
#[derive(Deserialize, Default)]
struct ProjectQuery {
    project: Option<ProjectId>,
}
#[derive(Deserialize, Default)]
struct OperationQuery {
    instance: Option<InstanceId>,
    status: Option<OperationStatus>,
    limit: Option<u32>,
}
#[derive(Deserialize, Default)]
struct IntelQuery {
    max_age: Option<u32>,
    q: Option<String>,
    limit: Option<u32>,
    offset: Option<u64>,
    max_chars: Option<u64>,
    format: Option<String>,
}
#[derive(Deserialize, Default)]
struct CheckoutQuery {
    #[serde(default)]
    refresh: bool,
}

fn doc_path(parts: &[&str]) -> Result<String, HttpError> {
    let path = parts.join("/");
    if path.is_empty()
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path.split('/').any(|segment| matches!(segment, "" | "." | ".."))
    {
        return Err(HttpError::new("forbidden", "invalid document path"));
    }
    Ok(path)
}

fn valid_percent_encoding(part: &str) -> bool {
    let mut bytes = part.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%'
            && !matches!((bytes.next(), bytes.next()), (Some(a), Some(b)) if a.is_ascii_hexdigit() && b.is_ascii_hexdigit())
        {
            return false;
        }
    }
    true
}

/// Route all documented path segments through a single envelope-aware dispatcher.
pub(crate) async fn handle(State(state): State<ApiState>, req: Request) -> Response {
    match dispatch(state, req).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\"', "&quot;")
        .replace('\'', "&#39;")
}
async fn dispatch(state: ApiState, req: Request) -> Result<Response, HttpError> {
    let method = req.method().clone();
    let raw_path = req.uri().path().to_string();
    let raw_query = req.uri().query().unwrap_or("").to_string();
    let accept_text = req
        .headers()
        .get(header::ACCEPT)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|s| s.split(',').any(|v| v.trim().starts_with("text/plain")));
    let shutdown = req
        .extensions()
        .get::<tokio_util::sync::CancellationToken>()
        .cloned()
        .unwrap_or_else(|| state.shutdown.clone());
    let parts: Vec<String> = raw_path
        .trim_start_matches('/')
        .split('/')
        .map(|part| {
            if !valid_percent_encoding(part) {
                return Err(HttpError::new("bad_request", "invalid percent encoding in path"));
            }
            percent_decode_str(part)
                .decode_utf8()
                .map(|s| s.into_owned())
                .map_err(|_| HttpError::new("bad_request", "invalid UTF-8 path"))
        })
        .collect::<Result<_, _>>()?;
    let path: Vec<&str> = parts.iter().map(String::as_str).collect();
    if path.as_slice() == ["api", "events"] {
        if method != Method::GET {
            return Err(method_error());
        }
        return sse::stream(state, &raw_query, shutdown).await;
    }
    let bytes = to_bytes(req.into_body(), 1024 * 1024)
        .await
        .map_err(|_| HttpError::new("payload_too_large", "request body exceeds 1 MiB"))?;
    let f = &state.facade;
    macro_rules! call {
        ($label:literal, $future:expr) => {
            within(&state, $label, $future).await?
        };
    }
    macro_rules! action {
        ($future:expr) => {{
            let w = wait_params(&raw_query)?;
            let ticket = call!("instance", $future);
            operation(&state, ticket, w).await?
        }};
    }
    let response = match path.as_slice() {
        ["api"] | ["api", ""] => {
            if method != Method::GET {
                return Err(method_error());
            }
            output(ApiRoot {
                name: "lantern".into(),
                version: state.config.version.clone(),
                api_version: API_VERSION,
                mcp: "/mcp".into(),
                links: BTreeMap::from([
                    ("projects".into(), "/api/projects".into()),
                    ("instances".into(), "/api/instances".into()),
                    ("events".into(), "/api/events".into()),
                ]),
            })
        }
        ["api", "system"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            output(call!("system", f.system_info()))
        }
        ["api", "system", "health"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            output(call!("system", f.system_health()))
        }
        ["api", "system", "reconcile"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            output(call!("system", f.reconcile()))
        }
        ["api", "system", "stop-all"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            let w = wait_params(&raw_query)?;
            let tickets = call!("runtime", f.stop_all(body::<StopAllRequest>(&bytes, true)?));
            operations(&state, tickets, w).await?
        }
        ["api", "settings"] => match method.as_str() {
            "GET" => output(call!("settings", f.settings())),
            "PATCH" => output(call!("settings", f.patch_settings(body::<Value>(&bytes, false)?))),
            _ => return Err(method_error()),
        },
        ["api", "settings", "reload"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            output(call!("settings", f.reload_settings()))
        }
        ["api", "projects"] => match method.as_str() {
            "GET" => {
                let mut filter: ProjectFilter = query(&raw_query)?;
                if filter
                    .enabled
                    .as_deref()
                    .is_some_and(|x| !matches!(x, "true" | "false" | "any"))
                {
                    return Err(HttpError::new(
                        "bad_request",
                        "enabled must be true, false, or any",
                    ));
                }
                filter.enabled.get_or_insert_with(|| "true".to_owned());
                list(call!("registry", f.projects(filter)))
            }
            "POST" => output_status(
                StatusCode::CREATED,
                call!(
                    "registry",
                    f.register_project(body::<RegisterProjectRequest>(&bytes, false)?)
                ),
            ),
            _ => return Err(method_error()),
        },
        ["api", "projects", "scan"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            output(call!(
                "registry",
                f.scan_projects(body::<ScanRequest>(&bytes, true)?)
            ))
        }
        ["api", "projects", id] => {
            let id = project(id)?;
            match method.as_str() {
                "GET" => output(call!("registry", f.project(id))),
                "PATCH" => output(call!(
                    "registry",
                    f.patch_project(id, body::<PatchProjectRequest>(&bytes, false)?)
                )),
                "DELETE" => {
                    call!(
                        "registry",
                        f.unregister_project(id, query::<UnregisterParams>(&raw_query)?)
                    );
                    no_content()
                }
                _ => return Err(method_error()),
            }
        }
        ["api", "projects", id, "reload"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            output(call!(
                "registry",
                f.reload_project(project(id)?, body::<ReloadRequest>(&bytes, true)?)
            ))
        }
        ["api", "projects", id, "sync"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            list(call!("registry", f.sync_project(project(id)?)))
        }
        ["api", "projects", id, "instances"] => match method.as_str() {
            "GET" => list(call!("registry", f.project_instances(project(id)?))),
            "POST" => {
                action!(f.run_branch(project(id)?, body::<CreateInstanceRequest>(&bytes, false)?))
            }
            _ => return Err(method_error()),
        },
        ["api", "projects", id, "checkouts"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            if query::<CheckoutQuery>(&raw_query)?.refresh {
                return Err(HttpError::new(
                    "bad_request",
                    "use POST /api/projects/{id}/sync to refresh checkouts",
                ));
            }
            list(call!("instances", f.checkouts(project(id)?)))
        }
        ["api", "projects", id, "branches"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            list(call!("instances", f.branches(project(id)?)))
        }
        ["api", "dependencies"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            output(call!(
                "registry",
                f.dependencies(query::<ProjectQuery>(&raw_query)?.project)
            ))
        }
        ["api", "instances"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            list(call!(
                "registry",
                f.instances(query::<InstanceFilter>(&raw_query)?)
            ))
        }
        ["api", "instances", id] => {
            let id = instance(id)?;
            match method.as_str() {
                "GET" => output(call!("registry", f.instance(id))),
                "PATCH" => output(call!(
                    "instances",
                    f.pin(id, body::<PatchInstanceRequest>(&bytes, false)?.pinned)
                )),
                "DELETE" => action!(f.remove_instance(id, query::<RemoveInstanceParams>(&raw_query)?)),
                _ => return Err(method_error()),
            }
        }
        ["api", "instances", id, "start"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            if !bytes.is_empty() {
                let _: Value = body(&bytes, false)?;
            }
            action!(f.start(instance(id)?))
        }
        ["api", "instances", id, "stop"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            if !bytes.is_empty() {
                let _: Value = body(&bytes, false)?;
            }
            action!(f.stop(instance(id)?))
        }
        ["api", "instances", id, "restart"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            if !bytes.is_empty() {
                let _: Value = body(&bytes, false)?;
            }
            action!(f.restart(instance(id)?))
        }
        ["api", "instances", id, "prepare"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            action!(f.prepare(instance(id)?, body::<PrepareRequest>(&bytes, true)?))
        }
        ["api", "instances", id, "pin"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            output(call!("instances", f.pin(instance(id)?, true)))
        }
        ["api", "instances", id, "unpin"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            output(call!("instances", f.pin(instance(id)?, false)))
        }
        ["api", "instances", id, "rename"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            output(call!(
                "instances",
                f.rename_instance(instance(id)?, body::<RenameInstanceRequest>(&bytes, false)?)
            ))
        }
        ["api", "instances", id, "removal-check"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            output(call!("instances", f.removal_check(instance(id)?)))
        }
        ["api", "instances", id, "adopt"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            output(call!(
                "instances",
                f.adopt(instance(id)?, body::<AdoptRequest>(&bytes, true)?)
            ))
        }
        ["api", "instances", id, "unadopt"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            output(call!("instances", f.unadopt(instance(id)?)))
        }
        ["api", "instances", id, "logs"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            let mut params: LogParams = query(&raw_query)?;
            if params.tail.unwrap_or(200) > 5000 {
                return Err(HttpError::new("bad_request", "tail must be at most 5000"));
            }
            let text = params.format == Some(LogFormat::Text) || accept_text;
            params.tail.get_or_insert(200);
            let page = call!("logs", f.logs(instance(id)?, params));
            if text {
                let mut content = String::new();
                for line in page.lines {
                    let stream = serde_json::to_value(line.stream).unwrap_or(Value::Null);
                    let stream = stream.as_str().unwrap_or("system");
                    content.push_str(&format!("{} {} {}\n", line.ts, stream, line.line));
                }
                ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], content).into_response()
            } else {
                output(page)
            }
        }
        ["api", "instances", id, "health", "check"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            output(within_deadline("health", Duration::from_secs(10), f.check_health(instance(id)?)).await?)
        }
        ["api", "instances", id, "manifest"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            output(call!("registry", f.manifest(instance(id)?)))
        }
        ["api", "instances", id, "intel"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            output(call!(
                "intel",
                f.intel(instance(id)?, query::<IntelQuery>(&raw_query)?.max_age)
            ))
        }
        ["api", "instances", id, "intel", "refresh"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            output(within_deadline("intel", Duration::from_secs(30), f.refresh_intel(instance(id)?)).await?)
        }
        ["api", "intel", "refresh"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            output(within_deadline("intel", Duration::from_secs(30), f.refresh_all_intel()).await?)
        }
        ["api", "instances", id, "docs"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            list(call!("intel", f.docs(instance(id)?)))
        }
        ["api", "instances", id, "docs", path @ ..] if !path.is_empty() => {
            if method != Method::GET {
                return Err(method_error());
            }
            let q: IntelQuery = query(&raw_query)?;
            let content = call!(
                "intel",
                f.doc(instance(id)?, doc_path(path)?, q.offset, q.max_chars)
            );
            if q.format.as_deref() == Some("json") {
                output(content)
            } else {
                (
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    content.content,
                )
                    .into_response()
            }
        }
        ["api", "instances", id, "endpoints"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            let q: IntelQuery = query(&raw_query)?;
            list(call!("intel", f.endpoints(instance(id)?, q.q, q.limit)))
        }
        ["api", "operations"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            let q: OperationQuery = query(&raw_query)?;
            let items = state
                .operations
                .list()
                .into_iter()
                .filter(|op| {
                    q.instance
                        .as_ref()
                        .is_none_or(|id| op.instance.as_ref() == Some(id))
                        && q.status.is_none_or(|status| op.status == status)
                })
                .take(q.limit.unwrap_or(50) as usize)
                .collect();
            list(items)
        }
        ["api", "operations", id] => {
            if method != Method::GET {
                return Err(method_error());
            }
            let id = OperationId((*id).to_string());
            let w = wait_params(&raw_query)?;
            let op = if w.wait {
                state
                    .operations
                    .wait(&id, Duration::from_secs(u64::from(w.timeout.unwrap_or(60))))
                    .await
            } else {
                state.operations.get(&id)
            }
            .ok_or_else(|| HttpError::new("operation_not_found", "operation not found"))?;
            output(op)
        }
        ["api", "operations", id, "cancel"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            let id = OperationId((*id).to_string());
            let op = state
                .operations
                .get(&id)
                .ok_or_else(|| HttpError::new("operation_not_found", "operation not found"))?;
            if !state.operations.cancel(&id) {
                return Err(HttpError::new(
                    "operation_in_progress",
                    "operation is already finished",
                ));
            }
            output(state.operations.get(&id).unwrap_or(op))
        }
        ["api", "listeners"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            list(call!(
                "discovery",
                f.listeners(query::<ListenerFilter>(&raw_query)?)
            ))
        }
        ["api", "listeners", "scan"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            list(call!("discovery", f.scan_listeners()))
        }
        ["api", "ports"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            output(call!(
                "discovery",
                f.ports(query::<ProjectQuery>(&raw_query)?.project)
            ))
        }
        ["api", "services"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            list(call!("services", f.services()))
        }
        ["api", "services", name] => {
            if method != Method::GET {
                return Err(method_error());
            }
            output(call!("services", f.service((*name).into())))
        }
        ["api", "services", name, action @ ("start" | "stop" | "restart")] => {
            if method != Method::POST {
                return Err(method_error());
            }
            let action = match *action {
                "start" => ServiceAction::Start,
                "stop" => ServiceAction::Stop,
                _ => ServiceAction::Restart,
            };
            let result =
                tokio::time::timeout(Duration::from_secs(30), f.service_action((*name).into(), action)).await;
            let value = match result {
                Ok(Ok(value)) => value,
                Ok(Err(error)) if error.code == "operation_failed" => {
                    return Err(HttpError(error, Some(StatusCode::BAD_GATEWAY)));
                }
                Ok(Err(error)) => return Err(error.into()),
                Err(_) => {
                    tracing::warn!(module = "services", "daemon API module deadline expired");
                    return Err(HttpError::new("timeout", "services timed out"));
                }
            };
            output(value)
        }
        ["api", "profiles"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            list(call!("config", f.profiles()))
        }
        ["api", "profiles", name] => match method.as_str() {
            "GET" => output(call!("config", f.profile((*name).into()))),
            "PUT" => {
                let (value, created) = call!(
                    "config",
                    f.put_profile((*name).into(), body::<Value>(&bytes, false)?)
                );
                output_status(
                    if created {
                        StatusCode::CREATED
                    } else {
                        StatusCode::OK
                    },
                    value,
                )
            }
            "DELETE" => {
                call!("config", f.delete_profile((*name).into()));
                no_content()
            }
            _ => return Err(method_error()),
        },
        ["api", "profiles", name, "activate"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            let w = wait_params(&raw_query)?;
            let tickets = call!("config", f.activate_profile((*name).into()));
            operations(&state, tickets, w).await?
        }
        ["api", "templates"] => match method.as_str() {
            "GET" => list(call!("config", f.templates())),
            "POST" => output_status(
                StatusCode::CREATED,
                call!("config", f.create_template(body::<Value>(&bytes, false)?)),
            ),
            _ => return Err(method_error()),
        },
        ["api", "templates", name] => match method.as_str() {
            "GET" => output(call!("config", f.template((*name).into()))),
            "PUT" => {
                let (value, created) = call!(
                    "config",
                    f.put_template((*name).into(), body::<Value>(&bytes, false)?)
                );
                output_status(
                    if created {
                        StatusCode::CREATED
                    } else {
                        StatusCode::OK
                    },
                    value,
                )
            }
            "DELETE" => {
                call!("config", f.delete_template((*name).into()));
                no_content()
            }
            _ => return Err(method_error()),
        },
        ["api", "suggestions"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            list(call!("registry", f.suggestions()))
        }
        ["api", "suggestions", "dismiss"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            call!(
                "registry",
                f.dismiss_suggestion(body::<DismissSuggestionRequest>(&bytes, false)?.path)
            );
            no_content()
        }
        ["api", "import", "v1"] => {
            if method != Method::POST {
                return Err(method_error());
            }
            output(call!(
                "config",
                f.import_v1(body::<ImportV1Request>(&bytes, true)?)
            ))
        }
        ["api", "schema", name] => {
            if method != Method::GET {
                return Err(method_error());
            }
            output(call!("config", f.schema((*name).into())))
        }
        [id, "docs"] => {
            if method != Method::GET {
                return Err(method_error());
            }
            let id = instance(id)?;
            let docs = call!("intel", f.docs(id.clone()));
            let mut html = format!(
                "<!doctype html><meta charset=\"utf-8\"><title>Docs for {}</title><h1>Docs for {}</h1><ul>",
                escape_html(id.as_str()),
                escape_html(id.as_str())
            );
            for doc in docs {
                let path =
                    percent_encoding::utf8_percent_encode(&doc.path, percent_encoding::NON_ALPHANUMERIC);
                html.push_str(&format!(
                    "<li><a href=\"/{}/docs/{}\">{}</a> ({}, {} bytes)</li>",
                    id.as_str(),
                    path,
                    escape_html(&doc.path),
                    escape_html(&doc.kind),
                    doc.size.unwrap_or(0)
                ));
            }
            html.push_str("</ul>");
            ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
        }
        [id, "docs", path @ ..] if !path.is_empty() => {
            if method != Method::GET {
                return Err(method_error());
            }
            let path = doc_path(path)?;
            let q: IntelQuery = query(&raw_query)?;
            let content = call!("intel", f.doc(instance(id)?, path.clone(), q.offset, q.max_chars));
            let kind = if path.ends_with(".md") {
                "text/markdown; charset=utf-8"
            } else if path.ends_with(".json") {
                "application/json; charset=utf-8"
            } else {
                "text/plain; charset=utf-8"
            };
            ([(header::CONTENT_TYPE, kind)], content.content).into_response()
        }
        _ => return Err(HttpError::new("not_found", "route not found")),
    };
    Ok(response)
}
