use std::{net::SocketAddr, time::Duration};

use futures_util::StreamExt;
use lantern_client::{
    Client, ClientConfig, ClientError, EventsOptions, ProjectFilter, RunOptions, StreamEvent, Wait,
};
use lantern_core::{
    api::{OperationStatus, RunTarget, WireEvent},
    ids::{InstanceId, ProjectId},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

async fn listener() -> TcpListener {
    for port in 39300..=39399 {
        if let Ok(socket) = TcpListener::bind(("127.0.0.1", port)).await {
            return socket;
        }
    }
    panic!("no free test port in the api lane range");
}
async fn request(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut temp = [0; 1024];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut temp))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&temp[..n]);
        if let Some(end) = bytes.windows(4).position(|p| p == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&bytes[..end]);
            let len = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + len {
                return String::from_utf8(bytes).unwrap();
            }
        }
    }
}
async fn reply(socket: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // Small writes exercise incremental transport; the parser also has byte-at-a-time unit tests.
    for chunk in response.as_bytes().chunks(17) {
        socket.write_all(chunk).await.unwrap();
    }
    socket.shutdown().await.unwrap();
}
async fn server(
    status: &'static str,
    content_type: &'static str,
    body: String,
) -> (SocketAddr, JoinHandle<String>) {
    let socket = listener().await;
    let address = socket.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut conn, _) = socket.accept().await.unwrap();
        let request = request(&mut conn).await;
        reply(&mut conn, status, content_type, &body).await;
        request
    });
    (address, task)
}
fn client(address: SocketAddr) -> Client {
    Client::new(ClientConfig {
        base_url: Some(format!("http://{address}")),
    })
    .unwrap()
}
fn operation_response() -> Value {
    json!({"operation":{"id":"op_123","kind":"create_instance","instance":"myapp@feature-x","status":"running","noop":false,
        "step":null,"steps":[],"created_at":"2026-11-06T17:52:03.412Z","finished_at":null,"error":null,"result":null},
        "instance":null,"created":true})
}

