//! Shared SSRF policy and a narrow, pinned HTTP client for project servers.

use async_trait::async_trait;
use reqwest::{Url, redirect::Policy};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::time::timeout;

/// User-controlled destination policy; manifests cannot add allowed hosts or infrastructure ports.
#[derive(Debug, Clone)]
pub struct UpstreamPolicy {
    /// The Lantern local-domain suffix, without a leading dot.
    pub tld: String,
    /// Exact external host names or IP literals permitted by user configuration.
    pub allowed_hosts: Vec<String>,
    /// Infrastructure ports which must never be contacted by MCP or intel.
    pub infra_ports: Vec<u16>,
}
impl Default for UpstreamPolicy {
    fn default() -> Self {
        Self {
            tld: "glow".into(),
            allowed_hosts: Vec::new(),
            infra_ports: vec![4777, 4778, 2019, 5354],
        }
    }
}

/// Fully validated upstream request, with DNS already resolved and pinned.
#[derive(Debug, Clone)]
pub struct ToolApiRequest {
    /// Validated URL from a registered instance's manifest/runtime.
    pub url: Url,
    /// Addresses resolved and vetted by the policy.
    pub addresses: Vec<SocketAddr>,
    /// An allowlisted HTTP method.
    pub method: String,
    /// Optional pre-encoded request body.
    pub body: Option<Vec<u8>>,
    /// Total request timeout.
    pub timeout: Duration,
    /// Maximum response bytes retained.
    pub max_bytes: usize,
    /// Accepted response media types.
    pub accept: &'static str,
}
/// A bounded upstream response.
#[derive(Debug, Clone)]
pub struct ToolApiResponse {
    /// HTTP status code.
    pub status: u16,
    /// Content-Type, if present.
    pub content_type: Option<String>,
    /// Response bytes up to the configured cap.
    pub body: Vec<u8>,
    /// Whether the response exceeded the cap.
    pub truncated: bool,
}
/// A testable transport boundary; callers must apply `UpstreamPolicy` before invoking it.
#[async_trait]
pub trait ToolApiClient: Send + Sync + 'static {
    /// Execute a fully validated and DNS-pinned request.
    async fn send(&self, request: ToolApiRequest) -> Result<ToolApiResponse, String>;
}
/// Reqwest transport with proxy and redirect handling disabled.
#[derive(Debug, Default)]
pub struct ReqwestToolApiClient;
#[async_trait]
impl ToolApiClient for ReqwestToolApiClient {
    async fn send(&self, request: ToolApiRequest) -> Result<ToolApiResponse, String> {
        let host = request.url.host_str().ok_or("missing upstream host")?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(request.timeout)
            .resolve_to_addrs(host, &request.addresses)
            .build()
            .map_err(|e| e.to_string())?;
        let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|e| e.to_string())?;
        let mut builder = client
            .request(method, request.url)
            .header("accept", request.accept)
            .header("user-agent", concat!("lantern/", env!("CARGO_PKG_VERSION")))
            .header("x-lantern-caller", "mcp");
        if let Some(body) = request.body {
            builder = builder.header("content-type", "application/json").body(body);
        }
        let mut response = builder
            .send()
            .await
            .map_err(|e| format!("Upstream connection failed: {e}. Check get_project and start_project."))?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned);
        let mut body = Vec::new();
        let mut truncated = false;
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            let remaining = request.max_bytes.saturating_sub(body.len());
            if chunk.len() > remaining {
                body.extend_from_slice(&chunk[..remaining]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok(ToolApiResponse {
            status,
            content_type,
            body,
            truncated,
        })
    }
}
fn forbidden(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_unspecified() || ip.is_link_local() || ip.is_multicast() || ip.is_broadcast(),
        IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or(
            ip.is_unspecified() || ip.is_unicast_link_local() || ip.is_multicast(),
            |ip| forbidden(ip.into()),
        ),
    }
}
fn valid_path(path: &str) -> bool {
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path.contains('#')
    {
        return false;
    }
    let original = path.split('?').next().unwrap_or(path);
    let mut decoded = original.to_owned();
    for _ in 0..3 {
        if decoded.to_ascii_lowercase().contains("%2f") || decoded.to_ascii_lowercase().contains("%5c") {
            return false;
        }
        let next = match percent_encoding::percent_decode_str(&decoded).decode_utf8() {
            Ok(v) => v.into_owned(),
            Err(_) => return false,
        };
        if next.contains('%') && next != decoded {
            return false;
        }
        if next.contains('\\')
            || next.chars().any(char::is_control)
            || next.starts_with("//")
            || next.split('/').any(|s| s == "." || s == "..")
        {
            return false;
        }
        if next == decoded {
            break;
        }
        decoded = next;
    }
    // Encoded path separators can change upstream routing or bypass an endpoint risk match.
    !original.to_ascii_lowercase().contains("%2f")
        && !original.to_ascii_lowercase().contains("%5c")
        && !decoded.contains('%')
}
impl UpstreamPolicy {
    /// Resolve and pin a registered base URL and relative request path; no arbitrary authority is accepted.
    #[allow(clippy::too_many_arguments)]
    pub async fn prepare(
        &self,
        base: &str,
        path: &str,
        method: &str,
        body: Option<Vec<u8>>,
        timeout_value: Duration,
        max_bytes: usize,
        accept: &'static str,
    ) -> Result<ToolApiRequest, String> {
        let method = method.to_ascii_uppercase();
        if !["GET", "POST", "PUT", "PATCH", "DELETE"].contains(&method.as_str()) {
            return Err("Method is not allowed. Use GET, POST, PUT, PATCH or DELETE.".into());
        }
        if !valid_path(path) {
            return Err("Invalid path: use one leading slash and no traversal or encoded separators. Try get_project_endpoints.".into());
        }
        if body.as_ref().is_some_and(|b| b.len() > 1_048_576) {
            return Err("Request body exceeds 1 MiB.".into());
        }
        let mut url = Url::parse(base).map_err(|_| "Invalid registered upstream URL".to_string())?;
        if !["http", "https"].contains(&url.scheme())
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err("Unsupported registered upstream URL".into());
        }
        let host = url
            .host_str()
            .ok_or("Upstream has no host")?
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let port = url.port_or_known_default().ok_or("Upstream has no port")?;
        if self.infra_ports.contains(&port) {
            return Err("Upstream uses a blocked infrastructure port".into());
        }
        let local_name = host == "localhost"
            || host
                .strip_suffix(&format!(
                    ".{}",
                    self.tld.trim_start_matches('.').to_ascii_lowercase()
                ))
                .is_some_and(|prefix| !prefix.is_empty());
        let ip = host.parse::<IpAddr>().ok();
        let local_ip = ip.is_some_and(|ip| {
            ip.is_loopback()
                || matches!(ip,IpAddr::V6(v) if v.to_ipv4_mapped().is_some_and(|v|v.is_loopback()))
        });
        let permitted = self
            .allowed_hosts
            .iter()
            .any(|h| h.trim_end_matches('.').eq_ignore_ascii_case(&host));
        if !local_name && !local_ip && !permitted {
            return Err(format!(
                "Upstream host '{host}' is not in mcp.allowed_upstream_hosts."
            ));
        }
        let addresses = timeout(
            Duration::from_secs(3),
            tokio::net::lookup_host((host.as_str(), port)),
        )
        .await
        .map_err(|_| "Upstream DNS lookup timed out")?
        .map_err(|e| format!("Upstream DNS lookup failed: {e}"))?
        .collect::<Vec<_>>();
        if addresses.is_empty()
            || addresses
                .iter()
                .any(|a| forbidden(a.ip()) || (local_name && !permitted && !a.ip().is_loopback()))
        {
            return Err("Upstream resolved to a forbidden address".into());
        }
        let (p, q) = path.split_once('?').map_or((path, None), |(p, q)| (p, Some(q)));
        url.set_path(p);
        url.set_query(q);
        if url
            .host_str()
            .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
            .as_deref()
            != Some(host.as_str())
            || url.port_or_known_default() != Some(port)
        {
            return Err("Upstream authority changed".into());
        }
        Ok(ToolApiRequest {
            url,
            addresses,
            method,
            body,
            timeout: timeout_value,
            max_bytes,
            accept,
        })
    }
}
/// The default request transport.
pub fn default_client() -> Arc<dyn ToolApiClient> {
    Arc::new(ReqwestToolApiClient)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn mcp_upstream_rejects_ssrf_and_traversal() {
        let p = UpstreamPolicy::default();
        for (base, path) in [
            ("http://127.0.0.1:4777", "/x"),
            ("http://169.254.169.254", "/x"),
            ("http://0.0.0.0:39800", "/x"),
            ("http://example.org", "/x"),
            ("http://127.0.0.1:39800", "//evil"),
            ("http://127.0.0.1:39800", "/a/../b"),
            ("http://127.0.0.1:39800", "/%252e%252e/x"),
            ("http://127.0.0.1:39800", "/%2f%2fevil"),
            ("http://127.0.0.1:39800", "/x\\y"),
        ] {
            assert!(
                p.prepare(
                    base,
                    path,
                    "GET",
                    None,
                    Duration::from_secs(1),
                    100,
                    "application/json"
                )
                .await
                .is_err(),
                "{base} {path}"
            );
        }
        assert!(
            p.prepare(
                "http://127.0.0.1:39800",
                "/x?hello=1",
                "GET",
                None,
                Duration::from_secs(1),
                100,
                "application/json"
            )
            .await
            .is_ok()
        );
    }
}
