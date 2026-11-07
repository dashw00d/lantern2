use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use futures_util::StreamExt;
use lantern_client::{
    Client, ClientConfig, EventsOptions, InstanceFilter as ClientInstanceFilter, StreamEvent, Wait,
};
use lantern_core::{
    api::{ApiError, Event, InstanceView, OperationKind, ProjectSummary, WireEvent},
    ids::InstanceId,
    model::LogLine,
};
use lantern_daemon::{
    api::{
        self, ApiConfig, ApiState,
        facade::{DaemonFacade, FacadeResult, InstanceFilter, OperationTicket},
    },
    events::EventBus,
    operations::{OpHandle, OpOutcome, OperationTracker},
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::broadcast;
use tower::ServiceExt;

fn inst() -> InstanceView {
    serde_json::from_value(json!({"id":"myapp@feature-x", "project":"myapp", "checkout":{"path":"/fixture/myapp","git_root":null,
        "kind":"managed","branch":null,"head":null,"dirty":null,"locked":null,"missing":false},
        "domain":"feature-x.myapp.glow","url":"https://feature-x.myapp.glow","pinned":false,"adopt":false,"visible":true,"setup_stale":false,
        "runtime":{"status":"stopped","port":null,"unit":null,"main_pid":null,"started_at":null,"last_exit":null,"restarts":0,
            "next_retry_at":null,"health":{"state":"unknown","checked_at":null,"latency_ms":null,"consecutive_failures":0,"last_error":null,"recent":[]},
            "message":null,"issue":null,"upstream":null,"external":null},"manifest":{"id":"myapp", "serve":"proxy", "run_cmd":null, "diagnostics":{"errors":0,"warnings":0,"info":0},"manifest_rev":"abc"}})).unwrap()
}
fn summary() -> ProjectSummary {
    serde_json::from_value(json!({"id":"myapp","aliases":[],"name":"My App","root":"/fixture/myapp", "repo":null,
        "kind":"project","enabled":true,"tags":[],"registered_at":"2026-11-06T17:52:03.412Z","source":"manifest",
        "primary":"myapp","status":"stopped","url":"https://myapp.glow", "port":null,"instance_count":1,"docs_count":0,"endpoints_count":0,"routing":null})).unwrap()
}
fn line(text: &str) -> LogLine {
    serde_json::from_value(json!({"instance":"myapp@feature-x", "stream":"stdout", "line":text, "ts":"2026-11-06T17:52:03.412Z"})).unwrap()
}
#[derive(Debug, Clone, Copy)]
enum Mode {
    Success,
    Running,
    Failure,
}
#[derive(Debug)]
struct Fake {
    ops: OperationTracker,
    mode: Mode,
    pending: Mutex<Vec<OpHandle>>,
    logs: broadcast::Sender<LogLine>,
}
impl Fake {
    fn new(ops: OperationTracker, mode: Mode) -> Self {
        Self {
            ops,
            mode,
            pending: Mutex::new(Vec::new()),
            logs: broadcast::channel(2).0,
        }
    }
    fn start_op(&self, id: InstanceId) -> OperationTicket {
        let handle = self.ops.begin(OperationKind::Start, Some(id));
        let op_id = handle.id().clone();
        match self.mode {
            Mode::Success => {
                handle.finish(OpOutcome::Noop);
            }
            Mode::Running => self.pending.lock().unwrap().push(handle),
            Mode::Failure => {
                handle.finish(OpOutcome::Failed(ApiError {
                    code: "needs_config".into(),
                    message: "missing command".into(),
                    details: json!({"field":"run.cmd"}),
                }));
            }
        }
        OperationTicket {
            id: op_id,
            instance: Some(inst()),
            created: None,
            forgotten: None,
        }
    }
}
#[async_trait]
impl DaemonFacade for Fake {
    async fn instances(&self, _: InstanceFilter) -> FacadeResult<Vec<InstanceView>> {
        Ok(vec![inst()])
    }
    async fn instance(&self, id: InstanceId) -> FacadeResult<InstanceView> {
        if id == inst().id {
            Ok(inst())
        } else {
            Err(ApiError {
                code: "instance_not_found".into(),
                message: "missing".into(),
                details: json!({}),
            })
        }
    }
    async fn start(&self, id: InstanceId) -> FacadeResult<OperationTicket> {
        Ok(self.start_op(id))
    }
    async fn event_snapshot(&self) -> FacadeResult<Vec<Event>> {
        Ok(vec![
            Event::ProjectUpdated { project: summary() },
            Event::InstanceUpdated { instance: inst() },
            Event::ListenersChanged {
                seq: 1,
                listeners: vec![],
            },
        ])
    }
    async fn subscribe_logs(
        &self,
        _: InstanceId,
        tail: usize,
    ) -> FacadeResult<(Vec<LogLine>, broadcast::Receiver<LogLine>)> {
        Ok((
            if tail > 0 { vec![line("backlog")] } else { vec![] },
            self.logs.subscribe(),
        ))
    }
}
fn setup(mode: Mode, capacity: usize, port: u16) -> (Router, Arc<Fake>, EventBus) {
    let bus = EventBus::with_capacity(capacity);
    let ops = OperationTracker::new(bus.clone());
    let fake = Arc::new(Fake::new(ops.clone(), mode));
    let mut config = ApiConfig::new(port, "glow", "test-boot");
    config.sse_ping_interval = Duration::from_millis(50);
    let state = ApiState::new(fake.clone(), bus.clone(), ops, config);
    (api::router(state), fake, bus)
}
fn request(method: &str, uri: &str, body: &str) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:39400");
    if !body.is_empty() {
        b = b.header("content-type", "application/json");
    }
    b.body(Body::from(body.to_owned())).unwrap()
}
async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 2_000_000).await.unwrap()).unwrap()
}

