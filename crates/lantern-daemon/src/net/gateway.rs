//! Guarded gateway pages for known, unavailable and unknown local hosts.

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header, uri::Authority},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use lantern_core::{ids::InstanceId, model::InstanceStatus};
use serde::Serialize;
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

use super::routes::valid_host;

/// Public instance details needed by the gateway.
#[derive(Debug, Clone, Serialize)]
pub struct GatewayInstance {
    /// Owning instance ID.
    pub id: InstanceId,
    /// Current lifecycle status.
    pub status: InstanceStatus,
    /// Human-readable status or failure details.
    pub message: Option<String>,
    /// Current upstream port, if present.
    pub port: Option<u16>,
}

/// Future returned by a gateway start request.
pub type GatewayStartFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Narrow boundary to the current route plan and runtime start operation.
pub trait GatewayBackend: Send + Sync {
    /// Return literal hosts in the current route plan, including the daemon host.
    fn known_hosts(&self) -> Vec<String>;
    /// Look up the instance owning a literal host, if any.
    fn instance_for_host(&self, host: &str) -> Option<GatewayInstance>;
    /// Start one instance through the normal runtime operation.
    fn start<'a>(&'a self, id: &'a InstanceId) -> GatewayStartFuture<'a>;
}

/// Explicit gateway dependencies, including expected forwarded HTTPS port.
#[derive(Clone)]
pub struct GatewayState {
    /// Current plan and runtime adapter.
    pub backend: Arc<dyn GatewayBackend>,
    /// Port of the loopback daemon listener.
    pub daemon_port: u16,
    /// Public HTTPS listener port.
    pub https_port: u16,
    /// Public root certificate path.
    pub root_cert: PathBuf,
}
impl std::fmt::Debug for GatewayState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayState")
            .field("daemon_port", &self.daemon_port)
            .field("https_port", &self.https_port)
            .field("root_cert", &self.root_cert)
            .finish_non_exhaustive()
    }
}
fn one<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let v = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(v)
}
fn guard_host(state: &GatewayState, h: &HeaderMap) -> bool {
    let Some(value) = one(h, "host") else {
        return false;
    };
    value == format!("127.0.0.1:{}", state.daemon_port) || value == format!("localhost:{}", state.daemon_port)
}
fn forwarded<'a>(state: &GatewayState, h: &'a HeaderMap) -> Option<(&'a str, String)> {
    let raw = one(h, "x-forwarded-host")?;
    let authority: Authority = raw.parse().ok()?;
    let host = authority.host();
    if !valid_host(host) || authority.as_str() != raw || raw.contains('@') {
        return None;
    }
    match (state.https_port, authority.port_u16()) {
        (443, None) if raw == host => {}
        (443, Some(443)) => {}
        (port, Some(actual)) if port == actual => {}
        _ => return None,
    }
    Some((raw, host.to_owned()))
}
fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
fn forbidden() -> Response {
    no_store((StatusCode::FORBIDDEN, "forbidden").into_response())
}
fn escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}
fn nonce() -> Result<String, ()> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| ())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

