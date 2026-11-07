//! Manual and OpenAPI endpoint ingestion.

use super::docs::read_bytes;
use crate::mcp::upstream::{ToolApiClient, UpstreamPolicy};
use lantern_core::{
    api::{ApiMeta, EndpointEntry, IntelError, IntelSource, ParamHint, Risk},
    manifest::ResolvedManifest,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    time::Duration,
};

const MAX_OPENAPI: usize = 5 * 1024 * 1024;
fn method_risk(method: &str) -> Risk {
    match method {
        "GET" | "HEAD" | "OPTIONS" => Risk::Low,
        "DELETE" => Risk::High,
        _ => Risk::Medium,
    }
}
fn risk(value: Option<&str>, method: &str) -> Risk {
    match value {
        Some("low") => Risk::Low,
        Some("medium") => Risk::Medium,
        Some("high") => Risk::High,
        Some("critical") => Risk::Critical,
        Some(_) => Risk::Unknown,
        None => method_risk(method),
    }
}
fn parse(body: &[u8]) -> Result<Value, String> {
    let text = std::str::from_utf8(body).map_err(|_| "OpenAPI is not UTF-8")?;
    let value: Value = serde_json::from_str(text)
        .or_else(|_| {
            serde_saphyr::from_str(text)
                .map_err(|_| serde_json::Error::io(std::io::Error::other("invalid YAML")))
        })
        .map_err(|_| "invalid OpenAPI JSON/YAML")?;
    if !value.get("paths").is_some_and(Value::is_object) {
        return Err("OpenAPI has no paths object".into());
    }
    Ok(value)
}
fn parameters(root: &Value, value: Option<&Value>) -> Vec<ParamHint> {
    let mut out = Vec::new();
    for item in value.and_then(Value::as_array).into_iter().flatten() {
        let item = if let Some(reference) = item.get("$ref").and_then(Value::as_str) {
            if !(reference.starts_with("#/components/parameters/") || reference.starts_with("#/parameters/"))
            {
                continue;
            }
            match root.pointer(&reference[1..]) {
                Some(v) => v,
                None => continue,
            }
        } else {
            item
        };
        if let (Some(name), Some(location)) = (
            item.get("name").and_then(Value::as_str),
            item.get("in").and_then(Value::as_str),
        ) {
            out.push(ParamHint {
                name: name.into(),
                r#in: location.into(),
                required: item.get("required").and_then(Value::as_bool).unwrap_or(false),
            });
        }
    }
    out
}
fn extract(root: &Value) -> Vec<EndpointEntry> {
    let mut out = Vec::new();
    if let Some(paths) = root.get("paths").and_then(Value::as_object) {
        for (path, item) in paths {
            if item.get("$ref").is_some() {
                continue;
            }
            for method in ["get", "post", "put", "patch", "delete", "options", "head"] {
                let Some(op) = item.get(method).filter(|v| v.is_object()) else {
                    continue;
                };
                let method = method.to_ascii_uppercase();
                let description = op
                    .get("summary")
                    .or_else(|| op.get("description"))
                    .and_then(Value::as_str)
                    .map(|s| s.lines().next().unwrap_or("").chars().take(200).collect());
                let category = op
                    .get("tags")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let mut params = parameters(root, item.get("parameters"));
                for param in parameters(root, op.get("parameters")) {
                    if let Some(existing) = params
                        .iter_mut()
                        .find(|p| p.name == param.name && p.r#in == param.r#in)
                    {
                        *existing = param
                    } else {
                        params.push(param)
                    }
                }
                let body_hint = op
                    .pointer("/requestBody/content")
                    .and_then(Value::as_object)
                    .map(|o| o.keys().cloned().collect::<Vec<_>>().join(", "))
                    .or_else(|| {
                        op.get("consumes")
                            .and_then(Value::as_array)
                            .map(|v| v.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
                    });
                let risk = risk(op.get("x-risk").and_then(Value::as_str), &method);
                out.push(EndpointEntry {
                    method,
                    path: path.clone(),
                    description,
                    category,
                    risk,
                    body_hint,
                    params,
                    source: IntelSource::Discovered,
                });
            }
        }
    }
    out
}
async fn fetch(
    base: &str,
    path: &str,
    policy: &UpstreamPolicy,
    client: &dyn ToolApiClient,
    timeout_ms: u64,
) -> Result<Value, String> {
    let request = policy
        .prepare(
            base,
            path,
            "GET",
            None,
            Duration::from_millis(timeout_ms),
            MAX_OPENAPI,
            "application/json, application/yaml",
        )
        .await?;
    let response = client.send(request).await?;
    if response.status != 200 {
        return Err(format!("HTTP {}", response.status));
    }
    if response.truncated {
        return Err("OpenAPI exceeds 5 MiB".into());
    }
    parse(&response.body)
}
/// Inputs to an individual endpoint refresh.
pub(super) struct Input<'a> {
    pub root: &'a Path,
    pub manifest: &'a ResolvedManifest,
    pub base: Option<&'a str>,
    pub running: bool,
    pub previous: &'a [EndpointEntry],
    pub policy: &'a UpstreamPolicy,
    pub client: &'a dyn ToolApiClient,
}
pub(super) async fn scan(input: Input<'_>) -> (Vec<EndpointEntry>, ApiMeta, bool) {
    let auto = input.manifest.api_auto.as_ref();
    let enabled = auto.and_then(|a| a.enabled).unwrap_or(false);
    let sources = auto.and_then(|a| a.sources.clone()).unwrap_or_default();
    let candidates = auto.and_then(|a| a.candidates.clone()).unwrap_or_else(|| {
        vec![
            "/openapi.json".into(),
            "/swagger.json".into(),
            "/docs/openapi.json".into(),
        ]
    });
    let timeout_ms = auto.and_then(|a| a.timeout_ms).unwrap_or(2500).clamp(1, 30_000);
    let max = auto.and_then(|a| a.max_endpoints).unwrap_or(400).min(10_000) as usize;
    let mut entries: BTreeMap<(String, String), EndpointEntry> = BTreeMap::new();
    for e in &input.manifest.endpoints {
        let method = e.method.to_ascii_uppercase();
        let body_hint = e
            .body_hint
            .as_ref()
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned));
        let params = e
            .params
            .clone()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        entries.insert(
            (e.path.clone(), method.clone()),
            EndpointEntry {
                method: method.clone(),
                path: e.path.clone(),
                description: e.description.clone(),
                category: e.category.clone(),
                risk: risk(e.risk.as_deref(), &method),
                body_hint,
                params,
                source: IntelSource::Manual,
            },
        );
    }
    let mut errors = Vec::new();
    let mut tried = Vec::new();
    let mut docs = Vec::new();
    if enabled {
        for source in &sources {
            tried.push(source.clone());
            let result = if source.starts_with("http://") || source.starts_with("https://") {
                match (
                    reqwest::Url::parse(source),
                    input.base.and_then(|s| reqwest::Url::parse(s).ok()),
                ) {
                    (Ok(url), Some(base))
                        if url.scheme() == base.scheme()
                            && url.host_str() == base.host_str()
                            && url.port_or_known_default() == base.port_or_known_default()
                            && url.username().is_empty()
                            && url.password().is_none()
                            && url.fragment().is_none() =>
                    {
                        let path = if let Some(q) = url.query() {
                            format!("{}?{q}", url.path())
                        } else {
                            url.path().to_string()
                        };
                        fetch(base.as_str(), &path, input.policy, input.client, timeout_ms).await
                    }
                    _ => Err("OpenAPI URL must use the registered upstream authority".into()),
                }
            } else {
                read_bytes(input.root, source, MAX_OPENAPI as u64).and_then(|b| parse(&b))
            };
            match result {
                Ok(doc) => docs.push(doc),
                Err(error) => errors.push(IntelError {
                    source: source.clone(),
                    error,
                }),
            }
        }
        if input.running
            && let Some(base) = input.base
        {
            let mut found = false;
            for candidate in &candidates {
                tried.push(candidate.clone());
                if let Ok(doc) = fetch(base, candidate, input.policy, input.client, timeout_ms).await {
                    docs.push(doc);
                    found = true;
                    break;
                }
            }
            if !found && !candidates.is_empty() {
                errors.push(IntelError {
                    source: "candidates".into(),
                    error: format!("no_openapi_found: {}", candidates.join(", ")),
                });
            }
        }
    }
    let stale = enabled && !input.running && input.base.is_some();
    if stale {
        for e in input
            .previous
            .iter()
            .filter(|e| e.source == IntelSource::Discovered)
        {
            entries
                .entry((e.path.clone(), e.method.clone()))
                .or_insert_with(|| e.clone());
        }
    }
    for doc in docs {
        for e in extract(&doc) {
            entries.entry((e.path.clone(), e.method.clone())).or_insert(e);
        }
    }
    let endpoints: Vec<_> = entries.into_values().take(max).collect();
    let mut unique = HashSet::new();
    tried.retain(|s| unique.insert(s.clone()));
    let meta = ApiMeta {
        enabled,
        count: endpoints.len() as u32,
        source_count: tried.len() as u32,
        sources: tried,
        errors,
    };
    (endpoints, meta, stale)
}
