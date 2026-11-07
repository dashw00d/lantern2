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
    std::fs::write(
        concat!(env!("CARGO_MANIFEST_DIR"), "/src/mcp/tools_golden.json"),
        serde_json::to_string_pretty(tools).unwrap() + "\n",
    )
    .unwrap();
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
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let stop = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router())
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .unwrap()
    });
    let response = client.post(format!("http://{addr}/mcp"))
        .header("content-type", "application/json").header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-11-25").header("mcp-session-id", "old-session")
        .body(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_projects","arguments":{}}}).to_string())
        .send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key("mcp-session-id"));
    let value: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert!(value.get("result").is_some());
    shutdown.cancel();
    server.await.unwrap();
}

#[derive(Debug)]
struct RiskFacade;
#[async_trait]
impl DaemonFacade for RiskFacade {
    async fn projects(&self, filter: ProjectFilter) -> FacadeResult<Vec<ProjectSummary>> {
        FakeFacade.projects(filter).await
    }
    async fn instance(&self, id: InstanceId) -> FacadeResult<InstanceView> {
        FakeFacade.instance(id).await
    }
    async fn manifest(&self, _: InstanceId) -> FacadeResult<Value> {
        Ok(
            json!({"base_url":"http://127.0.0.1:39801","endpoints":[{"method":"GET","path":"/manifest-only","risk":"critical"}],"routing":{"risk":null,"requires_confirmation":false,"max_concurrent":1}}),
        )
    }
    async fn endpoints(
        &self,
        _: InstanceId,
        _: Option<String>,
        _: Option<u32>,
    ) -> FacadeResult<Vec<lantern_core::api::EndpointEntry>> {
        Ok(vec![serde_json::from_value(json!({"method":"POST","path":"/danger/{id}","description":null,"category":null,"risk":"critical","body_hint":null,"params":[],"source":"manual"})).unwrap()])
    }
}
#[derive(Debug, Default)]
struct RecordingClient(std::sync::atomic::AtomicUsize);
#[async_trait]
impl mcp::upstream::ToolApiClient for RecordingClient {
    async fn send(
        &self,
        req: mcp::upstream::ToolApiRequest,
    ) -> Result<mcp::upstream::ToolApiResponse, String> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(req.addresses.iter().all(|addr| addr.ip().is_loopback()));
        Ok(mcp::upstream::ToolApiResponse {
            status: 200,
            content_type: Some("application/json".into()),
            body: b"ok".to_vec(),
            truncated: false,
        })
    }
}
async fn api_call(router: Router, arguments: Value) -> Value {
    let response=router.oneshot(request(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"call_tool_api","arguments":arguments}}))).await.unwrap();
    serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap()).unwrap()
}
#[tokio::test]
async fn mcp_risk_and_ssrf_gates_run_before_transport() {
    let client = Arc::new(RecordingClient::default());
    let config = McpConfig {
        api_client: client.clone(),
        trust_risk_metadata: true,
        ..Default::default()
    };
    let router = Router::new().nest_service("/mcp", mcp::service(Arc::new(RiskFacade), config));
    for arguments in [
        json!({"tool":"myapp","path":"/danger/1","method":"POST"}),
        json!({"tool":"myapp","path":"/danger/1","method":"POST","confirm":true}),
        json!({"tool":"myapp","path":"/manifest-only","method":"GET","confirm":true}),
        json!({"tool":"myapp","path":"/%252e%252e/secret"}),
        json!({"tool":"myapp","path":"//169.254.169.254"}),
        json!({"tool":"myapp","path":"/ok","method":"TRACE"}),
    ] {
        let response = api_call(router.clone(), arguments).await;
        assert_eq!(response["result"]["isError"], true, "{response}");
    }
    assert_eq!(client.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    let response = api_call(router, json!({"tool":"myapp","path":"/ok","method":"GET"})).await;
    assert_eq!(response["result"]["isError"], false, "{response}");
    assert_eq!(client.0.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[derive(Debug)]
struct CompleteFake;
fn ticket() -> lantern_daemon::api::facade::OperationTicket {
    lantern_daemon::api::facade::OperationTicket {
        id: lantern_core::api::OperationId("op-fake".into()),
        instance: None,
        created: None,
        forgotten: None,
    }
}
#[async_trait]
impl DaemonFacade for CompleteFake {
    async fn projects(&self, f: ProjectFilter) -> FacadeResult<Vec<ProjectSummary>> {
        FakeFacade.projects(f).await
    }
    async fn instance(&self, id: InstanceId) -> FacadeResult<InstanceView> {
        FakeFacade.instance(id).await
    }
    async fn instances(
        &self,
        _: lantern_daemon::api::facade::InstanceFilter,
    ) -> FacadeResult<Vec<InstanceView>> {
        Ok(vec![self.instance(InstanceId::parse("myapp").unwrap()).await?])
    }
    async fn project_instances(&self, _: lantern_core::ids::ProjectId) -> FacadeResult<Vec<InstanceView>> {
        self.instances(Default::default()).await
    }
    async fn project(
        &self,
        _: lantern_core::ids::ProjectId,
    ) -> FacadeResult<lantern_core::api::ProjectDetail> {
        let summary = self.projects(Default::default()).await?.remove(0);
        let view = self.instance(InstanceId::parse("myapp").unwrap()).await?;
        Ok(lantern_core::api::ProjectDetail {
            summary,
            instances: vec![view.clone()],
            depends_on: vec![],
            manifest: view.manifest,
        })
    }
    async fn manifest(&self, _: InstanceId) -> FacadeResult<Value> {
        Ok(
            json!({"base_url":"http://127.0.0.1:39801","endpoints":[],"description":"Example","run":{"cmd":"serve","env":{"API_TOKEN":"secret","PUBLIC":"ok"}},"routing":{"risk":null,"requires_confirmation":false,"max_concurrent":1}}),
        )
    }
    async fn start(&self, _: InstanceId) -> FacadeResult<lantern_daemon::api::facade::OperationTicket> {
        Ok(ticket())
    }
    async fn stop(&self, _: InstanceId) -> FacadeResult<lantern_daemon::api::facade::OperationTicket> {
        Ok(ticket())
    }
    async fn restart(&self, _: InstanceId) -> FacadeResult<lantern_daemon::api::facade::OperationTicket> {
        Ok(ticket())
    }
    async fn run_branch(
        &self,
        _: lantern_core::ids::ProjectId,
        _: lantern_core::api::CreateInstanceRequest,
    ) -> FacadeResult<lantern_daemon::api::facade::OperationTicket> {
        Ok(ticket())
    }
    async fn logs(
        &self,
        id: InstanceId,
        _: lantern_core::api::LogParams,
    ) -> FacadeResult<lantern_core::api::LogPage> {
        Ok(lantern_core::api::LogPage {
            instance: id,
            lines: vec![],
            buffered: 0,
            truncated: false,
        })
    }
    async fn docs(&self, _: InstanceId) -> FacadeResult<Vec<lantern_core::api::DocEntry>> {
        Ok(vec![lantern_core::api::DocEntry {
            path: "README.md".into(),
            kind: "readme".into(),
            source: lantern_core::api::IntelSource::Manual,
            exists: true,
            size: Some(5),
            mtime: None,
        }])
    }
    async fn doc(
        &self,
        _: InstanceId,
        path: String,
        _: Option<u64>,
        _: Option<u64>,
    ) -> FacadeResult<lantern_core::api::DocContent> {
        Ok(lantern_core::api::DocContent {
            path,
            content: "hello".into(),
            offset: 0,
            total_chars: 5,
            truncated: false,
        })
    }
    async fn endpoints(
        &self,
        _: InstanceId,
        _: Option<String>,
        _: Option<u32>,
    ) -> FacadeResult<Vec<lantern_core::api::EndpointEntry>> {
        Ok(vec![])
    }
    async fn intel(&self, id: InstanceId, _: Option<u32>) -> FacadeResult<lantern_core::api::IntelSnapshot> {
        Ok(serde_json::from_value(json!({"instance":id,"refreshed_at":null,"stale":false,"docs":[],"endpoints":[],"discovery":{"refreshed_at":null,"docs":{"enabled":true,"count":0,"source_count":0,"sources":[],"skipped_large":0,"truncated":false},"api":{"enabled":false,"count":0,"source_count":0,"sources":[],"errors":[]}}})).unwrap())
    }
    async fn refresh_intel(&self, id: InstanceId) -> FacadeResult<lantern_core::api::IntelSnapshot> {
        self.intel(id, None).await
    }
    async fn refresh_all_intel(&self) -> FacadeResult<lantern_core::api::IntelRefreshReport> {
        Ok(lantern_core::api::IntelRefreshReport {
            refreshed: 1,
            errors: vec![],
            took_ms: 0,
        })
    }
    async fn check_health(&self, _: InstanceId) -> FacadeResult<lantern_core::model::HealthSummary> {
        Ok(Default::default())
    }
    async fn system_health(&self) -> FacadeResult<lantern_core::api::SystemHealth> {
        Ok(lantern_core::api::SystemHealth {
            ok: true,
            checked_at: lantern_core::Timestamp::now(),
            checks: vec![],
        })
    }
    async fn dependencies(
        &self,
        _: Option<lantern_core::ids::ProjectId>,
    ) -> FacadeResult<lantern_core::api::DependencyGraph> {
        Ok(lantern_core::api::DependencyGraph(Default::default()))
    }
    async fn ports(
        &self,
        _: Option<lantern_core::ids::ProjectId>,
    ) -> FacadeResult<lantern_core::api::PortsView> {
        Ok(lantern_core::api::PortsView {
            instances: Default::default(),
            unmanaged: vec![],
            conflicts: vec![],
        })
    }
    async fn listeners(
        &self,
        _: lantern_daemon::api::facade::ListenerFilter,
    ) -> FacadeResult<Vec<lantern_core::model::Listener>> {
        Ok(vec![])
    }
}
async fn rpc_using(router: Router, method: &str, params: Value) -> Value {
    let response = router
        .oneshot(request(
            json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&to_bytes(response.into_body(), 4_000_000).await.unwrap()).unwrap()
}
#[tokio::test]
async fn mcp_all_tools_prompts_resources_against_fake() {
    let client = Arc::new(RecordingClient::default());
    let config = McpConfig {
        api_client: client,
        trust_risk_metadata: true,
        ..Default::default()
    };
    let router = Router::new().nest_service("/mcp", mcp::service(Arc::new(CompleteFake), config));
    for (tool, args) in [
        ("list_projects", json!({})),
        ("search_projects", json!({"query":"My"})),
        ("get_project", json!({"name":"myapp"})),
        ("list_instances", json!({"project":"myapp"})),
        ("start_project", json!({"name":"myapp","wait_seconds":"1"})),
        ("stop_project", json!({"name":"myapp"})),
        ("restart_project", json!({"name":"myapp"})),
        (
            "start_branch",
            json!({"name":"myapp","branch":"feature/x","wait_seconds":0}),
        ),
        ("get_project_logs", json!({"name":"myapp","tail":"5"})),
        ("get_project_docs", json!({"name":"myapp","path":"README.md"})),
        ("get_project_endpoints", json!({"name":"myapp"})),
        ("get_project_discovery", json!({"name":"myapp"})),
        ("refresh_discovery", json!({"name":"myapp"})),
        ("check_health", json!({})),
        ("get_dependencies", json!({})),
        ("get_ports", json!({})),
        ("list_listeners", json!({"port":"39801"})),
        ("list_tools", json!({})),
        ("call_tool_api", json!({"tool":"myapp","path":"/ok"})),
    ] {
        let response = rpc_using(
            router.clone(),
            "tools/call",
            json!({"name":tool,"arguments":args}),
        )
        .await;
        assert_eq!(response["result"]["isError"], false, "{tool}: {response}");
        if tool == "get_project" {
            let text = response["result"]["content"][0]["text"].as_str().unwrap();
            assert!(text.contains("***"));
            assert!(!text.contains("secret"));
        }
    }
    let submit = rpc_using(
        router.clone(),
        "tools/call",
        json!({"name":"call_tool_api","arguments":{"tool":"myapp","path":"/ok","async":true}}),
    )
    .await;
    let submit: Value =
        serde_json::from_str(submit["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let id = submit["job_id"].as_str().unwrap();
    for name in ["get_operation", "get_job_result"] {
        let key = if name == "get_operation" {
            "operation_id"
        } else {
            "job_id"
        };
        let response = rpc_using(
            router.clone(),
            "tools/call",
            json!({"name":name,"arguments":{key:id}}),
        )
        .await;
        assert_eq!(response["result"]["isError"], false, "{name}: {response}");
    }
    for (method, params) in [
        ("resources/list", json!({})),
        ("resources/templates/list", json!({})),
        (
            "resources/read",
            json!({"uri":"lantern:///projects/myapp/docs/README.md"}),
        ),
        ("prompts/list", json!({})),
        (
            "prompts/get",
            json!({"name":"diagnose_service","arguments":{"name":"myapp"}}),
        ),
        (
            "prompts/get",
            json!({"name":"dependency_trace","arguments":{"name":"myapp"}}),
        ),
    ] {
        let response = rpc_using(router.clone(), method, params).await;
        assert!(response.get("result").is_some(), "{method}: {response}");
    }
}

#[tokio::test]
async fn mcp_critical_calls_need_setting_and_confirmation() {
    let client = Arc::new(RecordingClient::default());
    let config = McpConfig {
        api_client: client.clone(),
        trust_risk_metadata: true,
        allow_critical_calls: true,
        ..Default::default()
    };
    let router = Router::new().nest_service("/mcp", mcp::service(Arc::new(RiskFacade), config));
    let blocked = api_call(
        router.clone(),
        json!({"tool":"myapp","path":"/danger/1","method":"POST"}),
    )
    .await;
    assert_eq!(blocked["result"]["isError"], true);
    assert_eq!(client.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    let allowed = api_call(
        router,
        json!({"tool":"myapp","path":"/danger/1","method":"POST","confirm":true}),
    )
    .await;
    assert_eq!(allowed["result"]["isError"], false);
    assert_eq!(client.0.load(std::sync::atomic::Ordering::SeqCst), 1);
}
#[derive(Debug)]
struct ImageClient;
#[async_trait]
impl mcp::upstream::ToolApiClient for ImageClient {
    async fn send(&self, _: mcp::upstream::ToolApiRequest) -> Result<mcp::upstream::ToolApiResponse, String> {
        Ok(mcp::upstream::ToolApiResponse {
            status: 200,
            content_type: Some("image/png".into()),
            body: vec![137, 80, 78, 71],
            truncated: false,
        })
    }
}
#[tokio::test]
async fn mcp_tool_api_image_result_is_an_image_block() {
    let config = McpConfig {
        api_client: Arc::new(ImageClient),
        trust_risk_metadata: true,
        ..Default::default()
    };
    let router = Router::new().nest_service("/mcp", mcp::service(Arc::new(CompleteFake), config));
    let response = api_call(router, json!({"tool":"myapp","path":"/ok"})).await;
    assert_eq!(response["result"]["content"][0]["type"], "image");
    assert_eq!(response["result"]["content"][0]["mimeType"], "image/png");
    assert_eq!(response["result"]["content"][0]["data"], "iVBORw==");
}

#[tokio::test]
#[ignore = "real system: binds a loopback TCP socket in the mcp lane range"]
async fn mcp_upstream_http_redirect_and_response_cap() {
    use mcp::upstream::ToolApiClient;
    let mut listener = None;
    for port in 39800..39900 {
        if let Ok(l) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            listener = Some(l);
            break;
        }
    }
    let listener = listener.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route(
            "/redirect",
            axum::routing::get(|| async {
                axum::response::Redirect::temporary("http://127.0.0.1:4777/never")
            }),
        )
        .route("/large", axum::routing::get(|| async { "1234567890" }));
    let shutdown = tokio_util::sync::CancellationToken::new();
    let stop = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .unwrap()
    });
    let client = mcp::upstream::ReqwestToolApiClient;
    let policy = mcp::upstream::UpstreamPolicy::default();
    let base = format!("http://{addr}");
    let redir = policy
        .prepare(
            &base,
            "/redirect",
            "GET",
            None,
            std::time::Duration::from_secs(3),
            100,
            "application/json",
        )
        .await
        .unwrap();
    assert_eq!(client.send(redir).await.unwrap().status, 307);
    let large = policy
        .prepare(
            &base,
            "/large",
            "GET",
            None,
            std::time::Duration::from_secs(3),
            4,
            "application/json",
        )
        .await
        .unwrap();
    let result = client.send(large).await.unwrap();
    assert_eq!(result.body, b"1234");
    assert!(result.truncated);
    shutdown.cancel();
    server.await.unwrap();
}
