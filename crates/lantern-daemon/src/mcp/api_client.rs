//! Project API authorization, concurrency and job handling.

use super::{
    LanternMcp,
    upstream::{ToolApiClient, ToolApiResponse, UpstreamPolicy},
};
use crate::operations::{OpOutcome, OperationTracker};
use base64::{Engine, engine::general_purpose::STANDARD};
use lantern_core::{
    api::{ApiError, EndpointEntry, OperationId, OperationKind, OperationStatus, Risk},
    ids::InstanceId,
    model::InstanceStatus,
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Mutex, Notify, Semaphore};

#[derive(Debug)]
struct GateCount {
    active: usize,
    limit: usize,
}
#[derive(Debug)]
struct InstanceGate {
    count: std::sync::Mutex<GateCount>,
    changed: Notify,
}
impl InstanceGate {
    fn new(limit: usize) -> Self {
        Self {
            count: std::sync::Mutex::new(GateCount { active: 0, limit }),
            changed: Notify::new(),
        }
    }
    async fn acquire(self: Arc<Self>, limit: usize) -> InstancePermit {
        {
            let mut count = self.count.lock().expect("instance gate");
            // A lower observed limit applies to queued and new calls; raising it waits for restart.
            count.limit = count.limit.min(limit);
        }
        loop {
            let notified = self.changed.notified();
            {
                let mut count = self.count.lock().expect("instance gate");
                if count.active < count.limit {
                    count.active += 1;
                    return InstancePermit(self.clone());
                }
            }
            notified.await;
        }
    }
}
struct InstancePermit(Arc<InstanceGate>);
impl Drop for InstancePermit {
    fn drop(&mut self) {
        let mut count = self.0.count.lock().expect("instance gate");
        count.active -= 1;
        drop(count);
        self.0.changed.notify_one();
    }
}
/// Shared API client state retained across stateless requests.
#[derive(Clone)]
pub(super) struct ApiState {
    pub policy: UpstreamPolicy,
    pub client: Arc<dyn ToolApiClient>,
    pub trust_risk_metadata: bool,
    pub allow_critical_calls: bool,
    global: Arc<Semaphore>,
    limits: Arc<Mutex<HashMap<InstanceId, Arc<InstanceGate>>>>,
    pub tracker: OperationTracker,
}
impl ApiState {
    pub fn new(
        policy: UpstreamPolicy,
        client: Arc<dyn ToolApiClient>,
        trust_risk_metadata: bool,
        allow_critical_calls: bool,
        tracker: OperationTracker,
    ) -> Self {
        Self {
            policy,
            client,
            trust_risk_metadata,
            allow_critical_calls,
            global: Arc::new(Semaphore::new(16)),
            limits: Arc::new(Mutex::new(HashMap::new())),
            tracker,
        }
    }
}
fn risk_rank(r: &Risk) -> u8 {
    match r {
        Risk::Low => 0,
        Risk::Medium => 1,
        Risk::High => 2,
        Risk::Critical => 3,
        Risk::Unknown => 2,
    }
}
fn declared_risk(s: Option<&str>) -> u8 {
    match s {
        None | Some("low") => 0,
        Some("medium") => 1,
        Some("high") => 2,
        Some("critical") => 3,
        _ => 2,
    }
}
fn matches_path(pattern: &str, path: &str) -> bool {
    let pattern: Vec<_> = pattern.trim_matches('/').split('/').collect();
    let path: Vec<_> = path.trim_matches('/').split('/').collect();
    pattern.len() == path.len()
        && pattern
            .iter()
            .zip(path)
            .all(|(p, v)| p.starts_with(':') || (p.starts_with('{') && p.ends_with('}')) || *p == v)
}
fn effective_risk(method: &str, path: &str, endpoints: &[EndpointEntry], project_risk: Option<&str>) -> u8 {
    let inferred = match method {
        "GET" => 0,
        "DELETE" => 2,
        _ => 1,
    };
    // Fail closed for overlapping declarations: the highest matching risk wins.
    let endpoint = endpoints
        .iter()
        .filter(|e| e.method.eq_ignore_ascii_case(method) && matches_path(&e.path, path))
        .map(|e| risk_rank(&e.risk))
        .max()
        .unwrap_or(inferred);
    endpoint.max(declared_risk(project_risk))
}
fn result(response: ToolApiResponse, limit: usize) -> Result<Value, String> {
    let mime = response
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream")
        .split(';')
        .next()
        .unwrap_or("application/octet-stream")
        .trim()
        .to_ascii_lowercase();
    if response.status < 400
        && !response.truncated
        && ["image/png", "image/jpeg", "image/webp", "image/gif"].contains(&mime.as_str())
    {
        return Ok(json!({"$lantern_image": {"data": STANDARD.encode(response.body), "mime": mime}}));
    }
    let text_type = mime.starts_with("text/")
        || mime.contains("json")
        || mime.contains("yaml")
        || mime.contains("xml")
        || response.content_type.is_none();
    let mut text = if text_type {
        match String::from_utf8(response.body) {
            Ok(s) => s,
            Err(e) => format!(
                "[binary response: {}, {} bytes, not shown]",
                mime,
                e.as_bytes().len()
            ),
        }
    } else {
        format!(
            "[binary response: {}, {} bytes, not shown]",
            mime,
            response.body.len()
        )
    };
    if response.truncated {
        text.push_str(&format!(
            "\n[truncated at {limit} bytes; narrow the request or raise max_bytes (max 1000000)]"
        ));
    }
    if response.status >= 400 {
        return Err(format!(
            "HTTP {}: {}",
            response.status,
            text.chars().take(2000).collect::<String>()
        ));
    }
    Ok(Value::String(text))
}