#[tokio::test]
async fn api_guard_and_error_envelopes() {
    let (router, _, _) = setup(Mode::Success, 16, 39400);
    for (header, value, code) in [
        ("host", "evil.example:39400", "forbidden_host"),
        ("host", "127.0.0.1.evil.example:39400", "forbidden_host"),
        ("origin", "https://evil.example", "forbidden_origin"),
        ("origin", "null", "forbidden_origin"),
        ("sec-fetch-site", "cross-site", "forbidden_origin"),
    ] {
        let mut req = request("GET", "/api", "");
        req.headers_mut().insert(header, value.parse().unwrap());
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(res.headers()["content-type"], "application/json; charset=utf-8");
        assert_eq!(res.headers()["x-content-type-options"], "nosniff");
        assert!(res.headers().get("access-control-allow-origin").is_none());
        assert_eq!(json_body(res).await["error"]["code"], code);
    }
    let req = Request::builder()
        .method("POST")
        .uri("/api/projects")
        .header("host", "localhost:39400")
        .header("content-type", "text/plain")
        .body(Body::from("{}"))
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(json_body(res).await["error"]["code"], "unsupported_media_type");
    let res = router
        .clone()
        .oneshot(request("POST", "/api/projects", "{"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(res).await["error"]["code"], "invalid_json");
    let huge = "x".repeat(1024 * 1024 + 1);
    let res = router
        .clone()
        .oneshot(request("POST", "/api/projects", &huge))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json_body(res).await["error"]["code"], "payload_too_large");
    let res = router
        .clone()
        .oneshot(request("OPTIONS", "/api", ""))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(json_body(res).await["error"]["code"], "method_not_allowed");
    let res = router.oneshot(request("GET", "/api/missing", "")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_eq!(json_body(res).await["error"]["code"], "not_found");
}
#[tokio::test]
async fn api_wait_success_running_and_failure() {
    for (mode, query, status, code) in [
        (Mode::Success, "?wait=true", StatusCode::OK, None),
        (Mode::Running, "?wait=true&timeout=0", StatusCode::ACCEPTED, None),
        (Mode::Running, "", StatusCode::ACCEPTED, None),
        (
            Mode::Failure,
            "?wait=true",
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("needs_config"),
        ),
    ] {
        let (r, _fake, _) = setup(mode, 16, 39400);
        let res = r
            .oneshot(request(
                "POST",
                &format!("/api/instances/myapp%40feature-x/start{query}"),
                "{}",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), status);
        let value = json_body(res).await;
        if let Some(code) = code {
            assert_eq!(value["error"]["code"], code);
            assert!(value["error"]["details"]["operation_id"].is_string());
            assert_eq!(value["error"]["details"]["field"], "run.cmd");
        } else {
            assert_eq!(value["instance"]["id"], "myapp@feature-x");
        }
    }
}
#[tokio::test(start_paused = true)]
async fn api_sse_snapshot_live_logs_ping_and_lag() {
    let (r, fake, bus) = setup(Mode::Success, 2, 39400);
    let res = r
        .oneshot(request("GET", "/api/events?logs=myapp%40feature-x&tail=1", ""))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let mut stream = res.into_body().into_data_stream();
    let mut start = String::new();
    while !start.contains("event: snapshot_complete") {
        let chunk = stream.next().await.unwrap().unwrap();
        start.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    let hello = start.find("event: hello").unwrap();
    let project = start.find("event: project_updated").unwrap();
    let instance = start.find("event: instance_updated").unwrap();
    let backlog = start.find("event: log").unwrap();
    let end = start.find("event: snapshot_complete").unwrap();
    assert!(hello < project && project < instance && instance < backlog && backlog < end);
    let ping = stream.next().await.unwrap().unwrap();
    assert!(std::str::from_utf8(&ping).unwrap().contains(": ping"));
    fake.logs.send(line("live")).unwrap();
    let live = stream.next().await.unwrap().unwrap();
    assert!(std::str::from_utf8(&live).unwrap().contains("live"));
    for seq in 0..10 {
        bus.publish(Event::ListenersChanged {
            seq,
            listeners: vec![],
        });
    }
    let lag = stream.next().await.unwrap().unwrap();
    let lag = std::str::from_utf8(&lag).unwrap();
    assert!(lag.contains("event: resync") && lag.contains("lagged"));
    assert!(stream.next().await.is_none());
}
#[tokio::test]
async fn api_sse_state_filter_and_operation_progress() {
    let (r, fake, bus) = setup(Mode::Success, 16, 39400);
    let mut stream = r
        .oneshot(request(
            "GET",
            "/api/events?state=false&op=op-test&logs=myapp%40feature-x&tail=0",
            "",
        ))
        .await
        .unwrap()
        .into_body()
        .into_data_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert!(std::str::from_utf8(&first).unwrap().contains("event: hello"));
    let second = stream.next().await.unwrap().unwrap();
    assert!(
        std::str::from_utf8(&second)
            .unwrap()
            .contains("event: snapshot_complete")
    );
    bus.publish(Event::ListenersChanged {
        seq: 1,
        listeners: vec![],
    });
    bus.publish(Event::OperationProgress {
        op: "op-test".to_string().into(),
        instance: inst().id,
        step: "ready".into(),
        done: true,
        error: None,
    });
    let next = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        std::str::from_utf8(&next)
            .unwrap()
            .contains("event: operation_progress")
    );
    fake.logs.send(line("live")).unwrap();
    let next = stream.next().await.unwrap().unwrap();
    assert!(std::str::from_utf8(&next).unwrap().contains("event: log"));
}

#[tokio::test]
async fn api_client_real_socket() {
    let mut listener = None;
    for port in 39400..39500 {
        if let Ok(bound) = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            listener = Some(bound);
            break;
        }
    }
    let listener = listener.expect("free server-lane port");
    let port = listener.local_addr().unwrap().port();
    let (router, fake, bus) = setup(Mode::Success, 16, port);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(api::serve(listener, router, shutdown.clone()));
    let client = Client::new(ClientConfig {
        base_url: Some(format!("http://127.0.0.1:{port}")),
    })
    .unwrap();
    assert_eq!(client.ping().await.unwrap().api_version, 1);
    assert_eq!(
        client.instances(ClientInstanceFilter::default()).await.unwrap()[0].id,
        inst().id
    );
    assert_eq!(client.instance(&inst().id).await.unwrap().id, inst().id);
    assert!(
        client
            .start(
                &inst().id,
                Wait {
                    wait: true,
                    timeout: Some(Duration::from_secs(1))
                }
            )
            .await
            .unwrap()
            .operation
            .noop
    );
    let mut stream = client.events_once(EventsOptions {
        logs: vec![inst().id],
        tail: 1,
        ..EventsOptions::default()
    });
    let mut types = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap()
        {
            StreamEvent::Event(event) => {
                let done = matches!(event, WireEvent::SnapshotComplete);
                types.push(event.event_name());
                if done {
                    break;
                }
            }
            StreamEvent::Lost { error, .. } => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(
        types,
        [
            "hello",
            "project_updated",
            "instance_updated",
            "listeners_changed",
            "log",
            "snapshot_complete"
        ]
    );
    fake.logs.send(line("socket live")).unwrap();
    bus.publish(Event::ListenersChanged {
        seq: 2,
        listeners: vec![],
    });
    let mut got_log = false;
    let mut got_state = false;
    for _ in 0..2 {
        if let StreamEvent::Event(event) = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap()
        {
            got_log |= matches!(&event, WireEvent::Log { line } if line.line == "socket live");
            got_state |= matches!(&event, WireEvent::ListenersChanged { seq: 2, .. });
        }
    }
    assert!(got_log && got_state);
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(stream);
}

#[tokio::test]
async fn api_route_table_delegates_and_returns_envelopes() {
    let (r, _, _) = setup(Mode::Success, 16, 39400);
    let cases = [
        ("GET", "/api/system", ""),
        ("GET", "/api/system/health", ""),
        ("POST", "/api/system/reconcile", ""),
        ("POST", "/api/system/stop-all", "{}"),
        ("GET", "/api/settings", ""),
        ("PATCH", "/api/settings", "{}"),
        ("POST", "/api/settings/reload", ""),
        ("GET", "/api/projects", ""),
        ("POST", "/api/projects", "{\"path\":\"/fixture\"}"),
        ("POST", "/api/projects/scan", "{}"),
        ("GET", "/api/projects/myapp", ""),
        ("PATCH", "/api/projects/myapp", "{}"),
        ("DELETE", "/api/projects/myapp", ""),
        ("POST", "/api/projects/myapp/reload", "{}"),
        ("POST", "/api/projects/myapp/sync", ""),
        ("GET", "/api/projects/myapp/instances", ""),
        ("POST", "/api/projects/myapp/instances", "{\"branch\":\"main\"}"),
        ("GET", "/api/projects/myapp/checkouts", ""),
        ("GET", "/api/projects/myapp/branches", ""),
        ("GET", "/api/dependencies", ""),
        ("PATCH", "/api/instances/myapp%40feature-x", "{\"pinned\":true}"),
        ("DELETE", "/api/instances/myapp%40feature-x", ""),
        ("POST", "/api/instances/myapp%40feature-x/stop", "{}"),
        ("POST", "/api/instances/myapp%40feature-x/restart", "{}"),
        ("POST", "/api/instances/myapp%40feature-x/prepare", "{}"),
        ("POST", "/api/instances/myapp%40feature-x/pin", ""),
        ("POST", "/api/instances/myapp%40feature-x/unpin", ""),
        (
            "POST",
            "/api/instances/myapp%40feature-x/rename",
            "{\"slug\":\"x\"}",
        ),
        ("GET", "/api/instances/myapp%40feature-x/removal-check", ""),
        ("POST", "/api/instances/myapp%40feature-x/adopt", "{}"),
        ("POST", "/api/instances/myapp%40feature-x/unadopt", ""),
        ("GET", "/api/instances/myapp%40feature-x/logs", ""),
        ("POST", "/api/instances/myapp%40feature-x/health/check", ""),
        ("GET", "/api/instances/myapp%40feature-x/manifest", ""),
        ("GET", "/api/instances/myapp%40feature-x/intel", ""),
        ("POST", "/api/instances/myapp%40feature-x/intel/refresh", ""),
        ("POST", "/api/intel/refresh", ""),
        ("GET", "/api/instances/myapp%40feature-x/docs", ""),
        ("GET", "/api/instances/myapp%40feature-x/docs/sub/file", ""),
        ("GET", "/api/instances/myapp%40feature-x/endpoints", ""),
        ("GET", "/api/listeners", ""),
        ("POST", "/api/listeners/scan", ""),
        ("GET", "/api/ports", ""),
        ("GET", "/api/services", ""),
        ("GET", "/api/services/redis", ""),
        ("POST", "/api/services/redis/start", ""),
        ("POST", "/api/services/redis/stop", ""),
        ("POST", "/api/services/redis/restart", ""),
        ("GET", "/api/profiles", ""),
        ("GET", "/api/profiles/demo", ""),
        ("PUT", "/api/profiles/demo", "{}"),
        ("DELETE", "/api/profiles/demo", ""),
        ("POST", "/api/profiles/demo/activate", ""),
        ("GET", "/api/templates", ""),
        ("GET", "/api/templates/demo", ""),
        ("POST", "/api/templates", "{}"),
        ("PUT", "/api/templates/demo", "{}"),
        ("DELETE", "/api/templates/demo", ""),
        ("GET", "/api/suggestions", ""),
        ("POST", "/api/suggestions/dismiss", "{\"path\":\"/fixture\"}"),
        ("POST", "/api/import/v1", "{}"),
        ("GET", "/api/schema/manifest", ""),
    ];
    for (method, path, payload) in cases {
        let res = r.clone().oneshot(request(method, path, payload)).await.unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE, "{method} {path}");
        assert_eq!(
            json_body(res).await["error"]["code"],
            "unavailable",
            "{method} {path}"
        );
    }
    for path in ["/api/operations/no-such-id", "/api/operations/no-such-id/cancel"] {
        let method = if path.ends_with("cancel") { "POST" } else { "GET" };
        let res = r.clone().oneshot(request(method, path, "")).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(json_body(res).await["error"]["code"], "operation_not_found");
    }
}

