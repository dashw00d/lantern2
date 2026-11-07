//! Render Caddy's complete, deterministic admin configuration.

use std::collections::BTreeMap;
use std::path::PathBuf;

use lantern_core::settings::ProxySettings;
use serde_json::{Value, json};

use super::routes::{Route, RoutePlan, Target};
use crate::runtime::seams::RouteTarget;

/// Paths and listener settings required to render a Caddy configuration.
#[derive(Debug, Clone)]
pub struct RenderCtx {
    /// Loopback gateway address including its port.
    pub gateway: std::net::SocketAddr,
    /// Unix socket for Caddy's admin API.
    pub admin_socket: PathBuf,
    /// Caddy file storage root.
    pub storage: PathBuf,
    /// Lantern root certificate.
    pub root_cert: PathBuf,
    /// Lantern root private key.
    pub root_key: PathBuf,
    /// PHP-FPM Unix socket.
    pub php_socket: PathBuf,
    /// FastCGI environment supplied by service integration.
    pub php_env: BTreeMap<String, String>,
    /// HTTP and HTTPS bind settings.
    pub proxy: ProxySettings,
}

fn gateway(ctx: &RenderCtx, page: bool) -> Value {
    let mut set = json!({"Host": [ctx.gateway.to_string()], "X-Forwarded-Host": ["{http.request.hostport}"]});
    if page {
        set["X-Lantern-Orig-Uri"] = json!(["{http.request.orig_uri}"]);
        set["X-Lantern-Error-Status"] = json!(["{http.error.status_code}"]);
    }
    json!({"handler":"reverse_proxy", "upstreams":[{"dial":ctx.gateway.to_string()}],
        "stream_close_delay":"24h", "headers":{"request":{"set":set}}})
}
fn page(ctx: &RenderCtx) -> Vec<Value> {
    vec![
        json!({"handler":"rewrite","uri":"/__lantern/page"}),
        gateway(ctx, true),
    ]
}
fn php(root: &std::path::Path, ctx: &RenderCtx) -> Vec<Value> {
    vec![
        json!({"handler":"vars","root":root}),
        json!({"handler":"subroute","routes":[
            {"match":[{"file":{"try_files":["{http.request.uri.path}/index.php"]},"not":[{"path":["*/"]}]}],
             "handle":[{"handler":"static_response","status_code":308,"headers":{"Location":["./{http.request.orig_uri.path.file_escaped}/{http.request.orig_uri.prefixed_query}"]}}]},
            {"match":[{"file":{"split_path":[".php"],"try_policy":"first_exist_fallback","try_files":["{http.request.uri.path}","{http.request.uri.path}/index.php","index.php"]}}],
             "handle":[{"handler":"rewrite","uri":"{http.matchers.file.relative}"}]},
            {"match":[{"path":["*.php"]}],"handle":[{"handler":"reverse_proxy","stream_close_delay":"24h",
                "transport":{"protocol":"fastcgi","split_path":[".php"],"env":ctx.php_env},
                "upstreams":[{"dial":format!("unix/{}",ctx.php_socket.display())}]}]},
            {"handle":[{"handler":"file_server"}]}
        ]}),
    ]
}
fn handlers(target: &Target, ctx: &RenderCtx) -> Result<Vec<Value>, String> {
    let (target, tls) = match target {
        Target::Down => return Ok(page(ctx)),
        Target::Gateway => return Ok(vec![gateway(ctx, false)]),
        Target::Live { target, tls } => (target, *tls),
    };
    let (dial, tls) = match target {
        RouteTarget::Php(root) => return Ok(php(root, ctx)),
        RouteTarget::Static(root) => {
            return Ok(vec![
                json!({"handler":"file_server","root":root,"index_names":["index.html"]}),
            ]);
        }
        RouteTarget::Socket(addr) => (addr.to_string(), tls),
        RouteTarget::Url(value) => {
            let url = reqwest::Url::parse(value).map_err(|e| format!("invalid upstream URL: {e}"))?;
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
                || url.path() != "/"
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(
                    "upstream URL must be an http(s) origin without credentials, path, query or fragment"
                        .into(),
                );
            }
            let host = url.host_str().ok_or("upstream URL has no host")?;
            let port = url.port_or_known_default().ok_or("upstream URL has no port")?;
            let host = if host.contains(':') && !host.starts_with('[') {
                format!("[{host}]")
            } else {
                host.to_owned()
            };
            (format!("{host}:{port}"), url.scheme() == "https")
        }
    };
    let mut transport = json!({"protocol":"http","dial_timeout":"2s"});
    if tls {
        transport["tls"] = json!({"insecure_skip_verify":true});
    }
    Ok(vec![
        json!({"handler":"reverse_proxy","upstreams":[{"dial":dial}],"flush_interval":-1,
        "stream_close_delay":"24h","transport":transport}),
    ])
}
fn route(route: &Route, ctx: &RenderCtx) -> Result<Value, String> {
    let mut inner =
        vec![json!({"match":[{"path":["/__lantern/*"]}],"terminal":true,"handle":[gateway(ctx,false)]})];
    if route.target == Target::Gateway {
        inner.push(
            json!({"match":[{"path":["/api/*","/mcp","/mcp/*"]}],"terminal":true,
            "handle":[{"handler":"static_response","status_code":404}]}),
        );
    }
    inner.push(json!({"handle":handlers(&route.target, ctx)?}));
    Ok(json!({"match":[{"host":[route.host]}],"terminal":true,
        "handle":[{"handler":"subroute","routes":inner}]}))
}
fn listen(port: u16, proxy: &ProxySettings) -> Vec<String> {
    if proxy.bind == "all" {
        return vec![format!(":{port}")];
    }
    let mut addresses = vec![format!("127.0.0.1:{port}")];
    if proxy.ipv6 {
        addresses.push(format!("[::1]:{port}"));
    }
    addresses
}