impl LanternMcp {
    pub(super) async fn job_result(&self, id: &str, compat: bool) -> Result<Value, String> {
        let operation = self
            .api
            .tracker
            .get(&OperationId(id.into()))
            .ok_or("Unknown job id. Try call_tool_api with async=true.")?;
        if operation.kind != OperationKind::ApiCall {
            return Err("Operation is not an API job.".into());
        }
        match operation.status {
            OperationStatus::Queued | OperationStatus::Running => Ok(
                json!({"status":"running", "job_id":id, "message":"Still processing. Poll again shortly."}),
            ),
            OperationStatus::Succeeded if compat => Ok(operation.result.unwrap_or(Value::Null)),
            OperationStatus::Succeeded => {
                Ok(json!({"status":"succeeded", "job_id":id, "result":operation.result}))
            }
            _ => Err(operation
                .error
                .map_or_else(|| "API job cancelled".into(), |e| e.message)),
        }
    }
    pub(super) async fn call_api(&self, a: &Value) -> Result<Value, String> {
        if !self.api.trust_risk_metadata {
            return Err("API calls are disabled until the daemon preserves critical endpoint risk metadata. Ask the Lantern controller to enable them after integration.".into());
        }
        let tool = a["tool"].as_str().unwrap_or_default();
        let id = self
            .target(tool, a["instance"].as_str(), a["branch"].as_str())
            .await?;
        let instance = self.facade.instance(id.clone()).await.map_err(|e| e.message)?;
        let manifest = self.facade.manifest(id.clone()).await.map_err(|e| e.message)?;
        let routing = &manifest["routing"];
        if !routing.is_object()
            || !manifest["endpoints"].is_array()
            || !routing["requires_confirmation"].is_boolean()
        {
            return Err("Cannot verify complete manifest risk metadata; no request was sent.".into());
        }
        let method = a["method"].as_str().unwrap_or("GET").to_ascii_uppercase();
        let path = a["path"].as_str().unwrap_or_default();
        let base = if id.is_primary() {
            manifest["upstream_url"].as_str()
        } else {
            None
        }
        .map(str::to_owned)
        .or_else(|| {
            if matches!(
                instance.runtime.status,
                InstanceStatus::Running | InstanceStatus::Unhealthy | InstanceStatus::External
            ) {
                instance.runtime.port.map(|p| format!("http://127.0.0.1:{p}"))
            } else {
                None
            }
        })
        .or_else(|| manifest["base_url"].as_str().map(str::to_owned))
        .ok_or_else(|| format!("{id} is stopped; start it with start_project."))?;
        let body = match &a["body"] {
            Value::Null => None,
            Value::String(s) => Some(s.as_bytes().to_vec()),
            Value::Object(_) | Value::Array(_) => {
                Some(serde_json::to_vec(&a["body"]).map_err(|e| e.to_string())?)
            }
            _ => return Err("Body must be a JSON string, object, or array.".into()),
        };
        let body_async = match &a["body"] {
            Value::Object(o) => o.get("async").and_then(Value::as_bool).unwrap_or(false),
            Value::String(s) => serde_json::from_str::<Value>(s)
                .ok()
                .and_then(|v| v.get("async").and_then(Value::as_bool))
                .unwrap_or(false),
            _ => false,
        };
        let asynchronous = a["async"].as_bool().unwrap_or(body_async);
        let secs = if asynchronous {
            900
        } else {
            a["timeout_seconds"].as_u64().unwrap_or(50).clamp(1, 300)
        };
        let max = a["max_bytes"].as_u64().unwrap_or(100_000).clamp(1, 1_000_000) as usize;
        let request = self
            .api
            .policy
            .prepare(
                &base,
                path,
                &method,
                body,
                Duration::from_secs(secs),
                max,
                "application/json",
            )
            .await?;
        let endpoints = self
            .facade
            .endpoints(id.clone(), None, Some(10000))
            .await
            .map_err(|e| format!("Cannot check endpoint risk: {}", e.message))?;
        let decoded = percent_encoding::percent_decode_str(path.split('?').next().unwrap_or(path))
            .decode_utf8()
            .map_err(|_| "Invalid path encoding")?;
        let mut risk = effective_risk(&method, &decoded, &endpoints, routing["risk"].as_str());
        for endpoint in manifest["endpoints"].as_array().expect("checked array") {
            if endpoint["method"]
                .as_str()
                .is_some_and(|m| m.eq_ignore_ascii_case(&method))
                && endpoint["path"]
                    .as_str()
                    .is_some_and(|p| matches_path(p, &decoded))
            {
                let endpoint_risk = endpoint["risk"].as_str().map_or_else(
                    || match method.as_str() {
                        "GET" => 0,
                        "DELETE" => 2,
                        _ => 1,
                    },
                    |r| declared_risk(Some(r)),
                );
                risk = risk.max(endpoint_risk);
            }
        }
        if risk >= 3 && !self.api.allow_critical_calls {
            return Err("Not sent: critical calls are disabled by mcp.allow_critical_calls.".into());
        }
        if (risk >= 2 || routing["requires_confirmation"].as_bool().unwrap_or(false))
            && a["confirm"].as_bool() != Some(true)
        {
            return Err(format!(
                "Not sent: {method} {path} on '{id}' requires confirmation. Ask the user, then repeat with confirm=true."
            ));
        }
        let global = tokio::time::timeout(Duration::from_secs(5), self.api.global.clone().acquire_owned())
            .await
            .map_err(|_| "busy: global API limit reached")?
            .map_err(|_| "API semaphore closed")?;
        let per_instance = if let Some(n) = routing["max_concurrent"].as_u64() {
            if n == 0 {
                return Err("Invalid max_concurrent; no request was sent.".into());
            }
            let n = n.min(1_000) as usize;
            let gate = {
                let mut limits = self.api.limits.lock().await;
                limits
                    .entry(id.clone())
                    .or_insert_with(|| Arc::new(InstanceGate::new(n)))
                    .clone()
            };
            Some(
                tokio::time::timeout(Duration::from_secs(5), gate.acquire(n))
                    .await
                    .map_err(|_| "busy: project API limit reached")?,
            )
        } else {
            None
        };
        let client = self.api.client.clone();
        if asynchronous {
            let active = self
                .api
                .tracker
                .list()
                .into_iter()
                .filter(|op| {
                    op.kind == OperationKind::ApiCall
                        && matches!(op.status, OperationStatus::Queued | OperationStatus::Running)
                })
                .count();
            if active >= 200 {
                return Err("Too many active API jobs; try again later.".into());
            }
            let job = self.api.tracker.begin(OperationKind::ApiCall, Some(id));
            let job_id = job.id().to_string();
            let cancel = job.cancel_token();
            tokio::spawn(async move {
                let _permits = (global, per_instance);
                let answer = tokio::select! {
                    () = cancel.cancelled() => { job.finish(OpOutcome::Cancelled); return; },
                    response = tokio::time::timeout(Duration::from_secs(900), client.send(request)) => match response {
                        Ok(r) => r.and_then(|r| result(r, max)),
                        Err(_) => Err("API job timed out".into()),
                    },
                };
                match answer {
                    Ok(value) => {
                        job.finish(OpOutcome::Succeeded(Some(value)));
                    }
                    Err(message) => {
                        job.finish(OpOutcome::Failed(ApiError {
                            code: "api_call_failed".into(),
                            message,
                            details: json!({}),
                        }));
                    }
                }
            });
            Ok(
                json!({"status":"submitted","job_id":job_id,"message":"Use get_job_result or get_operation with this id."}),
            )
        } else {
            let _permits = (global, per_instance);
            result(client.send(request).await?, max)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn mcp_instance_gate_enforces_lowered_limit() {
        let gate = Arc::new(InstanceGate::new(2));
        let first = gate.clone().acquire(2).await;
        let second = gate.clone().acquire(2).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), gate.clone().acquire(1))
                .await
                .is_err()
        );
        drop(first);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), gate.clone().acquire(1))
                .await
                .is_err()
        );
        drop(second);
        let one = tokio::time::timeout(Duration::from_millis(100), gate.clone().acquire(1))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), gate.clone().acquire(2))
                .await
                .is_err()
        );
        drop(one);
    }
}
