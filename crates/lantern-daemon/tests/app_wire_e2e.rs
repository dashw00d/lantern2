use futures_util::FutureExt;
use lantern_core::{ids::InstanceId, settings::Settings};
use lantern_daemon::{
    app::{DaemonArgs, run_until},
    runtime::systemd::UnitNames,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    net::{Ipv4Addr, TcpListener, UdpSocket},
    panic::AssertUnwindSafe,
    path::Path,
    process::Command,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

struct UnitGuard {
    unit: String,
    shutdown: CancellationToken,
}
impl UnitGuard {
    fn cleanup(&self) {
        self.shutdown.cancel();
        for action in ["stop", "reset-failed"] {
            let _ = Command::new("systemctl")
                .args(["--user", action, "--", &self.unit])
                .output();
        }
    }
}
impl Drop for UnitGuard {
    fn drop(&mut self) {
        self.cleanup();
    }
}
fn free_tcp(start: u16, end: u16) -> u16 {
    (start..=end)
        .find(|p| TcpListener::bind((Ipv4Addr::LOCALHOST, *p)).is_ok())
        .expect("free test TCP port")
}
fn free_dns(start: u16, end: u16) -> u16 {
    (start..=end)
        .find(|p| {
            UdpSocket::bind((Ipv4Addr::LOCALHOST, *p)).is_ok()
                && TcpListener::bind((Ipv4Addr::LOCALHOST, *p)).is_ok()
        })
        .expect("free test DNS port")
}
async fn rpc(client: &reqwest::Client, base: &str, method: &str, params: Value) -> Value {
    let response = client
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-11-25")
        .json(&json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(status.is_success(), "MCP HTTP {status}: {body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert!(value.get("result").is_some(), "MCP {method}: {value}");
    value
}
async fn tool(client: &reqwest::Client, base: &str, name: &str, arguments: Value) -> Value {
    let response = rpc(
        client,
        base,
        "tools/call",
        json!({"name":name,"arguments":arguments}),
    )
    .await;
    assert_ne!(response["result"]["isError"], true, "{name}: {response}");
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}
async fn wait_operation(client: &reqwest::Client, base: &str, id: &str) -> Value {
    let op = tool(
        client,
        base,
        "get_operation",
        json!({"operation_id":id,"wait_seconds":30}),
    )
    .await;
    assert_eq!(op["status"], "succeeded", "operation {id}: {op}");
    op
}
async fn scenario(client: &reqwest::Client, base: &str, dns_port: u16, root: &Path, https_port: u16) {
    for _ in 0..100 {
        if client
            .get(format!("{base}/api/system/info"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut dns = vec![0x12, 0x34, 0x01, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in ["x", "myapp", "glow"] {
        dns.push(label.len() as u8);
        dns.extend_from_slice(label.as_bytes());
    }
    dns.extend_from_slice(&[0, 0, 1, 0, 1]);
    let mut socket = None;
    for port in 39700..=39729 {
        if let Ok(s) = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).await {
            socket = Some(s);
            break;
        }
    }
    let socket = socket.expect("free test UDP client port");
    let mut answer = [0u8; 512];
    let mut received = None;
    for _ in 0..20 {
        socket
            .send_to(&dns, (Ipv4Addr::LOCALHOST, dns_port))
            .await
            .unwrap();
        if let Ok(Ok((n, _))) =
            tokio::time::timeout(Duration::from_millis(150), socket.recv_from(&mut answer)).await
        {
            received = Some(n);
            break;
        }
    }
    let n = received.expect("DNS response");
    assert_eq!(&answer[6..8], &[0, 1]);
    assert_eq!(&answer[n - 4..n], &[127, 0, 0, 1]);

    let response = client
        .post(format!("{base}/api/projects"))
        .json(&json!({"path":root}))
        .send()
        .await
        .unwrap();
    let code = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(code, 201, "register: {body}");
    rpc(client, base, "initialize", json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"wire-e2e","version":"1"}})).await;
    let list = tool(client, base, "list_projects", json!({})).await;
    assert!(list.as_array().unwrap().iter().any(|p| p["id"] == "myapp"));
    let denied = client
        .post(format!("{base}/mcp"))
        .header("host", "evil.test")
        .header("accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
    let start = tool(
        client,
        base,
        "start_project",
        json!({"name":"myapp", "wait_seconds":0}),
    )
    .await;
    wait_operation(client, base, start["operation_id"].as_str().unwrap()).await;
    let running: Value = client
        .get(format!("{base}/api/instances/myapp"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(running["runtime"]["status"], "running", "{running}");
    let project_port = running["runtime"]["port"].as_u64().unwrap();
    assert!((39670..=39699).contains(&project_port));
    let health: Value = client
        .get(format!("{base}/api/system/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let checks = health["checks"].as_array().unwrap();
    assert!(
        checks
            .iter()
            .any(|c| c["id"] == "caddy_missing" && c["fix"].is_object()),
        "{health}"
    );
    assert!(!checks.iter().any(|c| c["status"] == "fail"), "{health}");
    let status = client
        .get(format!("{base}/__lantern/status"))
        .header("x-forwarded-host", format!("myapp.glow:{https_port}"))
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), 200);
    let gateway: Value = status.json().await.unwrap();
    assert_eq!(gateway["status"], "running");
    assert_eq!(gateway["host"], "myapp.glow");
    let refresh = client
        .post(format!("{base}/api/instances/myapp/intel/refresh"))
        .send()
        .await
        .unwrap();
    assert!(refresh.status().is_success(), "{}", refresh.text().await.unwrap());
    let doc = client
        .get(format!("{base}/api/instances/myapp/docs/README.md"))
        .send()
        .await
        .unwrap();
    assert!(doc.status().is_success());
    assert_eq!(doc.text().await.unwrap(), "Wire acceptance documentation\n");
    let stop = tool(
        client,
        base,
        "stop_project",
        json!({"name":"myapp", "wait_seconds":0}),
    )
    .await;
    wait_operation(client, base, stop["operation_id"].as_str().unwrap()).await;
    let stopped: Value = client
        .get(format!("{base}/api/instances/myapp"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stopped["runtime"]["status"], "stopped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real system: isolated transient wire unit, loopback DNS and HTTP"]
async fn app_wire_real_system_dns_mcp_gateway_intel_no_caddy() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(temp.path().join("cache")).unwrap();
    fs::write(
        temp.path().join("cache/shell-env.sh"),
        "export PATH='/usr/bin:/bin'\n",
    )
    .unwrap();
    fs::write(root.join("lantern.yaml"), "id: myapp\ntype: proxy\nrun: {cmd: 'python3 -m http.server ${PORT} --bind 127.0.0.1'}\ndocs: [README.md]\n").unwrap();
    fs::write(root.join("README.md"), "Wire acceptance documentation\n").unwrap();
    let id = InstanceId::parse("myapp").unwrap();
    let nonce = std::process::id();
    let names = UnitNames {
        instance_prefix: format!("lantern-test-wire-{nonce}-inst-"),
        operation_prefix: format!("lantern-test-wire-{nonce}-op-"),
        slice: "app-lantern-test.slice".into(),
    };
    let shutdown = CancellationToken::new();
    let guard = UnitGuard {
        unit: names.instance(&id),
        shutdown: shutdown.clone(),
    };
    let mut env = BTreeMap::new();
    for (key, path) in [
        ("HOME", temp.path().to_path_buf()),
        ("LANTERN_CONFIG_DIR", temp.path().join("config")),
        ("LANTERN_STATE_DIR", temp.path().join("state")),
        ("LANTERN_DATA_DIR", temp.path().join("data")),
        ("LANTERN_CACHE_DIR", temp.path().join("cache")),
        ("LANTERN_RUNTIME_DIR", temp.path().join("runtime")),
    ] {
        env.insert(key.into(), path.to_string_lossy().into_owned());
    }
    env.insert("PATH".into(), String::new());
    let port = free_tcp(39600, 39619);
    let dns_port = free_dns(39630, 39649);
    let mut settings = Settings::default();
    settings.daemon.port = port;
    settings.dns.listen = format!("127.0.0.1:{dns_port}");
    settings.proxy.http_port = 39760;
    settings.proxy.https_port = 39761;
    settings.ports.range_start = 39670;
    settings.ports.range_end = 39699;
    settings.workspaces.roots.clear();
    settings.runtime.startup_timeout_secs = 15;
    settings.runtime.stop_timeout_secs = 3;
    settings.health.interval_secs = 60;
    let args = DaemonArgs {
        dev: false,
        environment: Some(env),
        settings: Some(settings),
        unit_names: Some(names),
        restrict_journal_to_units: true,
    };
    let mut task = tokio::spawn(run_until(args, shutdown.clone()));
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(40))
        .build()
        .unwrap();
    let outcome = AssertUnwindSafe(scenario(&client, &base, dns_port, &root, 39761))
        .catch_unwind()
        .await;
    shutdown.cancel();
    let daemon = tokio::time::timeout(Duration::from_secs(20), &mut task).await;
    if daemon.is_err() {
        task.abort();
        let _ = task.await;
    }
    guard.cleanup();
    let state = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--property=LoadState",
            "--value",
            "--",
            &guard.unit,
        ])
        .output()
        .unwrap();
    assert_ne!(
        String::from_utf8_lossy(&state.stdout).trim(),
        "loaded",
        "test unit still loaded"
    );
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    daemon.expect("daemon shutdown timed out").unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real system: isolated daemon and occupied test DNS socket"]
async fn app_wire_real_system_dns_bind_failure_is_nonfatal() {
    let temp = tempfile::tempdir().unwrap();
    let dns_port = free_dns(39650, 39659);
    let _occupied = UdpSocket::bind((Ipv4Addr::LOCALHOST, dns_port)).unwrap();
    let api_port = free_tcp(39620, 39629);
    let mut env = BTreeMap::new();
    for (key, path) in [
        ("HOME", temp.path().to_path_buf()),
        ("LANTERN_CONFIG_DIR", temp.path().join("config")),
        ("LANTERN_STATE_DIR", temp.path().join("state")),
        ("LANTERN_DATA_DIR", temp.path().join("data")),
        ("LANTERN_CACHE_DIR", temp.path().join("cache")),
        ("LANTERN_RUNTIME_DIR", temp.path().join("runtime")),
    ] {
        env.insert(key.into(), path.to_string_lossy().into_owned());
    }
    env.insert("PATH".into(), String::new());
    let mut settings = Settings::default();
    settings.daemon.port = api_port;
    settings.dns.listen = format!("127.0.0.1:{dns_port}");
    settings.proxy.http_port = 39760;
    settings.proxy.https_port = 39761;
    settings.workspaces.roots.clear();
    let names = UnitNames {
        instance_prefix: format!("lantern-test-wire-{}-busy-inst-", std::process::id()),
        operation_prefix: format!("lantern-test-wire-{}-busy-op-", std::process::id()),
        slice: "app-lantern-test.slice".into(),
    };
    let shutdown = CancellationToken::new();
    let mut daemon = tokio::spawn(run_until(
        DaemonArgs {
            dev: false,
            environment: Some(env),
            settings: Some(settings),
            unit_names: Some(names),
            restrict_journal_to_units: true,
        },
        shutdown.clone(),
    ));
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let url = format!("http://127.0.0.1:{api_port}/api/system/health");
    let mut health = None;
    for _ in 0..100 {
        if let Ok(response) = client.get(&url).send().await
            && let Ok(value) = response.json::<Value>().await
        {
            health = Some(value);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    shutdown.cancel();
    let done = tokio::time::timeout(Duration::from_secs(15), &mut daemon).await;
    if done.is_err() {
        daemon.abort();
        let _ = daemon.await;
    }
    done.expect("daemon did not shut down").unwrap().unwrap();
    let health = health.expect("daemon continued to serve HTTP");
    assert!(
        health["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["id"] == "dns.responder" && check["status"] == "fail"),
        "{health}"
    );
}