/// Render full Caddy JSON; invalid upstream URLs are rejected before reload.
pub fn render(plan: &RoutePlan, ctx: &RenderCtx) -> Result<Value, String> {
    let mut routes = plan
        .routes
        .iter()
        .map(|r| route(r, ctx))
        .collect::<Result<Vec<_>, _>>()?;
    // Unknown hosts fall through to the gateway without asking Caddy to issue wildcard certificates.
    routes.push(json!({"handle":page(ctx)}));
    let suffix = if ctx.proxy.https_port == 443 {
        String::new()
    } else {
        format!(":{}", ctx.proxy.https_port)
    };
    Ok(json!({
        "admin":{"listen":format!("unix/{}|0600",ctx.admin_socket.display()),"config":{"persist":false}},
        "logging":{"logs":{"default":{"level":"INFO","encoder":{"format":"console"},"writer":{"output":"stderr"}}}},
        "storage":{"module":"file_system","root":ctx.storage},
        "apps":{
            "pki":{"certificate_authorities":{"local":{"name":"Lantern Local CA","install_trust":false,
                "root":{"certificate":ctx.root_cert,"private_key":ctx.root_key,"format":"pem_file"},
                "intermediate_common_name":"Lantern Local CA Intermediate","intermediate_lifetime":"720h"}}},
            "tls":{"automation":{"policies":[{"issuers":[{"module":"internal","ca":"local","lifetime":"168h"}]}]}},
            "http":{"http_port":ctx.proxy.http_port,"https_port":ctx.proxy.https_port,"servers":{
                "lantern-https":{"listen":listen(ctx.proxy.https_port,&ctx.proxy),"protocols":["h1","h2"],
                    "automatic_https":{"disable_redirects":true},"routes":routes,
                    "errors":{"routes":[{"match":[{"expression":"{http.error.status_code} in [502, 503, 504]"}],"handle":page(ctx)}]}},
                "lantern-http":{"listen":listen(ctx.proxy.http_port,&ctx.proxy),"routes":[{"handle":[{"handler":"static_response",
                    "status_code":308,"headers":{"Location":[format!("https://{{http.request.host}}{suffix}{{http.request.uri}}") ]}}]}]}
            }}
        }
    }))
}
