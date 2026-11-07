//! DNS-rebinding, browser-origin and request body guard.

use super::{ApiConfig, error::HttpError};
use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

const MAX_BODY: usize = 1024 * 1024;
fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, ()> {
    let mut values = headers.get_all(name).iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    first.to_str().map(Some).map_err(|_| ())
}
fn valid_host(host: &str, config: &ApiConfig) -> bool {
    if host == format!("127.0.0.1:{}", config.port) || host == format!("localhost:{}", config.port) {
        return true;
    }
    let domain = format!("lantern.{}", config.tld);
    host == domain
        || host.strip_prefix(&format!("{domain}:")).is_some_and(|port| {
            !port.is_empty() && port.bytes().all(|c| c.is_ascii_digit()) && port.parse::<u16>().is_ok()
        })
}

fn valid_origin(origin: &str, config: &ApiConfig) -> bool {
    let mut allowed = vec![
        format!("http://127.0.0.1:{}", config.port),
        format!("http://localhost:{}", config.port),
        format!("https://lantern.{}", config.tld),
        format!("http://lantern.{}", config.tld),
    ];
    if let Some(port) = config.gateway_port {
        allowed.push(format!("https://lantern.{}:{port}", config.tld));
        allowed.push(format!("http://lantern.{}:{port}", config.tld));
    }
    allowed.iter().any(|a| a == origin)
}
/// Wrap every response, including guard and routing errors, in browser-safety headers.
pub(crate) async fn headers(request: Request, next: Next) -> Response {
    let docs_page = request.uri().path().trim_start_matches('/').split('/').nth(1) == Some("docs");
    let mut response = next.run(request).await;
    let h = response.headers_mut();
    if docs_page {
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"),
        );
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}
/// Enforce the startup allowlist and JSON body requirements.
pub(crate) async fn check(State(config): State<ApiConfig>, request: Request, next: Next) -> Response {
    let host = single_header(request.headers(), "host");
    if !matches!(host, Ok(Some(value)) if valid_host(value, &config)) {
        return HttpError::new(
            "forbidden_host",
            format!(
                "allowed hosts: 127.0.0.1:{0}, localhost:{0}, lantern.{1}",
                config.port, config.tld
            ),
        )
        .into_response();
    }
    if !matches!(single_header(request.headers(), "origin"), Ok(None) | Ok(Some(_))) {
        return HttpError::new("forbidden_origin", "invalid Origin").into_response();
    }
    if let Ok(Some(origin)) = single_header(request.headers(), "origin")
        && !valid_origin(origin, &config)
    {
        return HttpError::new("forbidden_origin", "Origin is not a daemon origin").into_response();
    }
    if !matches!(
        single_header(request.headers(), "sec-fetch-site"),
        Ok(None) | Ok(Some("same-origin" | "none"))
    ) {
        return HttpError::new("forbidden_origin", "cross-site browser request rejected").into_response();
    }
    let (parts, body) = request.into_parts();
    let bytes = match to_bytes(body, MAX_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => return HttpError::new("payload_too_large", "request body exceeds 1 MiB").into_response(),
    };
    if !bytes.is_empty() {
        let content_type = single_header(&parts.headers, "content-type");
        let is_json = matches!(content_type, Ok(Some(value)) if value.split(';').next().is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json")));
        if !is_json {
            return HttpError::new(
                "unsupported_media_type",
                "request bodies require application/json",
            )
            .into_response();
        }
        if let Err(error) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            return HttpError::new("invalid_json", format!("invalid JSON: {error}")).into_response();
        }
    }
    next.run(Request::from_parts(parts, Body::from(bytes))).await
}