async fn ping(State(s): State<GatewayState>, h: HeaderMap) -> Response {
    if !guard_host(&s, &h) {
        return forbidden();
    }
    no_store((StatusCode::OK, "lantern-ok").into_response())
}
async fn cert(State(s): State<GatewayState>, h: HeaderMap) -> Response {
    if !guard_host(&s, &h) {
        return forbidden();
    }
    match lantern_system::ca::read_bounded_nofollow(&s.root_cert, 16 * 1024) {
        Ok(bytes) => {
            no_store(([(header::CONTENT_TYPE, "application/x-x509-ca-cert")], bytes).into_response())
        }
        Err(_) => no_store((StatusCode::NOT_FOUND, "certificate unavailable").into_response()),
    }
}
async fn status(State(s): State<GatewayState>, h: HeaderMap) -> Response {
    if !guard_host(&s, &h) {
        return forbidden();
    }
    let Some((_, host)) = forwarded(&s, &h) else {
        return forbidden();
    };
    match s.backend.instance_for_host(&host) {
        Some(view) if s.backend.known_hosts().contains(&host) => no_store(
            Json(
                serde_json::json!({"host":host,"status":view.status,"message":view.message,"port":view.port}),
            )
            .into_response(),
        ),
        _ => no_store((StatusCode::NOT_FOUND, "unknown host").into_response()),
    }
}
async fn start(State(s): State<GatewayState>, h: HeaderMap) -> Response {
    if !guard_host(&s, &h) {
        return forbidden();
    }
    let Some((raw, host)) = forwarded(&s, &h) else {
        return forbidden();
    };
    if !s.backend.known_hosts().contains(&host)
        || one(&h, "origin") != Some(format!("https://{raw}").as_str())
    {
        return forbidden();
    }
    if h.contains_key("sec-fetch-site") && one(&h, "sec-fetch-site") != Some("same-origin") {
        return forbidden();
    }
    let Some(instance) = s.backend.instance_for_host(&host) else {
        return forbidden();
    };
    match s.backend.start(&instance.id).await {
        Ok(()) => no_store((StatusCode::ACCEPTED, Json(serde_json::json!({"started":true}))).into_response()),
        Err(message) => {
            tracing::warn!(%message,"gateway start failed");
            no_store((StatusCode::BAD_GATEWAY, "start failed").into_response())
        }
    }
}
async fn page(State(s): State<GatewayState>, h: HeaderMap) -> Response {
    if !guard_host(&s, &h) {
        return forbidden();
    }
    let Some((_, host)) = forwarded(&s, &h) else {
        return forbidden();
    };
    let hosts = s.backend.known_hosts();
    let known = hosts.contains(&host);
    let instance = if known {
        s.backend.instance_for_host(&host)
    } else {
        None
    };
    let is_error = one(&h, "x-lantern-error-status").is_some_and(|v| matches!(v, "502" | "503" | "504"));
    let code = if !known {
        StatusCode::NOT_FOUND
    } else if is_error {
        StatusCode::BAD_GATEWAY
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    if one(&h, "accept").is_some_and(|v| {
        v.split(',')
            .any(|part| part.trim().starts_with("application/json"))
    }) {
        return no_store((code, Json(serde_json::json!({"host":host,"instance":instance}))).into_response());
    }
    let Ok(nonce) = nonce() else {
        return no_store((StatusCode::INTERNAL_SERVER_ERROR, "randomness unavailable").into_response());
    };
    let (title, detail, button, refresh) = match instance {
        Some(ref instance) => {
            let title = if is_error {
                format!("{} is running but not answering", instance.id)
            } else {
                format!("{} is {:?}", instance.id, instance.status).to_lowercase()
            };
            let detail = instance
                .message
                .clone()
                .unwrap_or_else(|| format!("Run lantern on {}", instance.id));
            (title, detail, true, instance.status == InstanceStatus::Starting)
        }
        None if known => (
            format!("{host} is unavailable"),
            "Check the service status in Lantern.".into(),
            false,
            false,
        ),
        None => (
            format!("Unknown host: {host}"),
            format!("Known hosts: {}", hosts.join(", ")),
            false,
            false,
        ),
    };
    let button = if button && !is_error {
        "<button id=\"start\">Start</button><p id=\"result\"></p>"
    } else {
        ""
    };
    let script = if refresh {
        "setTimeout(()=>location.reload(),1000);"
    } else if !button.is_empty() && !is_error {
        "setInterval(async()=>{try{const r=await fetch('/__lantern/status');if(r.ok&&(await r.json()).status==='running')location.reload()}catch(_){}},2000);"
    } else {
        ""
    };
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Lantern</title><style nonce=\"{nonce}\">body{{font:16px system-ui;max-width:48rem;margin:4rem auto;padding:1rem}}button{{padding:.6rem 1rem}}</style></head><body><h1>{}</h1><p>{}</p>{button}<script nonce=\"{nonce}\">const b=document.getElementById('start');if(b)b.onclick=async()=>{{const r=await fetch('/__lantern/start',{{method:'POST'}});document.getElementById('result').textContent=r.ok?'Starting…':'Start failed';}};{script}</script></body></html>",
        escape(&title),
        escape(&detail)
    );
    let csp = format!(
        "default-src 'none'; style-src 'nonce-{nonce}'; script-src 'nonce-{nonce}'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'"
    );
    let mut response = Response::new(Body::from(html));
    *response.status_mut() = code;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_str(&csp).expect("hex nonce is a header value"),
    );
    if code == StatusCode::SERVICE_UNAVAILABLE {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
    }
    no_store(response)
}

/// Build the gateway router at its full `/__lantern/` paths.
pub fn router(state: GatewayState) -> Router {
    Router::new()
        .route("/__lantern/page", get(page))
        .route("/__lantern/status", get(status))
        .route("/__lantern/start", post(start))
        .route("/__lantern/ping", get(ping))
        .route("/__lantern/ca.crt", get(cert))
        .with_state(state)
}
