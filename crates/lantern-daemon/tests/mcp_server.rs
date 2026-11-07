use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use lantern_core::{
    api::{InstanceView, ProjectSummary},
    ids::InstanceId,
};
use lantern_daemon::{
    api::facade::{DaemonFacade, FacadeResult, ProjectFilter},
    mcp::{self, McpConfig},
};
use serde_json::{Value, json};
use tower::ServiceExt;

#[derive(Debug)]
struct FakeFacade;
#[async_trait]
impl DaemonFacade for FakeFacade {
    async fn projects(&self, _: ProjectFilter) -> FacadeResult<Vec<ProjectSummary>> {
        Ok(vec![serde_json::from_value(json!({"id":"myapp","aliases":[],"name":"My App","root":"/fixture/myapp", "repo":null,"kind":"project","enabled":true,"tags":[],"registered_at":"2026-11-06T17:52:03.412Z","source":"manifest","primary":"myapp","status":"stopped","url":"https://myapp.glow", "port":null,"instance_count":1,"docs_count":0,"endpoints_count":0,"routing":null})).unwrap()])
    }
    async fn instance(&self, id: InstanceId) -> FacadeResult<InstanceView> {
        Ok(serde_json::from_value(json!({"id":id,"project":"myapp","checkout":{"path":"/fixture/myapp","git_root":null,"kind":"primary","branch":"main","head":null,"dirty":null,"locked":null,"missing":false},"domain":"myapp.glow","url":"https://myapp.glow","pinned":false,"adopt":false,"visible":true,"setup_stale":false,"runtime":{"status":"stopped","port":null,"unit":null,"main_pid":null,"started_at":null,"last_exit":null,"restarts":0,"next_retry_at":null,"health":{"state":"unknown","checked_at":null,"latency_ms":null,"consecutive_failures":0,"last_error":null,"recent":[]},"message":null,"issue":null,"upstream":null,"external":null},"manifest":{"id":"myapp","serve":"proxy","run_cmd":null,"diagnostics":{"errors":0,"warnings":0,"info":0},"manifest_rev":"abc"}})).unwrap())
    }
}
fn router() -> Router {
    Router::new().nest_service("/mcp", mcp::service(Arc::new(FakeFacade), McpConfig::default()))
}
fn request(body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "127.0.0.1:39800")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-11-25")
        .body(Body::from(body.to_string()))
        .unwrap()
}
async fn rpc(method: &str, params: Value) -> Value {
    let response = router()
        .oneshot(request(
            json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
        ))
        .await
        .unwrap();
    let status = response.status();
    assert!(!response.headers().contains_key("mcp-session-id"));
    let bytes = to_bytes(response.into_body(), 4_000_000).await.unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&bytes));
    serde_json::from_slice(&bytes).unwrap()
}
#[tokio::test]
async fn mcp_tools_list_golden_and_fake_call() {
    let list = rpc("tools/list", json!({})).await;
    let tools = &list["result"]["tools"];
    let golden: Value = serde_json::from_str(include_str!("../src/mcp/tools_golden.json")).unwrap();
    assert_eq!(*tools, golden);
    let call = rpc("tools/call", json!({"name":"list_projects","arguments":{}})).await;
    let projects: Value =
        serde_json::from_str(call["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(projects[0]["id"], "myapp");
    let ports = rpc("tools/call", json!({"name":"get_ports","arguments":{}})).await;
    assert_eq!(ports["result"]["isError"], true);
}
#[tokio::test]
#[ignore = "real system: binds a loopback TCP socket in the mcp lane range"]
async fn mcp_socket_initialize_list_call_and_stateless() {
    let mut listener = None;
    for port in 39800..39900 {
        if let Ok(l) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            listener = Some(l);
            break;
        }
    }
    let listener = listener.expect("a free lane port");
    let addr = listener.local_addr().unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let stop = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router())
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .unwrap()
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    for (method, params) in [
        (
            "initialize",
            json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"0"}}),
        ),
        ("tools/list", json!({})),
        ("tools/call", json!({"name":"list_projects","arguments":{}})),
    ] {
        let response = client
            .post(format!("http://{addr}/mcp"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-11-25")
            .header("mcp-session-id", "old-session")
            .body(json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key("mcp-session-id"));
        let v: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert!(v.get("result").is_some(), "{v}");
    }
    let response = client.get(format!("http://{addr}/mcp")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    shutdown.cancel();
    server.await.unwrap();
}