#[tokio::test]
async fn json_create_wait_and_error_envelope() {
    let (addr, task) = server(
        "202 Accepted",
        "application/json",
        operation_response().to_string(),
    )
    .await;
    let project = ProjectId::parse("myapp").unwrap();
    let res = client(addr)
        .run_branch(
            &project,
            RunTarget::NewBranch {
                name: "feature/x".into(),
                base: Some("main".into()),
            },
            RunOptions::default(),
            Wait {
                wait: true,
                timeout: Some(Duration::from_secs(12)),
            },
        )
        .await
        .unwrap();
    assert_eq!(res.operation.status, OperationStatus::Running);
    let captured = task.await.unwrap();
    assert!(
        captured.starts_with("POST /api/projects/myapp/instances?wait=true&timeout=12 HTTP/1.1\r\n"),
        "{captured}"
    );
    let body: Value = serde_json::from_str(captured.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(
        body,
        json!({"target":{"new_branch":"feature/x","base":"main"},"fetch":true,"start":true,"pin":null})
    );

    let (addr, task) = server(
        "422 Unprocessable Entity",
        "application/json",
        json!({"error":{"code":"needs_config","message":"no command","details":{"operation_id":"op_123"}}})
            .to_string(),
    )
    .await;
    let instance = InstanceId::parse("myapp@feature-x").unwrap();
    let err = client(addr).start(&instance, Wait::default()).await.unwrap_err();
    match err {
        ClientError::Api { status, error } => {
            assert_eq!(status, 422);
            assert_eq!(error.code, "needs_config");
            assert_eq!(error.details["operation_id"], "op_123");
        }
        other => panic!("unexpected error: {other:?}"),
    }
    let captured = task.await.unwrap();
    assert!(
        captured.starts_with("POST /api/instances/myapp%40feature-x/start?wait=false HTTP/1.1"),
        "{captured}"
    );
}

#[tokio::test]
async fn list_query_and_version_rejection() {
    let (addr, task) = server("200 OK", "application/json", json!({"items":[]}).to_string()).await;
    let filter = ProjectFilter {
        q: Some("rust & web".into()),
        ..ProjectFilter::default()
    };
    assert!(client(addr).projects(filter).await.unwrap().is_empty());
    let captured = task.await.unwrap();
    assert!(
        captured.starts_with("GET /api/projects?q=rust+%26+web HTTP/1.1"),
        "{captured}"
    );
    let root = json!({"name":"lantern","version":"future","api_version":999,"mcp":"/mcp","links":{}});
    let (addr, task) = server("200 OK", "application/json", root.to_string()).await;
    assert!(matches!(
        client(addr).ping().await.unwrap_err(),
        ClientError::Protocol(_)
    ));
    task.await.unwrap();
}

#[tokio::test]
async fn sse_snapshot_multiline_ping_unknown_and_resync() {
    let body = concat!(
        "event: hello\r\ndata: {\"type\":\"hello\",\"api_version\":1,\"daemon_version\":\"2\",\"boot_id\":\"abc\",\"now\":\"2026-11-06T17:52:03.412Z\"}\r\n\r\n",
        ": ping\n\n",
        "event: listeners_changed\ndata: {\"type\":\"listeners_changed\",\ndata: \"seq\":2,\"listeners\":[]}\n\n",
        "event: future_kind\ndata: {\"type\":\"future_kind\",\"more\":true}\n\n",
        "event: log\ndata: {\"type\":\"log\",\"instance\":\"myapp\",\"stream\":\"stdout\",\"line\":\"hello\",\"ts\":\"2026-11-06T17:52:03.412Z\"}\n\n",
        "event: snapshot_complete\ndata: {\"type\":\"snapshot_complete\"}\n\n",
        "event: resync\ndata: {\"type\":\"resync\",\"reason\":\"lagged\",\"skipped\":3}\n\n",
        "event: project_removed\ndata: {\"type\":\"project_removed\",\"id\":\"myapp\"}\n\n"
    );
    let (addr, task) = server("200 OK", "text/event-stream", body.into()).await;
    let options = EventsOptions {
        logs: vec![InstanceId::parse("myapp").unwrap()],
        ..EventsOptions::default()
    };
    let events: Vec<_> = tokio::time::timeout(
        Duration::from_secs(5),
        client(addr).events_once(options).collect(),
    )
    .await
    .unwrap();
    assert!(matches!(events[0], StreamEvent::Connecting { attempt: 0 }));
    assert!(matches!(events[1], StreamEvent::Connected));
    assert!(matches!(events[2], StreamEvent::Event(WireEvent::Hello { .. })));
    assert!(matches!(
        events[3],
        StreamEvent::Event(WireEvent::ListenersChanged { seq: 2, .. })
    ));
    assert!(matches!(events[4], StreamEvent::Event(WireEvent::Log { .. })));
    assert!(matches!(
        events[5],
        StreamEvent::Event(WireEvent::SnapshotComplete)
    ));
    assert_eq!(
        events.len(),
        6,
        "resync and all following frames must end this stream"
    );
    let captured = task.await.unwrap();
    assert!(
        captured.starts_with("GET /api/events?state=true&tail=100&logs=myapp HTTP/1.1"),
        "{captured}"
    );
}

#[tokio::test]
async fn resync_reconnects_to_fresh_snapshot() {
    let socket = listener().await;
    let addr = socket.local_addr().unwrap();
    let task = tokio::spawn(async move {
        for body in [
            "event: resync\ndata: {\"type\":\"resync\",\"reason\":\"lagged\",\"skipped\":1}\n\n",
            "event: snapshot_complete\ndata: {\"type\":\"snapshot_complete\"}\n\n",
        ] {
            let (mut conn, _) = socket.accept().await.unwrap();
            let _ = request(&mut conn).await;
            reply(&mut conn, "200 OK", "text/event-stream", body).await;
        }
    });
    let mut stream = client(addr).events(EventsOptions::default());
    let events = tokio::time::timeout(Duration::from_secs(5), async {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            let done = matches!(event, StreamEvent::Event(WireEvent::SnapshotComplete));
            events.push(event);
            if done {
                break;
            }
        }
        events
    })
    .await
    .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Connected))
            .count(),
        2
    );
    assert!(!events.iter().any(|e| matches!(e, StreamEvent::Lost { .. })));
    task.await.unwrap();
}

#[tokio::test]
async fn log_snapshot_serializes_optional_query_filters() {
    use lantern_core::api::LogParams;
    let body = json!({"instance":"myapp","lines":[],"buffered":0,"truncated":false});
    let (addr, task) = server("200 OK", "application/json", body.to_string()).await;
    let params = LogParams {
        tail: Some(25),
        since: None,
        stream: None,
        grep: Some("hello world".into()),
        format: None,
    };
    let page = client(addr)
        .logs(&InstanceId::parse("myapp").unwrap(), params)
        .await
        .unwrap();
    assert!(page.lines.is_empty());
    let captured = task.await.unwrap();
    assert!(
        captured.starts_with("GET /api/instances/myapp/logs?tail=25&grep=hello+world HTTP/1.1"),
        "{captured}"
    );
}

#[tokio::test]
async fn newer_sse_version_ends_even_with_reconnect_enabled() {
    let body = "event: hello\ndata: {\"type\":\"hello\",\"api_version\":999,\"daemon_version\":\"future\",\"boot_id\":\"id\",\"now\":\"2026-11-06T17:52:03.412Z\"}\n\n";
    let (addr, task) = server("200 OK", "text/event-stream", body.into()).await;
    let events: Vec<_> = tokio::time::timeout(
        Duration::from_secs(5),
        client(addr).events(EventsOptions::default()).collect(),
    )
    .await
    .unwrap();
    assert!(matches!(events.last(), Some(StreamEvent::Lost { retry_in, .. }) if retry_in.is_zero()));
    assert_eq!(events.len(), 3);
    task.await.unwrap();
}

#[tokio::test]
async fn rejects_dot_segments_before_making_requests() {
    let c = Client::new(ClientConfig {
        base_url: Some("http://127.0.0.1:39300".into()),
    })
    .unwrap();
    assert!(matches!(
        c.delete_profile("..").await,
        Err(ClientError::Protocol(_))
    ));
    assert!(matches!(c.schema(".").await, Err(ClientError::Protocol(_))));
}