#[tokio::test]
async fn api_sse_limit_is_released_on_disconnect() {
    let (router, _, _) = setup(Mode::Success, 16, 39400);
    let mut responses = Vec::new();
    for _ in 0..32 {
        let res = router
            .clone()
            .oneshot(request("GET", "/api/events?state=false", ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        responses.push(res);
    }
    let res = router
        .clone()
        .oneshot(request("GET", "/api/events?state=false", ""))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(res).await["error"]["code"], "unavailable");
    responses.pop();
    assert_eq!(
        router
            .oneshot(request("GET", "/api/events?state=false", ""))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn api_guard_rebinding_and_allowed_origins() {
    let (router, _, _) = setup(Mode::Success, 16, 39400);
    for host in [
        "127.0.0.1:39400",
        "localhost:39400",
        "lantern.glow",
        "lantern.glow:443",
    ] {
        let mut req = request("GET", "/api", "");
        req.headers_mut().insert("host", host.parse().unwrap());
        let response = router.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{host}");
    }
    for host in [
        "127.0.0.1:39401",
        "localhost.evil:39400",
        "lantern.glow.evil",
        "lantern.glow:abc",
        "lantern.glow:65536",
    ] {
        let mut req = request("GET", "/api", "");
        req.headers_mut().insert("host", host.parse().unwrap());
        assert_eq!(
            router.clone().oneshot(req).await.unwrap().status(),
            StatusCode::FORBIDDEN,
            "{host}"
        );
    }
    for origin in [
        "http://127.0.0.1:39400",
        "http://localhost:39400",
        "https://lantern.glow",
        "http://lantern.glow",
    ] {
        let mut req = request("GET", "/api", "");
        req.headers_mut().insert("origin", origin.parse().unwrap());
        req.headers_mut()
            .insert("sec-fetch-site", "same-origin".parse().unwrap());
        assert_eq!(
            router.clone().oneshot(req).await.unwrap().status(),
            StatusCode::OK,
            "{origin}"
        );
    }
    let mut req = request("GET", "/api", "");
    req.headers_mut()
        .append("host", "evil.example:39400".parse().unwrap());
    assert_eq!(
        router.clone().oneshot(req).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let mut req = request("GET", "/api", "");
    req.headers_mut().remove("host");
    assert_eq!(router.oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn api_sse_log_lag_requests_resync() {
    let (router, fake, _) = setup(Mode::Success, 16, 39400);
    let response = router
        .oneshot(request(
            "GET",
            "/api/events?state=false&logs=myapp%40feature-x&tail=0",
            "",
        ))
        .await
        .unwrap();
    let mut stream = response.into_body().into_data_stream();
    while !std::str::from_utf8(&stream.next().await.unwrap().unwrap())
        .unwrap()
        .contains("snapshot_complete")
    {}
    for n in 0..10 {
        fake.logs.send(line(&format!("line {n}"))).unwrap();
    }
    let chunk = stream.next().await.unwrap().unwrap();
    let text = std::str::from_utf8(&chunk).unwrap();
    assert!(text.contains("event: resync") && text.contains("lagged"));
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn api_wait_disconnect_does_not_cancel_the_operation() {
    let (router, fake, _) = setup(Mode::Running, 16, 39400);
    let waiting_router = router.clone();
    let waiter = tokio::spawn(async move {
        waiting_router
            .oneshot(request(
                "POST",
                "/api/instances/myapp%40feature-x/start?wait=true",
                "{}",
            ))
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while fake.pending.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    let handle = fake.pending.lock().unwrap().pop().unwrap();
    let id = handle.id().0.clone();
    assert!(!handle.cancel_token().is_cancelled());
    handle.finish(OpOutcome::Succeeded(None));
    let response = router
        .oneshot(request("GET", &format!("/api/operations/{id}"), ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["status"], "succeeded");
}

#[derive(Debug)]
struct ErrorFacade(&'static str);
#[async_trait]
impl DaemonFacade for ErrorFacade {
    async fn settings(&self) -> FacadeResult<Value> {
        Err(ApiError {
            code: self.0.into(),
            message: "failure from adapter".into(),
            details: json!({"field":"example"}),
        })
    }
    async fn service_action(
        &self,
        _: String,
        _: lantern_daemon::api::facade::ServiceAction,
    ) -> FacadeResult<lantern_core::api::ServiceStatus> {
        Err(ApiError {
            code: self.0.into(),
            message: "service failed".into(),
            details: json!({}),
        })
    }
}

#[tokio::test]
async fn api_every_error_code_mapping_and_service_override() {
    let cases = [
        ("bad_request", 400),
        ("invalid_json", 400),
        ("validation_failed", 422),
        ("not_found", 404),
        ("project_not_found", 404),
        ("instance_not_found", 404),
        ("service_not_found", 404),
        ("operation_not_found", 404),
        ("doc_not_found", 404),
        ("already_exists", 409),
        ("operation_in_progress", 409),
        ("cannot_remove_primary", 409),
        ("dirty_worktree", 409),
        ("port_unavailable", 409),
        ("builtin_template", 409),
        ("not_controllable", 409),
        ("reserved_id", 422),
        ("needs_config", 422),
        ("branch_invalid", 422),
        ("forbidden_host", 403),
        ("forbidden_origin", 403),
        ("forbidden", 403),
        ("unsupported_media_type", 415),
        ("payload_too_large", 413),
        ("method_not_allowed", 405),
        ("timeout", 504),
        ("unavailable", 503),
        ("operation_failed", 500),
        ("internal", 500),
        ("unknown_error", 500),
    ];
    for (code, status) in cases {
        let bus = EventBus::new();
        let ops = OperationTracker::new(bus.clone());
        let router = api::router(ApiState::new(
            Arc::new(ErrorFacade(code)),
            bus,
            ops,
            ApiConfig::new(39400, "glow", "test"),
        ));
        let res = router
            .clone()
            .oneshot(request("GET", "/api/settings", ""))
            .await
            .unwrap();
        assert_eq!(res.status(), status, "{code}");
        let body = json_body(res).await;
        assert_eq!(body["error"]["code"], code);
        assert_eq!(body["error"]["details"]["field"], "example");
        if code == "operation_failed" {
            let res = router
                .oneshot(request("POST", "/api/services/redis/start", ""))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
            assert_eq!(json_body(res).await["error"]["code"], "operation_failed");
        }
    }
}

#[derive(Debug)]
struct RouteFacade {
    ops: OperationTracker,
    calls: Mutex<Vec<String>>,
    targets: Mutex<Vec<lantern_core::api::CreateInstanceRequest>>,
}
impl RouteFacade {
    fn record(&self, text: String) {
        self.calls.lock().unwrap().push(text);
    }
}
#[async_trait]
impl DaemonFacade for RouteFacade {
    async fn projects(
        &self,
        filter: lantern_daemon::api::facade::ProjectFilter,
    ) -> FacadeResult<Vec<ProjectSummary>> {
        self.record(format!("projects:{:?}:{:?}", filter.enabled, filter.q));
        Ok(vec![summary()])
    }
    async fn register_project(
        &self,
        request: lantern_core::api::RegisterProjectRequest,
    ) -> FacadeResult<ProjectSummary> {
        self.record(format!("register:{}", request.path.display()));
        Ok(summary())
    }
    async fn run_branch(
        &self,
        id: lantern_core::ids::ProjectId,
        request: lantern_core::api::CreateInstanceRequest,
    ) -> FacadeResult<OperationTicket> {
        self.record(format!("run:{id}"));
        self.targets.lock().unwrap().push(request);
        let handle = self.ops.begin(OperationKind::CreateInstance, Some(inst().id));
        let id = handle.id().clone();
        handle.finish(OpOutcome::Succeeded(None));
        Ok(OperationTicket {
            id,
            instance: Some(inst()),
            created: Some(true),
            forgotten: None,
        })
    }
    async fn checkouts(&self, id: lantern_core::ids::ProjectId) -> FacadeResult<Vec<InstanceView>> {
        self.record(format!("checkouts:{id}"));
        Ok(vec![inst()])
    }
    async fn settings(&self) -> FacadeResult<Value> {
        Ok(json!({"api":{"port":39400}}))
    }
    async fn patch_settings(&self, patch: Value) -> FacadeResult<lantern_core::api::SettingsPatched> {
        self.record(format!("patch:{patch}"));
        Ok(lantern_core::api::SettingsPatched {
            settings: patch,
            restart_required: vec!["api.port".into()],
            overridden_by_env: vec![],
        })
    }
    async fn listeners(
        &self,
        filter: lantern_daemon::api::facade::ListenerFilter,
    ) -> FacadeResult<Vec<lantern_core::model::Listener>> {
        self.record(format!("listeners:{:?}", filter.port));
        Ok(vec![])
    }
    async fn services(&self) -> FacadeResult<Vec<lantern_core::api::ServiceStatus>> {
        self.record("services".into());
        Ok(vec![])
    }
    async fn profiles(&self) -> FacadeResult<Vec<Value>> {
        Ok(vec![json!({"name":"demo"})])
    }
    async fn templates(&self) -> FacadeResult<Vec<Value>> {
        Ok(vec![json!({"name":"web"})])
    }
    async fn suggestions(&self) -> FacadeResult<Vec<lantern_core::api::Suggestion>> {
        Ok(vec![])
    }
    async fn schema(&self, name: String) -> FacadeResult<Value> {
        Ok(json!({"name":name}))
    }
    async fn docs(&self, _: InstanceId) -> FacadeResult<Vec<lantern_core::api::DocEntry>> {
        Ok(vec![lantern_core::api::DocEntry {
            path: "docs/<guide>.html".into(),
            kind: "<script>".into(),
            source: lantern_core::api::IntelSource::Manual,
            exists: true,
            size: Some(8),
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
        self.record(format!("doc:{path}"));
        Ok(lantern_core::api::DocContent {
            path,
            content: "<script>danger()</script>".into(),
            offset: 0,
            total_chars: 25,
            truncated: false,
        })
    }
}

#[tokio::test]
async fn api_successful_route_groups_and_branch_request_shapes() {
    let bus = EventBus::new();
    let ops = OperationTracker::new(bus.clone());
    let facade = Arc::new(RouteFacade {
        ops: ops.clone(),
        calls: Mutex::new(vec![]),
        targets: Mutex::new(vec![]),
    });
    let router = api::router(ApiState::new(
        facade.clone(),
        bus,
        ops,
        ApiConfig::new(39400, "glow", "test"),
    ));
    let cases = [
        ("GET", "/api/projects?q=hello", "", 200, "items"),
        ("POST", "/api/projects", "{\"path\":\"/fixture\"}", 201, "id"),
        ("GET", "/api/projects/myapp/checkouts", "", 200, "items"),
        ("GET", "/api/settings", "", 200, "api"),
        (
            "PATCH",
            "/api/settings",
            "{\"api\":{\"port\":99}}",
            200,
            "restart_required",
        ),
        ("GET", "/api/listeners?port=42", "", 200, "items"),
        ("GET", "/api/services", "", 200, "items"),
        ("GET", "/api/profiles", "", 200, "items"),
        ("GET", "/api/templates", "", 200, "items"),
        ("GET", "/api/suggestions", "", 200, "items"),
        ("GET", "/api/schema/manifest", "", 200, "name"),
    ];
    for (method, path, payload, status, field) in cases {
        let res = router
            .clone()
            .oneshot(request(method, path, payload))
            .await
            .unwrap();
        assert_eq!(res.status(), status, "{method} {path}");
        assert_eq!(res.headers()["content-type"], "application/json; charset=utf-8");
        assert!(!json_body(res).await[field].is_null(), "{method} {path}");
    }
    for (payload, expected) in [
        (
            json!({"branch":"feature/x"}),
            lantern_core::api::RunTarget::Branch("feature/x".into()),
        ),
        (
            json!({"branch":"next","base":"main"}),
            lantern_core::api::RunTarget::NewBranch {
                name: "next".into(),
                base: Some("main".into()),
            },
        ),
        (
            json!({"target":{"pr":123},"start":false}),
            lantern_core::api::RunTarget::Pr(123),
        ),
    ] {
        let res = router
            .clone()
            .oneshot(request(
                "POST",
                "/api/projects/myapp/instances?wait=true",
                &payload.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(json_body(res).await["created"], true);
        assert_eq!(facade.targets.lock().unwrap().last().unwrap().target, expected);
    }
    let res = router
        .clone()
        .oneshot(request("GET", "/api/projects/myapp/checkouts?refresh=true", ""))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let calls = facade.calls.lock().unwrap();
    assert_eq!(
        calls.iter().filter(|call| call.starts_with("checkouts:")).count(),
        1
    );
    assert!(
        calls
            .iter()
            .any(|c| c == "projects:Some(\"true\"):Some(\"hello\")")
    );
    assert!(calls.iter().any(|c| c == "listeners:Some(42)"));
}

#[tokio::test]
async fn api_docs_are_inert_and_paths_are_checked() {
    let bus = EventBus::new();
    let ops = OperationTracker::new(bus.clone());
    let facade = Arc::new(RouteFacade {
        ops: ops.clone(),
        calls: Mutex::new(vec![]),
        targets: Mutex::new(vec![]),
    });
    let router = api::router(ApiState::new(
        facade.clone(),
        bus,
        ops,
        ApiConfig::new(39400, "glow", "test"),
    ));
    let response = router
        .clone()
        .oneshot(request("GET", "/myapp%40feature-x/docs", ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("sandbox")
    );
    let body = to_bytes(response.into_body(), 10_000).await.unwrap();
    let html = std::str::from_utf8(&body).unwrap();
    assert!(html.contains("&lt;script&gt;") && !html.contains("<script>"));
    let response = router
        .clone()
        .oneshot(request(
            "GET",
            "/myapp@feature-x/docs/docs%2F%3Cguide%3E.html",
            "",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/plain; charset=utf-8");
    assert!(response.headers().contains_key("content-security-policy"));
    for path in [
        "/api/instances/myapp/docs/%2e%2e%2fsecret",
        "/myapp/docs/%2e%2e%2fsecret",
        "/myapp/docs/a%5cb",
        "/myapp/docs/bad%zz",
    ] {
        let res = router.clone().oneshot(request("GET", path, "")).await.unwrap();
        assert!(res.status().is_client_error(), "{path}");
        assert!(json_body(res).await["error"]["code"].is_string());
    }
    assert_eq!(
        facade
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.starts_with("doc:"))
            .count(),
        1
    );
}

#[derive(Debug, Default)]
struct SnapshotRace {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait]
impl DaemonFacade for SnapshotRace {
    async fn event_snapshot(&self) -> FacadeResult<Vec<Event>> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(vec![])
    }
}

#[tokio::test]
async fn api_sse_subscription_precedes_snapshot() {
    let bus = EventBus::with_capacity(4);
    let ops = OperationTracker::new(bus.clone());
    let facade = Arc::new(SnapshotRace::default());
    let router = api::router(ApiState::new(
        facade.clone(),
        bus.clone(),
        ops,
        ApiConfig::new(39400, "glow", "test"),
    ));
    let request_task =
        tokio::spawn(async move { router.oneshot(request("GET", "/api/events", "")).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(1), facade.entered.notified())
        .await
        .unwrap();
    bus.publish(Event::ListenersChanged {
        seq: 77,
        listeners: vec![],
    });
    facade.release.notify_one();
    let response = request_task.await.unwrap();
    let mut stream = response.into_body().into_data_stream();
    let mut seen = String::new();
    for _ in 0..3 {
        let chunk = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        seen.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    let hello = seen.find("event: hello").unwrap();
    let snapshot = seen.find("event: snapshot_complete").unwrap();
    let live = seen.find("event: listeners_changed").unwrap();
    assert!(hello < snapshot && snapshot < live && seen.contains("\"seq\":77"));
}

#[derive(Debug)]
struct SlowFacade;
#[async_trait]
impl DaemonFacade for SlowFacade {
    async fn settings(&self) -> FacadeResult<Value> {
        std::future::pending().await
    }
}
#[tokio::test(start_paused = true)]
async fn api_module_deadline_and_query_validation() {
    let bus = EventBus::new();
    let ops = OperationTracker::new(bus.clone());
    let mut config = ApiConfig::new(39400, "glow", "test");
    config.module_timeout = Duration::from_millis(10);
    let router = api::router(ApiState::new(Arc::new(SlowFacade), bus, ops, config));
    let res = router
        .clone()
        .oneshot(request("GET", "/api/settings", ""))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(json_body(res).await["error"]["code"], "timeout");
    for path in [
        "/api/events?logs=myapp&tail=1001",
        "/api/events?logs=",
        "/api/events?state=maybe",
        "/api/operations/id?wait=true&timeout=601",
        "/api/instances/myapp/logs?tail=5001",
        "/api/projects?enabled=maybe",
    ] {
        let res = router.clone().oneshot(request("GET", path, "")).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{path}");
        assert_eq!(json_body(res).await["error"]["code"], "bad_request");
    }
}

#[tokio::test]
async fn api_wait_completes_after_worker_and_operation_reads_remain_resources() {
    let (router, fake, _) = setup(Mode::Running, 16, 39400);
    let waiting_router = router.clone();
    let waiter = tokio::spawn(async move {
        waiting_router
            .oneshot(request(
                "POST",
                "/api/instances/myapp@feature-x/start?wait=true&timeout=1",
                "{}",
            ))
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while fake.pending.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let handle = fake.pending.lock().unwrap().pop().unwrap();
    handle.finish(OpOutcome::Succeeded(None));
    let response = waiter.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["operation"]["status"], "succeeded");

    let (router, _, _) = setup(Mode::Failure, 16, 39400);
    let response = router
        .clone()
        .oneshot(request("POST", "/api/instances/myapp@feature-x/start", "{}"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let op = json_body(response).await;
    let id = op["operation"]["id"].as_str().unwrap();
    let response = router
        .oneshot(request("GET", &format!("/api/operations/{id}?wait=true"), ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["status"], "failed");
}

#[tokio::test(start_paused = true)]
async fn api_sse_without_logs_has_no_log_subscription() {
    let (router, fake, bus) = setup(Mode::Success, 4, 39400);
    let response = router
        .oneshot(request("GET", "/api/events?state=false", ""))
        .await
        .unwrap();
    let mut stream = response.into_body().into_data_stream();
    for expected in ["event: hello", "event: snapshot_complete"] {
        let chunk = stream.next().await.unwrap().unwrap();
        assert!(std::str::from_utf8(&chunk).unwrap().contains(expected));
    }
    assert!(fake.logs.send(line("must not be delivered")).is_err());
    bus.publish(Event::ListenersChanged {
        seq: 5,
        listeners: vec![],
    });
    tokio::time::advance(Duration::from_millis(50)).await;
    let chunk = stream.next().await.unwrap().unwrap();
    let text = std::str::from_utf8(&chunk).unwrap();
    assert!(text.contains(": ping") && !text.contains("event:"));
}

#[cfg(unix)]
#[derive(Debug)]
struct InvalidPathFacade;
#[cfg(unix)]
fn non_utf8_summary() -> ProjectSummary {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let mut value = summary();
    value.project.root = std::path::PathBuf::from(OsString::from_vec(vec![b'/', 0xff]));
    value
}
#[cfg(unix)]
#[async_trait]
impl DaemonFacade for InvalidPathFacade {
    async fn projects(
        &self,
        _: lantern_daemon::api::facade::ProjectFilter,
    ) -> FacadeResult<Vec<ProjectSummary>> {
        Ok(vec![non_utf8_summary()])
    }
    async fn event_snapshot(&self) -> FacadeResult<Vec<Event>> {
        Ok(vec![Event::ProjectUpdated {
            project: non_utf8_summary(),
        }])
    }
}
#[cfg(unix)]
#[tokio::test]
async fn api_serialization_failures_are_enveloped_or_end_live_stream() {
    let bus = EventBus::new();
    let ops = OperationTracker::new(bus.clone());
    let router = api::router(ApiState::new(
        Arc::new(InvalidPathFacade),
        bus,
        ops,
        ApiConfig::new(39400, "glow", "test"),
    ));
    let response = router
        .clone()
        .oneshot(request("GET", "/api/projects", ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_body(response).await["error"]["code"], "internal");
    let response = router.oneshot(request("GET", "/api/events", "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_body(response).await["error"]["code"], "internal");

    let (router, _, bus) = setup(Mode::Success, 16, 39400);
    let response = router.oneshot(request("GET", "/api/events", "")).await.unwrap();
    let mut stream = response.into_body().into_data_stream();
    loop {
        let chunk = stream.next().await.unwrap().unwrap();
        if std::str::from_utf8(&chunk).unwrap().contains("snapshot_complete") {
            break;
        }
    }
    bus.publish(Event::ProjectUpdated {
        project: non_utf8_summary(),
    });
    assert!(stream.next().await.unwrap().is_err());
}

#[derive(Debug)]
struct GroupFacade {
    ops: OperationTracker,
    fail: bool,
    pending: Mutex<Vec<OpHandle>>,
}
#[async_trait]
impl DaemonFacade for GroupFacade {
    async fn stop_all(&self, _: lantern_core::api::StopAllRequest) -> FacadeResult<Vec<OperationTicket>> {
        let first = self.ops.begin(OperationKind::Stop, Some(inst().id));
        let first_id = first.id().clone();
        if self.fail {
            first.finish(OpOutcome::Failed(ApiError {
                code: "port_unavailable".into(),
                message: "busy".into(),
                details: json!({"port":12}),
            }));
        } else {
            self.pending.lock().unwrap().push(first);
        }
        let second = self.ops.begin(OperationKind::Stop, Some(inst().id));
        let second_id = second.id().clone();
        self.pending.lock().unwrap().push(second);
        Ok(vec![first_id, second_id]
            .into_iter()
            .map(|id| OperationTicket {
                id,
                instance: None,
                created: None,
                forgotten: None,
            })
            .collect())
    }
    async fn activate_profile(&self, _: String) -> FacadeResult<Vec<OperationTicket>> {
        self.stop_all(lantern_core::api::StopAllRequest {
            include_services: false,
        })
        .await
    }
}

#[tokio::test(start_paused = true)]
async fn api_group_wait_uses_one_deadline_and_returns_failures() {
    for (path, fail) in [
        ("/api/system/stop-all", false),
        ("/api/profiles/demo/activate", false),
        ("/api/system/stop-all", true),
    ] {
        let bus = EventBus::new();
        let ops = OperationTracker::new(bus.clone());
        let facade = Arc::new(GroupFacade {
            ops: ops.clone(),
            fail,
            pending: Mutex::new(vec![]),
        });
        let router = api::router(ApiState::new(
            facade.clone(),
            bus,
            ops,
            ApiConfig::new(39400, "glow", "test"),
        ));
        let before = tokio::time::Instant::now();
        let response = router
            .oneshot(request("POST", &format!("{path}?wait=true&timeout=1"), "{}"))
            .await
            .unwrap();
        if fail {
            assert_eq!(response.status(), StatusCode::CONFLICT);
            let body = json_body(response).await;
            assert_eq!(body["error"]["code"], "port_unavailable");
            assert_eq!(body["error"]["details"]["port"], 12);
            assert!(body["error"]["details"]["operation_id"].is_string());
        } else {
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            assert_eq!(
                json_body(response).await["operations"].as_array().unwrap().len(),
                2
            );
            assert_eq!(
                tokio::time::Instant::now().duration_since(before),
                Duration::from_secs(1)
            );
        }
        for handle in facade.pending.lock().unwrap().drain(..) {
            handle.finish(OpOutcome::Succeeded(None));
        }
    }
}

#[derive(Debug)]
struct SlowViewFacade {
    ops: OperationTracker,
}
#[async_trait]
impl DaemonFacade for SlowViewFacade {
    async fn start(&self, id: InstanceId) -> FacadeResult<OperationTicket> {
        let handle = self.ops.begin(OperationKind::Start, Some(id));
        let id = handle.id().clone();
        handle.finish(OpOutcome::Succeeded(None));
        Ok(OperationTicket {
            id,
            instance: Some(inst()),
            created: None,
            forgotten: None,
        })
    }
    async fn instance(&self, _: InstanceId) -> FacadeResult<InstanceView> {
        std::future::pending().await
    }
}
#[tokio::test(start_paused = true)]
async fn api_wait_deadline_also_bounds_the_final_view_lookup() {
    let bus = EventBus::new();
    let ops = OperationTracker::new(bus.clone());
    let router = api::router(ApiState::new(
        Arc::new(SlowViewFacade { ops: ops.clone() }),
        bus,
        ops,
        ApiConfig::new(39400, "glow", "test"),
    ));
    let before = tokio::time::Instant::now();
    let response = router
        .oneshot(request(
            "POST",
            "/api/instances/myapp@feature-x/start?wait=true&timeout=1",
            "{}",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        tokio::time::Instant::now().duration_since(before),
        Duration::from_secs(1)
    );
    let body = json_body(response).await;
    assert_eq!(body["operation"]["status"], "succeeded");
    assert_eq!(body["instance"]["id"], "myapp@feature-x");
}
